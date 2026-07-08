use std::collections::HashMap;

use agent_session::{SessionAction, SessionEvent};
use anyhow::{anyhow, Context, Result};
use serde_json::json;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use crate::{
    ActionKind, ActionStatus, ActionUpdate, EventFrame, EventType, OutputBatch, PersistedAction,
    QueuedInputContent,
};

use super::action_records::{
    action_event_matches_row, action_payload, ActionKey, POST_COMPACTION_DISPATCH_KEY,
};
use super::events::{insert_event_tx, insert_session_event_tx};
use super::queue::{bump_revisions_tx, queue_event_payload, queue_state_tx};
use super::rows::row_text;
use super::sql::{action_is_unfinished, lock_session_tx, QUEUED_INPUT_DISPATCH_ORDER};
use super::transcript::{insert_entry_tx, session_state_for_event_tx};
use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn persist_outputs(
        &self,
        session_id: &str,
        batch: OutputBatch<'_>,
    ) -> Result<(Vec<EventFrame>, Vec<PersistedAction>)> {
        if !batch.has_durable_obligations() {
            return Ok((Vec::new(), Vec::new()));
        }
        let mut tx = self.pool.begin().await?;
        let (frames, dispatch) = persist_outputs_tx(&mut tx, session_id, batch).await?;
        tx.commit().await?;
        Ok((frames, dispatch))
    }
}

pub(super) async fn persist_outputs_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    batch: OutputBatch<'_>,
) -> Result<(Vec<EventFrame>, Vec<PersistedAction>)> {
    let OutputBatch {
        entries,
        active_leaf_id,
        active_leaf_unchanged: _,
        session_events,
        actions,
        action_update,
        consumed_input,
        accepted_input,
        provider_replay_attached: _,
        control_interrupt_input_id,
    } = batch;
    let had_action_update = action_update.is_some();
    let had_accepted_input = accepted_input.is_some();
    let had_actions = !actions.is_empty();
    let had_session_events = !session_events.is_empty();
    let transcript_changed = !entries.is_empty();
    let consumed_input_event = consumed_input.as_ref().map(|input| {
        json!({
            "input_id": input.id,
            "priority": input.priority,
            "client_input_id": input.client_input_id,
        })
    });

    lock_session_tx(tx, session_id).await?;
    let session_row = sqlx::query(
        r#"
            select active_leaf_id,
                exists(select 1 from transcript_entries where session_id=$1) as has_transcript_entries
            from sessions
            where id=$1
            "#,
    )
    .bind(session_id)
    .fetch_one(&mut **tx)
    .await?;
    let current_active_leaf_id = session_row.get::<Option<String>, _>("active_leaf_id");
    let has_transcript_entries = session_row.get::<bool, _>("has_transcript_entries");
    let active_leaf_changed = current_active_leaf_id.as_deref() != active_leaf_id;
    if let Some(first_entry) = entries.first() {
        if has_transcript_entries
            && current_active_leaf_id.as_deref() != first_entry.parent_id.as_deref()
        {
            return Err(anyhow!(
                "session active leaf changed while applying outputs: expected {:?}, found {:?}",
                first_entry.parent_id,
                current_active_leaf_id
            ));
        }
    }

    let mut entry_records_by_id = HashMap::new();
    for entry in entries {
        if let Some(record) = insert_entry_tx(tx, session_id, entry)
            .await
            .with_context(|| format!("insert transcript entry {}", entry.id))?
        {
            entry_records_by_id.insert(record.id.clone(), record);
        }
    }
    sqlx::query("update sessions set active_leaf_id=$2::text, updated_at=now() where id=$1")
        .bind(session_id)
        .bind(active_leaf_id)
        .execute(&mut **tx)
        .await
        .context("update session active leaf")?;

    let mut frames = Vec::new();
    if let Some(input) = consumed_input {
        let consume_query = format!(
            r#"
                update queued_inputs
                set status='consumed',
                    follow_up_position=null,
                    updated_at=now(),
                    origin=coalesce(origin, '{{}}'::jsonb)
                        || jsonb_build_object('consumed_at', now()::text)
                where id=$1
                    and session_id=$2::text
                    and (
                        (
                            status='queued'
                            and xmin::text=$4::text
                            and id=(
                                select id
                                from queued_inputs
                                where session_id=$2::text
                                  and status='queued'
                                  and not (
                                      coalesce((origin->>'control_interrupt')::boolean, false)
                                      and coalesce(origin->>'control_phase', 'pending_interrupt') <> 'ready'
                                  )
                                  and not exists (
                                      select 1
                                      from queued_inputs blocked
                                      where blocked.session_id=$2::text
                                        and blocked.status in ('queued', 'consuming')
                                        and coalesce((blocked.origin->>'control_interrupt')::boolean, false)
                                        and blocked.origin->>'control_phase' in ('pending_interrupt', 'interrupt_applied')
                                  )
                                order by {QUEUED_INPUT_DISPATCH_ORDER}
                                limit 1
                            )
                        )
                        or (
                            status='consuming'
                            and origin->>'claim_id'=$3
                        )
                    )
                "#,
        );
        let updated = sqlx::query(&consume_query)
            .bind(&input.id)
            .bind(session_id)
            .bind(&input.claim_id)
            .bind(&input.row_version)
            .execute(&mut **tx)
            .await
            .context("mark queued input consumed")?
            .rows_affected();
        if updated != 1 {
            return Err(anyhow!("queued input was already consumed: {}", input.id));
        }
    }

    let mut accepted_input_event = None;
    if let Some(input) = accepted_input {
        let mut input_id = None;
        if let Some(client_input_id) = input.client_input_id.as_deref() {
            let id = format!("input_{}", Uuid::new_v4());
            let inserted = sqlx::query(
                r#"
                    insert into queued_inputs (id, session_id, priority, content, status, client_input_id)
                    values ($1, $2, $3, $4, 'consumed', $5)
                    on conflict (session_id, client_input_id) where client_input_id is not null
                    do nothing
                    returning id
                    "#,
            )
            .bind(&id)
            .bind(session_id)
            .bind(input.priority.as_str())
            .bind(serde_json::to_value(QueuedInputContent::user_message(
                input.content.clone(),
            ))?)
            .bind(client_input_id)
            .fetch_optional(&mut **tx)
            .await
            .context("record accepted input")?;
            let Some(row) = inserted else {
                return Err(anyhow!("input already recorded: {client_input_id}"));
            };
            input_id = Some(row.get::<String, _>("id"));
        }

        accepted_input_event = Some(json!({
            "input_id": input_id,
            "priority": input.priority,
            "client_input_id": input.client_input_id,
            "content": input.content.content,
            "content_type": "user_message",
        }));
    }

    if let Some(mut update) = action_update {
        complete_action_tx(tx, session_id, &mut update, session_events).await?;
    }

    if let Some(input_id) = control_interrupt_input_id {
        let unfinished_actions = action_is_unfinished(Some("a"));
        let mark_interrupt_applied = format!(
            r#"
            update queued_inputs
            set origin=origin || jsonb_build_object(
                    'control_phase', 'interrupt_applied',
                    'control_interrupted', true,
                    'control_interrupt_outcome', 'interrupted',
                    'control_interrupt_applied_at', clock_timestamp()::text
                ),
                updated_at=now()
            where session_id=$1
              and id=$2
              and status='queued'
              and priority='steer'
              and origin->>'control_kind' in (
                  'scoped_subagent_steer',
                  'scoped_subagent_interrupt'
              )
              and coalesce((origin->>'control_interrupt')::boolean, false)
              and origin->>'control_phase'='pending_interrupt'
              and (
                  (origin->>'target_active_leaf_id') is not distinct from $3::text
                  or (
                      jsonb_array_length(
                          coalesce(
                              origin->'target_action_attempt_ids',
                              case
                                  when origin->>'target_action_attempt_id' is not null
                                  then jsonb_build_array(origin->>'target_action_attempt_id')
                                  else '[]'::jsonb
                              end
                          )
                      ) > 0
                      and exists (
                          select 1
                          from actions captured_current
                          where captured_current.session_id=$1
                            and captured_current.status in ('pending', 'blocked', 'running')
                            and captured_current.turn_id is not distinct from
                                nullif(origin->>'target_turn_id', '')::bigint
                            and exists (
                                select 1
                                from jsonb_array_elements_text(
                                    coalesce(
                                        origin->'target_action_attempt_ids',
                                        case
                                            when origin->>'target_action_attempt_id' is not null
                                            then jsonb_build_array(origin->>'target_action_attempt_id')
                                            else '[]'::jsonb
                                        end
                                    )
                                ) captured(attempt_id)
                                where captured.attempt_id=captured_current.attempt_id
                            )
                      )
                  )
              )
              and exists (
                  select 1
                  from transcript_entries boundary
                  where boundary.session_id=$1
                    and boundary.id=$4
                    and boundary.item->>'type'='turn_finished'
                    and boundary.item->>'outcome'='Interrupted'
              )
              and not exists (
                  select 1
                  from actions a
                  where a.session_id=$1
                    and {unfinished_actions}
                    and (
                        a.turn_id is distinct from
                            nullif(origin->>'target_turn_id', '')::bigint
                        or not exists (
                            select 1
                            from jsonb_array_elements_text(
                                coalesce(
                                    origin->'target_action_attempt_ids',
                                    case
                                        when origin->>'target_action_attempt_id' is not null
                                        then jsonb_build_array(origin->>'target_action_attempt_id')
                                        else '[]'::jsonb
                                    end
                                )
                            ) captured(attempt_id)
                            where captured.attempt_id=a.attempt_id
                        )
                    )
              )
            "#
        );
        let updated = sqlx::query(&mark_interrupt_applied)
            .bind(session_id)
            .bind(input_id)
            .bind(current_active_leaf_id.as_deref())
            .bind(active_leaf_id)
            .execute(&mut **tx)
            .await?
            .rows_affected();
        if updated != 1 {
            return Err(anyhow!(
                "subagent control interrupt phase update affected {updated} rows for {input_id}"
            ));
        }
        let interrupt_target_action = format!(
            r#"
            update actions a
            set status='interrupted',
                result=jsonb_build_object(
                    'reason', 'combined subagent control',
                    'control_input_id', $2::text
                ),
                updated_at=now()
            from queued_inputs q
            where a.session_id=$1
              and q.session_id=$1
              and q.id=$2
              and exists (
                  select 1
                  from jsonb_array_elements_text(
                      coalesce(
                          q.origin->'target_action_attempt_ids',
                          case
                              when q.origin->>'target_action_attempt_id' is not null
                              then jsonb_build_array(q.origin->>'target_action_attempt_id')
                              else '[]'::jsonb
                          end
                      )
                  ) captured(attempt_id)
                  where captured.attempt_id=a.attempt_id
              )
              and a.turn_id is not distinct from
                  nullif(q.origin->>'target_turn_id', '')::bigint
              and {unfinished_actions}
            "#
        );
        sqlx::query(&interrupt_target_action)
            .bind(session_id)
            .bind(input_id)
            .execute(&mut **tx)
            .await?;
    }

    let mut action_rows = HashMap::<ActionKey, String>::new();
    let mut dispatch = Vec::new();
    for action in actions {
        if matches!(action, SessionAction::CancelSessionWork) {
            let unfinished_actions = action_is_unfinished(None);
            let query = format!(
                r#"
                update actions
                set status='interrupted',
                    result='{{"reason":"session interrupted"}}'::jsonb,
                    payload=payload - '{POST_COMPACTION_DISPATCH_KEY}',
                    updated_at=now()
                where session_id=$1 and {unfinished_actions}
                "#
            );
            sqlx::query(&query)
                .bind(session_id)
                .execute(&mut **tx)
                .await
                .context("mark session work interrupted")?;
            continue;
        }

        let (kind, action_id, turn_id, payload) = action_payload(action)?;
        let row_id = format!("action_{}", Uuid::new_v4());
        let attempt_id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            insert into actions (id, session_id, turn_id, action_id, attempt_id, kind, status, payload)
            values ($1::text, $2::text, $3::bigint, $4, $5::text, $6::text, $7::text, $8)
            "#,
        )
        .bind(&row_id)
        .bind(session_id)
        .bind(turn_id)
        .bind(action_id)
        .bind(&attempt_id)
        .bind(kind.as_str())
        .bind(initial_action_status(kind).as_str())
        .bind(&payload)
        .execute(&mut **tx)
        .await
        .context("insert action row")?;
        action_rows.insert(ActionKey::new(kind, action_id), row_id.clone());
        dispatch.push(PersistedAction {
            row_id,
            attempt_id,
            action: action.clone(),
        });
    }

    let queue_changed = consumed_input_event.is_some();
    let session_changed = queue_changed
        || transcript_changed
        || active_leaf_changed
        || had_accepted_input
        || had_action_update
        || had_actions
        || had_session_events
        || control_interrupt_input_id.is_some();
    if session_changed {
        bump_revisions_tx(tx, session_id, queue_changed, transcript_changed).await?;
    }
    if consumed_input_event.is_some() || accepted_input_event.is_some() {
        let queue = queue_state_tx(tx, session_id).await?;
        if let Some(payload) = consumed_input_event {
            frames.push(
                insert_event_tx(
                    tx,
                    session_id,
                    EventType::InputConsumed,
                    queue_event_payload(&queue, payload),
                )
                .await
                .context("insert input.consumed event")?,
            );
        }
        if let Some(payload) = accepted_input_event {
            frames.push(
                insert_event_tx(
                    tx,
                    session_id,
                    EventType::InputAccepted,
                    queue_event_payload(&queue, payload),
                )
                .await
                .context("insert input.accepted event")?,
            );
        }
    }
    // This is the daemon progress hot path: after persisting output entries, do
    // not re-query each just-inserted transcript row while emitting websocket
    // events. Use the INSERT ... RETURNING records collected above, and read the
    // revision/head state once after the revision bump. The event helper keeps a
    // fallback lookup for rare idempotent-conflict or recovery/compaction paths.
    let has_transcript_events = session_events
        .iter()
        .any(|event| matches!(event, SessionEvent::TranscriptItemAppended { .. }));
    let event_state = if has_transcript_events {
        Some(session_state_for_event_tx(tx, session_id).await?)
    } else {
        None
    };
    for event in session_events {
        frames.extend(
            insert_session_event_tx(
                tx,
                session_id,
                event,
                event_state.as_ref(),
                &entry_records_by_id,
                &action_rows,
            )
            .await
            .with_context(|| format!("insert session event {event:?}"))?,
        );
    }
    Ok((frames, dispatch))
}

fn initial_action_status(kind: ActionKind) -> ActionStatus {
    match kind {
        ActionKind::Model | ActionKind::Tool => ActionStatus::Pending,
        ActionKind::Compaction => ActionStatus::Running,
    }
}

async fn complete_action_tx(
    tx: &mut Transaction<'_, Postgres>,
    session_id: &str,
    update: &mut ActionUpdate,
    session_events: &[SessionEvent],
) -> Result<()> {
    let explicit_status = update.status;
    let explicit_result = update.result.clone();
    let lease_owner = update
        .post_compaction_dispatch_lease
        .as_ref()
        .map(|lease| lease.owner_id.as_str());
    let lease_generation = update
        .post_compaction_dispatch_lease
        .as_ref()
        .map(|lease| lease.generation.to_string());
    let lease_context = update
        .post_compaction_dispatch_lease
        .as_ref()
        .map(|lease| lease.context_leaf_id.as_str());
    let select_query = format!(
        r#"
            select kind, action_id
            from actions
            where session_id=$1
                and id=$2::text
                and attempt_id=$3::text
                and status in ('pending','running')
                and (
                    (
                        $4::text is null
                        and not (payload ? '{POST_COMPACTION_DISPATCH_KEY}')
                    )
                    or (
                        payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'action_row_id'=$2
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'attempt_id'=$3
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'context_leaf_id'=$6
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'owner_id'=$4
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'generation'=$5
                        and (payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'expires_at_ms')::bigint
                            > (extract(epoch from clock_timestamp()) * 1000)::bigint
                    )
                )
            "#
    );
    if let Some(row) = sqlx::query(&select_query)
        .bind(session_id)
        .bind(&update.row_id)
        .bind(&update.attempt_id)
        .bind(lease_owner)
        .bind(&lease_generation)
        .bind(lease_context)
        .fetch_optional(&mut **tx)
        .await
        .context("load action row for completion")?
    {
        let row_kind = row_text(&row, "kind")?;
        let row_action_id: i64 = row.get("action_id");
        if !matches!(explicit_status, ActionStatus::Error) {
            for event in session_events {
                match event {
                    SessionEvent::ActionCompleted { kind, id }
                        if action_event_matches_row(row_kind, row_action_id, kind, id) =>
                    {
                        update.status = ActionStatus::Completed;
                    }
                    SessionEvent::ActionFailed { kind, id, error }
                        if action_event_matches_row(row_kind, row_action_id, kind, id) =>
                    {
                        update.status = ActionStatus::Error;
                        update.result = json!({ "error": error });
                    }
                    _ => {}
                }
            }
        } else {
            update.status = explicit_status;
            update.result = explicit_result;
        }
    }

    let update_query = format!(
        r#"
            update actions
            set status=$4,
                result=$5,
                payload=payload - '{POST_COMPACTION_DISPATCH_KEY}',
                updated_at=now()
            where session_id=$1
                and id=$2::text
                and attempt_id=$3::text
                and status in ('pending','running')
                and (
                    (
                        $6::text is null
                        and not (payload ? '{POST_COMPACTION_DISPATCH_KEY}')
                    )
                    or (
                        payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'action_row_id'=$2
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'attempt_id'=$3
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->>'context_leaf_id'=$8
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'owner_id'=$6
                        and payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'generation'=$7
                        and (payload->'{POST_COMPACTION_DISPATCH_KEY}'->'lease'->>'expires_at_ms')::bigint
                            > (extract(epoch from clock_timestamp()) * 1000)::bigint
                    )
                )
            "#
    );
    let updated = sqlx::query(&update_query)
        .bind(session_id)
        .bind(&update.row_id)
        .bind(&update.attempt_id)
        .bind(update.status.as_str())
        .bind(&update.result)
        .bind(lease_owner)
        .bind(lease_generation)
        .bind(lease_context)
        .execute(&mut **tx)
        .await
        .context("update completed action row")?
        .rows_affected();
    if updated != 1 {
        return Err(anyhow!(
            "action attempt is no longer running: {}",
            update.row_id
        ));
    }
    Ok(())
}

#[cfg(test)]
#[path = "outputs_tests.rs"]
mod tests;
