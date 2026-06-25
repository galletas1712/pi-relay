use agent_vocab::{DaemonToolObservation, TranscriptItem, UserMessage};
use anyhow::Result;
use serde_json::{json, Value};
use sqlx::{postgres::PgRow, Row};
use uuid::Uuid;

use super::events::insert_event_tx;
use super::queue::{
    append_queued_content_event_fields, bump_revisions_tx, queue_event_payload, queue_state_tx,
};
use super::sql::{action_is_unfinished, lock_session_tx, queued_input_is_active, session_activity};
use super::PostgresAgentStore;
use crate::{
    DelegationKind, DelegationStatus, EnqueueUserInputResult, EventFrame, EventType, InputPriority,
    QueuedInputContent, QueuedInputStatus, SessionActivity, SubagentType,
};

/// A durable delegation row: an ordered unit of work under a parent session that
/// is either one full subagent or a fan-out of read-only subagents.
#[derive(Debug, Clone)]
pub struct Delegation {
    pub id: String,
    pub parent_session_id: String,
    pub workflow: Option<String>,
    pub label: Option<String>,
    pub kind: DelegationKind,
    pub status: DelegationStatus,
    pub attempt_id: String,
    /// The full subagent set this delegation will spawn (1 for a full delegation,
    /// `tasks.len()` for a fan-out). The barrier never completes until exactly
    /// this many subagents exist and are all terminal.
    pub expected_subagents: i32,
}

/// A subagent session belonging to a delegation, with the fields
/// `delegation.status` needs to report per-subagent state.
#[derive(Debug, Clone)]
pub struct DelegationSubagent {
    pub session_id: String,
    pub activity: SessionActivity,
    pub subagent_type: Option<SubagentType>,
    pub role: Option<String>,
    pub task: Option<String>,
}

/// Lightweight progress counts for a delegation. This intentionally does not
/// read or render transcript bodies; it only inspects each subagent active leaf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DelegationProgress {
    pub expected: i32,
    pub spawned: i32,
    pub terminal: i32,
    pub running: i32,
    pub failed: i32,
}

/// Compact status fields for a subagent in the run-board list.
///
/// Unlike `delegation.status` / inspect snapshots this deliberately avoids
/// loading the full active branch or touching handoff files. Terminality is
/// derived from the active leaf row, matching `DelegationProgress`.
#[derive(Debug, Clone)]
pub struct DelegationSubagentOverview {
    pub session_id: String,
    pub activity: SessionActivity,
    pub subagent_type: Option<SubagentType>,
    pub role: Option<String>,
    pub has_task: bool,
    pub terminal_status: Option<String>,
}

impl PostgresAgentStore {
    /// Insert a fresh `running` delegation, minting its completion-fencing attempt
    /// id. The delegation row is created before its subagents so their
    /// `delegation_id` FK holds.
    pub async fn create_delegation(
        &self,
        parent_session_id: &str,
        kind: DelegationKind,
        workflow: Option<&str>,
        label: Option<&str>,
        expected_subagents: i32,
    ) -> Result<Delegation> {
        let id = format!("delegation_{}", Uuid::new_v4());
        let attempt_id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            insert into delegations (id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents)
            values ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(&id)
        .bind(parent_session_id)
        .bind(workflow)
        .bind(label)
        .bind(kind.as_str())
        .bind(DelegationStatus::Running.as_str())
        .bind(&attempt_id)
        .bind(expected_subagents)
        .execute(&self.pool)
        .await?;
        Ok(Delegation {
            id,
            parent_session_id: parent_session_id.to_string(),
            workflow: workflow.map(str::to_string),
            label: label.map(str::to_string),
            kind,
            status: DelegationStatus::Running,
            attempt_id,
            expected_subagents,
        })
    }

    /// Compact subagent rows for `delegation.list`.
    ///
    /// This is intentionally set-based: it avoids `active_branch` hydration,
    /// per-subagent `activity()` calls, and handoff filesystem probes. The run
    /// board only needs enough state to draw status dots, open a subagent, and
    /// decide whether re-run can fetch a task prompt on demand.
    pub async fn delegation_subagent_overview(
        &self,
        delegation_id: &str,
    ) -> Result<Vec<DelegationSubagentOverview>> {
        let running_actions = action_is_unfinished(Some("a"));
        let active_queue = queued_input_is_active(Some("q"));
        let query = format!(
            r#"
            select
                s.id,
                s.subagent_type,
                s.metadata,
                s.active_leaf_id,
                te.item,
                exists(select 1 from actions a where a.session_id=s.id and {running_actions}) as has_running_work,
                exists(select 1 from queued_inputs q where q.session_id=s.id and {active_queue}) as has_queued_input
            from sessions s
            left join transcript_entries te
                on te.session_id = s.id
               and te.id = s.active_leaf_id
            where s.delegation_id=$1
            order by s.created_at, s.id
            "#
        );
        let rows = sqlx::query(&query)
            .bind(delegation_id)
            .fetch_all(&self.pool)
            .await?;
        let mut subagents = Vec::with_capacity(rows.len());
        for row in rows {
            let session_id: String = row.get("id");
            let subagent_type: Option<String> = row.get("subagent_type");
            let subagent_type = subagent_type
                .map(|raw| raw.parse::<SubagentType>().map_err(anyhow::Error::msg))
                .transpose()?;
            let metadata: Value = row.get("metadata");
            let role = metadata
                .get("role_name")
                .and_then(Value::as_str)
                .map(str::to_string);
            let has_task = metadata
                .get("task")
                .and_then(Value::as_str)
                .is_some_and(|task| !task.trim().is_empty());
            let has_running_work: bool = row.get("has_running_work");
            let has_queued_input: bool = row.get("has_queued_input");
            let active_leaf_id: Option<String> = row.get("active_leaf_id");
            let terminal_status = if has_running_work || has_queued_input {
                None
            } else if active_leaf_id.is_none() {
                Some("done".to_string())
            } else {
                let item: Option<Value> = row.get("item");
                if let Some(item) = item {
                    match serde_json::from_value::<TranscriptItem>(item)? {
                        TranscriptItem::TurnFinished { outcome, .. } => {
                            let status = if outcome == agent_vocab::TurnOutcome::Graceful {
                                "done"
                            } else {
                                "failed"
                            };
                            Some(status.to_string())
                        }
                        TranscriptItem::CompactionSummary(_) => Some("done".to_string()),
                        _ => None,
                    }
                } else {
                    None
                }
            };
            subagents.push(DelegationSubagentOverview {
                session_id,
                activity: session_activity(has_running_work, has_queued_input),
                subagent_type,
                role,
                has_task,
                terminal_status,
            });
        }
        Ok(subagents)
    }

    pub async fn get_delegation(&self, delegation_id: &str) -> Result<Option<Delegation>> {
        let Some(row) = sqlx::query(
            r#"
            select id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents
            from delegations
            where id=$1
            "#,
        )
        .bind(delegation_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(row_to_delegation(&row)?))
    }

    /// The subagent sessions of a delegation, ordered by creation, with the
    /// activity and type `delegation.status`/`delegation.cancel` need.
    pub async fn list_delegation_subagents(
        &self,
        delegation_id: &str,
    ) -> Result<Vec<DelegationSubagent>> {
        let rows = sqlx::query(
            r#"
            select id, subagent_type, metadata
            from sessions
            where delegation_id=$1
            order by created_at, id
            "#,
        )
        .bind(delegation_id)
        .fetch_all(&self.pool)
        .await?;
        self.rows_to_delegation_subagents(rows).await
    }

    /// Context-bounded subagent sessions of a delegation, ordered by creation.
    ///
    /// This intentionally fetches at most `render_limit + 1` rows: enough for
    /// the compact context renderer to show its bounded window and detect that
    /// additional rows exist, without doing activity lookups for an unbounded
    /// fan-out.
    pub async fn list_delegation_subagents_for_context(
        &self,
        delegation_id: &str,
        render_limit: i64,
    ) -> Result<Vec<DelegationSubagent>> {
        let query_limit = render_limit.max(0).saturating_add(1);
        let rows = sqlx::query(
            r#"
            select id, subagent_type, metadata
            from sessions
            where delegation_id=$1
            order by created_at, id
            limit $2
            "#,
        )
        .bind(delegation_id)
        .bind(query_limit)
        .fetch_all(&self.pool)
        .await?;
        self.rows_to_delegation_subagents(rows).await
    }

    /// All delegations of a parent, oldest first. Backs internal callers that
    /// need the complete per-parent set.
    pub async fn list_parent_delegations(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<Delegation>> {
        let rows = sqlx::query(
            r#"
            select id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents
            from delegations
            where parent_session_id=$1
            order by created_at, id
            "#,
        )
        .bind(parent_session_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_delegation).collect()
    }

    /// A bounded page of delegations for the run board, newest first.
    ///
    /// Fetching all historical delegations made the common selected-session
    /// poll scale with the lifetime of the parent session even though the UI
    /// shows the newest few rows by default.
    pub async fn list_parent_delegations_newest(
        &self,
        parent_session_id: &str,
        limit: i64,
    ) -> Result<Vec<Delegation>> {
        let rows = sqlx::query(
            r#"
            select id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents
            from delegations
            where parent_session_id=$1
            order by created_at desc, id desc
            limit $2
            "#,
        )
        .bind(parent_session_id)
        .bind(limit.max(0))
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_delegation).collect()
    }

    /// Compute compact progress counts for a delegation without materializing
    /// active branches. A subagent is terminal only when it has no active queue
    /// or unfinished action and its active leaf is a turn boundary; a
    /// `TurnFinished` leaf with `Graceful` is a terminal success, other
    /// `TurnFinished` outcomes are terminal failures, and a compaction summary
    /// is terminal success.
    pub async fn delegation_progress(&self, delegation: &Delegation) -> Result<DelegationProgress> {
        let unfinished_actions = action_is_unfinished(Some("a"));
        let active_queue = queued_input_is_active(Some("q"));
        let query = format!(
            r#"
            select s.id,
                   s.active_leaf_id,
                   te.item,
                   exists(select 1 from actions a where a.session_id=s.id and {unfinished_actions}) as has_unfinished_actions,
                   exists(select 1 from queued_inputs q where q.session_id=s.id and {active_queue}) as has_queued_inputs
            from sessions s
            left join transcript_entries te
                on te.session_id = s.id
               and te.id = s.active_leaf_id
            where s.delegation_id=$1
            "#,
        );
        let rows = sqlx::query(&query)
            .bind(&delegation.id)
            .fetch_all(&self.pool)
            .await?;

        let mut terminal = 0i32;
        let mut failed = 0i32;
        for row in &rows {
            if row.get::<bool, _>("has_unfinished_actions")
                || row.get::<bool, _>("has_queued_inputs")
            {
                continue;
            }
            let active_leaf_id: Option<String> = row.get("active_leaf_id");
            if active_leaf_id.is_none() {
                terminal += 1;
                continue;
            }
            let item: Option<Value> = row.get("item");
            if let Some(item) = item {
                let item: TranscriptItem = serde_json::from_value(item)?;
                match item {
                    TranscriptItem::TurnFinished { outcome, .. } => {
                        terminal += 1;
                        if outcome != agent_vocab::TurnOutcome::Graceful {
                            failed += 1;
                        }
                    }
                    TranscriptItem::CompactionSummary(_) => {
                        terminal += 1;
                    }
                    _ => {}
                }
            }
        }
        let spawned = rows.len() as i32;
        let missing = delegation.expected_subagents.saturating_sub(spawned).max(0);
        let running = match delegation.status {
            DelegationStatus::Running => spawned.saturating_sub(terminal) + missing,
            _ => 0,
        };
        Ok(DelegationProgress {
            expected: delegation.expected_subagents,
            spawned,
            terminal,
            running,
            failed,
        })
    }

    /// Whether the parent already owns a `running` delegation. Backs the
    /// one-delegation-per-parent guard.
    pub async fn parent_has_running_delegation(&self, parent_session_id: &str) -> Result<bool> {
        let exists: bool = sqlx::query_scalar(
            r#"
            select exists(
                select 1 from delegations
                where parent_session_id=$1 and status=$2
            )
            "#,
        )
        .bind(parent_session_id)
        .bind(DelegationStatus::Running.as_str())
        .fetch_one(&self.pool)
        .await?;
        Ok(exists)
    }

    /// Mark a delegation's status, e.g. when `delegation.cancel` cancels it. The
    /// barrier's attempt-fenced completion lives in `finish_delegation`.
    pub async fn set_delegation_status(
        &self,
        delegation_id: &str,
        status: DelegationStatus,
    ) -> Result<()> {
        sqlx::query("update delegations set status=$2, updated_at=now() where id=$1")
            .bind(delegation_id)
            .bind(status.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Attempt-fenced cancellation CAS. This is the only path
    /// `delegation.cancel` uses to move an in-flight delegation to `cancelled`:
    /// it updates exactly one row iff the caller observed the current attempt
    /// and the delegation is still `running`. If completion, another cancel, or
    /// spawn-failure termination wins first, this returns `false` and the caller
    /// must not interrupt subagents or publish cancellation transcripts.
    pub async fn cancel_running_delegation(
        &self,
        delegation_id: &str,
        attempt_id: &str,
    ) -> Result<bool> {
        let updated = sqlx::query(
            r#"
            update delegations
            set status='cancelled', updated_at=now()
            where id=$1 and attempt_id=$2 and status='running'
            "#,
        )
        .bind(delegation_id)
        .bind(attempt_id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Whether a delegation has spawned the full subagent set promised by its
    /// expected-count fence. Partial observations must respect this just like
    /// final completion does: otherwise a very fast first child could wake the
    /// parent with a snapshot that omits siblings that have not been inserted
    /// yet.
    pub async fn delegation_spawned_expected_subagents(&self, delegation_id: &str) -> Result<bool> {
        let (spawned, expected): (i64, i32) = sqlx::query_as(
            r#"
            select
                (select count(*) from sessions where delegation_id=$1) as spawned,
                expected_subagents as expected
            from delegations
            where id=$1
            "#,
        )
        .bind(delegation_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(spawned == i64::from(expected))
    }

    /// Cancel queued daemon-authored partial wakeups for a delegation attempt.
    ///
    /// Partial wakeups are parent decision points. If the parent cancels the
    /// delegation, or if final completion wins before the parent consumes an
    /// older partial, any still-queued partial for the same attempt is stale and
    /// must not be observed after terminal status. Already-consuming/consumed
    /// rows are left alone because they have reached, or are reaching, the
    /// transcript.
    pub async fn cancel_queued_partial_delegation_wakeups(
        &self,
        parent_session_id: &str,
        delegation_id: &str,
        attempt_id: &str,
        reason: &str,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, parent_session_id).await?;
        let prefix = partial_delegation_wakeup_client_input_prefix(delegation_id, attempt_id);
        let input_ids = sqlx::query_scalar::<_, String>(
            r#"
            update queued_inputs
            set status='cancelled',
                follow_up_position=null,
                updated_at=now(),
                origin=coalesce(origin, '{}'::jsonb)
                    || jsonb_build_object(
                        'cancelled_at', now()::text,
                        'cancelled_reason', $3
                    )
            where session_id=$1
                and priority='steer'
                and status='queued'
                and left(client_input_id, char_length($2)) = $2
            returning id
            "#,
        )
        .bind(parent_session_id)
        .bind(&prefix)
        .bind(reason)
        .fetch_all(&mut *tx)
        .await?;
        if input_ids.is_empty() {
            tx.commit().await?;
            return Ok(Vec::new());
        }
        bump_revisions_tx(&mut tx, parent_session_id, true, false).await?;
        let queue = queue_state_tx(&mut tx, parent_session_id).await?;
        let event = insert_event_tx(
            &mut tx,
            parent_session_id,
            EventType::InputCancelled,
            queue_event_payload(
                &queue,
                json!({
                    "input_ids": input_ids,
                    "priority": InputPriority::Steer,
                    "status": QueuedInputStatus::Cancelled,
                    "reason": reason,
                    "delegation_id": delegation_id,
                    "attempt_id": attempt_id,
                }),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(vec![event])
    }

    /// Claim the delegation barrier's terminal status transition.
    ///
    /// This is an attempt-fenced `running -> done|done_with_failures` CAS. The
    /// CAS must run before normal handoff publishing so a concurrent
    /// `delegation.cancel` cannot win and then receive a normal completed
    /// handoff. The parent wakeup is intentionally NOT enqueued here: the runner
    /// publishes handoff files first, then enqueues the deterministic typed
    /// wakeup observation, so the parent is never pointed at missing files
    /// during normal operation.
    ///
    /// A crash after this CAS but before file/wakeup publication leaves a
    /// terminal delegation with no wakeup observation. The daemon boot sweep
    /// repairs that by re-rendering terminal handoffs and idempotently
    /// enqueueing any missing deterministic wakeup for completed delegations.
    pub async fn finish_delegation(
        &self,
        delegation_id: &str,
        attempt_id: &str,
        status: DelegationStatus,
    ) -> Result<bool> {
        let unfinished_actions = action_is_unfinished(Some("a"));
        let active_queue = queued_input_is_active(Some("q"));
        let query = format!(
            r#"
            update delegations
            set status=$3, updated_at=now()
            where id=$1
              and attempt_id=$2
              and status='running'
              and not exists (
                select 1
                from sessions s
                join actions a on a.session_id = s.id
                where s.delegation_id = delegations.id
                  and {unfinished_actions}
              )
              and not exists (
                select 1
                from sessions s
                join queued_inputs q on q.session_id = s.id
                where s.delegation_id = delegations.id
                  and {active_queue}
              )
            "#
        );
        let updated = sqlx::query(&query)
            .bind(delegation_id)
            .bind(attempt_id)
            .bind(status.as_str())
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(updated == 1)
    }

    /// Enqueue a steer for a delegation child while holding the delegation row
    /// lock and child session lock. This makes the public subagent-steer path
    /// parent/delegation scoped at the DB boundary: if cancellation/completion
    /// wins first, the insert is rejected instead of steering a terminal child.
    pub async fn enqueue_scoped_subagent_steer(
        &self,
        parent_session_id: &str,
        delegation_id: &str,
        subagent_id: &str,
        content: &UserMessage,
        client_input_id: &str,
    ) -> Result<Option<EnqueueUserInputResult>> {
        let mut tx = self.pool.begin().await?;
        let Some(delegation_row) = sqlx::query(
            r#"
            select parent_session_id, status
            from delegations
            where id=$1
            for update
            "#,
        )
        .bind(delegation_id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let delegation_parent: String = delegation_row.get("parent_session_id");
        let delegation_status: String = delegation_row.get("status");
        if delegation_parent != parent_session_id || delegation_status != "running" {
            tx.commit().await?;
            return Ok(None);
        }

        let Some(session_row) = sqlx::query(
            r#"
            select parent_session_id, delegation_id, subagent_type
            from sessions
            where id=$1
            for update
            "#,
        )
        .bind(subagent_id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.commit().await?;
            return Ok(None);
        };
        let child_parent: Option<String> = session_row.get("parent_session_id");
        let child_delegation: Option<String> = session_row.get("delegation_id");
        let subagent_type: Option<String> = session_row.get("subagent_type");
        if child_parent.as_deref() != Some(parent_session_id)
            || child_delegation.as_deref() != Some(delegation_id)
            || !matches!(subagent_type.as_deref(), Some("full") | Some("read_only"))
        {
            tx.commit().await?;
            return Ok(None);
        }

        if let Some(row) = sqlx::query(
            "select id from queued_inputs where session_id=$1 and client_input_id=$2::text",
        )
        .bind(subagent_id)
        .bind(client_input_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            let input_id = row.get("id");
            let queue = queue_state_tx(&mut tx, subagent_id).await?;
            tx.commit().await?;
            return Ok(Some(EnqueueUserInputResult {
                input_id,
                event: None,
                queue: Some(queue),
            }));
        }

        let id = format!("input_{}", Uuid::new_v4());
        let inserted = sqlx::query(
            r#"
            insert into queued_inputs (
                id, session_id, priority, content, status, client_input_id, origin
            )
            values (
                $1, $2, 'steer', $3, 'queued', $4,
                jsonb_build_object('promoted_at', clock_timestamp()::text)
            )
            on conflict (session_id, client_input_id) where client_input_id is not null
            do nothing
            returning id
            "#,
        )
        .bind(&id)
        .bind(subagent_id)
        .bind(serde_json::to_value(QueuedInputContent::user_message(
            content.clone(),
        ))?)
        .bind(client_input_id)
        .fetch_optional(&mut *tx)
        .await?;
        let input_id = if let Some(inserted) = inserted {
            inserted.get("id")
        } else {
            let row = sqlx::query(
                "select id from queued_inputs where session_id=$1 and client_input_id=$2::text",
            )
            .bind(subagent_id)
            .bind(client_input_id)
            .fetch_one(&mut *tx)
            .await?;
            row.get("id")
        };
        bump_revisions_tx(&mut tx, subagent_id, true, false).await?;
        let queue = queue_state_tx(&mut tx, subagent_id).await?;
        let event = insert_event_tx(
            &mut tx,
            subagent_id,
            EventType::InputQueued,
            queue_event_payload(
                &queue,
                json!({
                    "input_id": input_id,
                    "priority": InputPriority::Steer,
                    "client_input_id": client_input_id,
                    "content": content.content.clone(),
                    "content_type": "user_message",
                }),
            ),
        )
        .await?;
        tx.commit().await?;
        Ok(Some(EnqueueUserInputResult {
            input_id,
            event: Some(event),
            queue: Some(queue),
        }))
    }

    /// Enqueue a parent wakeup with the deterministic delegation/attempt key.
    /// This legacy text-steer compatibility path remains idempotent via the
    /// unique `(session_id, client_input_id)` index, so boot repair or a replay
    /// can call it again without creating a duplicate. Current delegation
    /// completion delivery uses `enqueue_delegation_observation` so daemon facts
    /// are stored as a typed observation rather than human/user message text.
    pub async fn enqueue_delegation_steer(
        &self,
        parent_session_id: &str,
        steer_message: &str,
        steer_client_input_id: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        enqueue_steer_tx(
            &mut tx,
            parent_session_id,
            steer_message,
            steer_client_input_id,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Enqueue a partial parent wakeup only if the delegation attempt is still
    /// running and no other active partial wakeup for the same attempt is
    /// already queued or being consumed.
    ///
    /// The active-partial check is inside the same transaction as the insert and
    /// is protected by the parent session row lock plus the delegation row lock.
    /// This is intentionally broader than idempotency for one child id: if two
    /// terminal children race, exactly one queued/consuming partial decision
    /// point may exist for `(parent_session_id, delegation_id, attempt_id)`.
    pub async fn enqueue_partial_delegation_observation_if_running(
        &self,
        parent_session_id: &str,
        delegation_id: &str,
        attempt_id: &str,
        observation: &DaemonToolObservation,
        client_input_id: &str,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, parent_session_id).await?;
        let status = sqlx::query_scalar::<_, String>(
            r#"
            select status
            from delegations
            where id=$1
                and parent_session_id=$2
                and attempt_id=$3
            for update
            "#,
        )
        .bind(delegation_id)
        .bind(parent_session_id)
        .bind(attempt_id)
        .fetch_optional(&mut *tx)
        .await?;
        if status.as_deref() != Some(DelegationStatus::Running.as_str()) {
            tx.commit().await?;
            return Ok(false);
        }

        let prefix = partial_delegation_wakeup_client_input_prefix(delegation_id, attempt_id);
        let active_partial_exists: bool = sqlx::query_scalar(
            r#"
            select exists(
                select 1
                from queued_inputs
                where session_id=$1
                    and priority='steer'
                    and status in ('queued', 'consuming')
                    and left(client_input_id, char_length($2)) = $2
            )
            "#,
        )
        .bind(parent_session_id)
        .bind(&prefix)
        .fetch_one(&mut *tx)
        .await?;
        if active_partial_exists {
            tx.commit().await?;
            return Ok(false);
        }

        let inserted = enqueue_steer_content_tx(
            &mut tx,
            parent_session_id,
            QueuedInputContent::daemon_tool_observation(observation.clone()),
            client_input_id,
        )
        .await?;
        tx.commit().await?;
        Ok(inserted)
    }

    /// Enqueue a typed daemon-authored observation that wakes the parent in the
    /// same way as a deterministic completion wakeup, without storing daemon
    /// facts as human/user message text.
    pub async fn enqueue_delegation_observation(
        &self,
        parent_session_id: &str,
        observation: &DaemonToolObservation,
        client_input_id: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        enqueue_steer_content_tx(
            &mut tx,
            parent_session_id,
            QueuedInputContent::daemon_tool_observation(observation.clone()),
            client_input_id,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Whether every subagent of a delegation is terminal. Two fences guard
    /// against a premature completion:
    ///
    /// 1. Expected-count fence: the delegation must have spawned its FULL set of
    ///    subagents. A fan-out spawns its children in a loop while each child
    ///    drives in a detached task, so subagent #1 can reach terminal before #2
    ///    is even inserted. Requiring `count(sessions where delegation_id) ==
    ///    expected_subagents` keeps the barrier closed during that window.
    ///
    /// 2. No active DB work: accepted queued steers/follow-ups and unfinished
    ///    model/tool/compaction actions keep the child non-terminal even if its
    ///    previous active leaf is a boundary. This closes the fan-out race where
    ///    a queued steer is accepted while a sibling runs the barrier.
    ///
    /// 3. Transcript-boundary terminality: once no active DB work remains, a
    ///    subagent is terminal only when its active leaf is a genuine turn
    ///    boundary (`TurnFinished` / compaction summary). A subagent that crashed
    ///    MID-TURN (boot's stale-mark erased its unfinished action, and it had no
    ///    queued input) is correctly NON-terminal and stays in the delegation
    ///    until it is recovered to a boundary.
    pub async fn delegation_subagents_all_terminal(&self, delegation_id: &str) -> Result<bool> {
        let session_ids: Vec<String> =
            sqlx::query_scalar("select id from sessions where delegation_id=$1")
                .bind(delegation_id)
                .fetch_all(&self.pool)
                .await?;
        if (session_ids.len() as i32) != self.delegation_expected_subagents(delegation_id).await? {
            return Ok(false);
        }
        for session_id in &session_ids {
            if self.has_unfinished_actions(session_id).await?
                || self.has_queued_inputs(session_id).await?
            {
                return Ok(false);
            }
            if !self.active_leaf_is_turn_boundary(session_id).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn delegation_expected_subagents(&self, delegation_id: &str) -> Result<i32> {
        sqlx::query_scalar("select expected_subagents from delegations where id=$1")
            .bind(delegation_id)
            .fetch_one(&self.pool)
            .await
            .map_err(Into::into)
    }

    /// Running delegations whose subagents are all terminal — the boot-sweep
    /// input. A crash mid-barrier leaves such a delegation `running` with every
    /// subagent
    /// idle; the sweep re-runs `finish_delegation` so it completes exactly once.
    pub async fn sweep_running_delegations(&self) -> Result<Vec<Delegation>> {
        let running = self.list_running_delegations().await?;
        let mut ready = Vec::new();
        for delegation in running {
            if self
                .delegation_subagents_all_terminal(&delegation.id)
                .await?
            {
                ready.push(delegation);
            }
        }
        Ok(ready)
    }

    pub async fn list_running_delegations(&self) -> Result<Vec<Delegation>> {
        let rows = sqlx::query(
            r#"
            select id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents
            from delegations
            where status='running'
            order by created_at, id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_delegation).collect()
    }

    async fn rows_to_delegation_subagents(
        &self,
        rows: Vec<PgRow>,
    ) -> Result<Vec<DelegationSubagent>> {
        let mut subagents = Vec::with_capacity(rows.len());
        for row in rows {
            let session_id: String = row.get("id");
            let subagent_type: Option<String> = row.get("subagent_type");
            let subagent_type = subagent_type
                .map(|raw| raw.parse::<SubagentType>().map_err(anyhow::Error::msg))
                .transpose()?;
            let metadata: Value = row.get("metadata");
            let role = metadata
                .get("role_name")
                .and_then(Value::as_str)
                .map(str::to_string);
            // The subagent's task prompt, persisted at spawn — carried in
            // delegation.list so the run board can re-run a delegation from
            // the delegation data itself.
            let task = metadata
                .get("task")
                .and_then(Value::as_str)
                .map(str::to_string);
            let activity = self.activity(&session_id).await?;
            subagents.push(DelegationSubagent {
                session_id,
                activity,
                subagent_type,
                role,
                task,
            });
        }
        Ok(subagents)
    }

    /// Completed delegations that may need boot-time publication repair. The
    /// normal barrier claims the terminal status before writing files/enqueueing
    /// the parent wakeup observation; if the daemon crashes in that narrow gap,
    /// these rows are no longer `running` and therefore are not covered by the
    /// ordinary running-delegation sweep.
    pub async fn list_completed_delegations_for_repair(&self) -> Result<Vec<Delegation>> {
        let rows = sqlx::query(
            r#"
            select id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents
            from delegations
            where status in ('done', 'done_with_failures')
            order by updated_at, id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_delegation).collect()
    }
}

/// Insert a text steer as a durable queued input inside the caller's
/// transaction, idempotent on `(session_id, client_input_id)`. A re-run with
/// the same key inserts nothing and emits no duplicate event. Mirrors the steer
/// branch of `enqueue_user_input`.
async fn enqueue_steer_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    parent_session_id: &str,
    message: &str,
    client_input_id: &str,
) -> Result<bool> {
    enqueue_steer_content_tx(
        tx,
        parent_session_id,
        QueuedInputContent::user_message(UserMessage::text(message)),
        client_input_id,
    )
    .await
}

async fn enqueue_steer_content_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    parent_session_id: &str,
    content: QueuedInputContent,
    client_input_id: &str,
) -> Result<bool> {
    lock_session_tx(tx, parent_session_id).await?;
    let id = format!("input_{}", Uuid::new_v4());
    let inserted = sqlx::query(
        r#"
            insert into queued_inputs (
                id, session_id, priority, content, status, client_input_id, origin
            )
            values (
                $1, $2, 'steer', $3, 'queued', $4,
                jsonb_build_object('promoted_at', clock_timestamp()::text)
            )
            on conflict (session_id, client_input_id) where client_input_id is not null
            do nothing
            returning id
            "#,
    )
    .bind(&id)
    .bind(parent_session_id)
    .bind(serde_json::to_value(&content)?)
    .bind(client_input_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(inserted) = inserted else {
        return Ok(false);
    };
    bump_revisions_tx(tx, parent_session_id, true, false).await?;
    let queue = queue_state_tx(tx, parent_session_id).await?;
    let input_id: String = inserted.get("id");
    let mut payload = json!({
        "input_id": input_id,
        "priority": InputPriority::Steer,
        "status": QueuedInputStatus::Queued,
        "client_input_id": client_input_id,
    });
    if matches!(&content, QueuedInputContent::UserMessage(_)) {
        append_queued_content_event_fields(&mut payload, &content);
    }
    insert_event_tx(
        tx,
        parent_session_id,
        EventType::InputQueued,
        queue_event_payload(&queue, payload),
    )
    .await?;
    Ok(true)
}

fn partial_delegation_wakeup_client_input_prefix(delegation_id: &str, attempt_id: &str) -> String {
    format!("delegation-steer:{delegation_id}:{attempt_id}:")
}

fn row_to_delegation(row: &sqlx::postgres::PgRow) -> Result<Delegation> {
    let kind: String = row.get("kind");
    let status: String = row.get("status");
    Ok(Delegation {
        id: row.get("id"),
        parent_session_id: row.get("parent_session_id"),
        workflow: row.get("workflow"),
        label: row.get("label"),
        kind: kind.parse().map_err(anyhow::Error::msg)?,
        status: status.parse().map_err(anyhow::Error::msg)?,
        attempt_id: row.get("attempt_id"),
        expected_subagents: row.get("expected_subagents"),
    })
}

#[cfg(test)]
#[path = "delegations_tests.rs"]
mod tests;
