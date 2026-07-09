use agent_vocab::{DaemonToolObservation, TranscriptItem, UserMessage};
use anyhow::Result;
use serde_json::{json, Value};
use sqlx::{postgres::PgRow, Postgres, Row, Transaction};
use uuid::Uuid;

use super::events::insert_event_tx;
use super::queue::{
    append_queued_content_event_fields, bump_revisions_tx, queue_event_payload, queue_state_tx,
};
use super::sql::{action_is_unfinished, lock_session_tx, queued_input_is_active, session_activity};
use super::PostgresAgentStore;
use crate::{
    DelegationKind, DelegationStatus, EnqueueUserInputResult, EventFrame, EventType, InputPriority,
    QueuedInputContent, QueuedInputStatus, SessionActivity, SubagentBoundaryInterruptResult,
    SubagentControlKind, SubagentControlPhase, SubagentControlRecord, SubagentType,
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

/// Compact status fields for a subagent in `delegation.list`.
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

struct ScopedSubagentControlRequest<'a> {
    parent_session_id: &'a str,
    delegation_id: &'a str,
    subagent_id: &'a str,
    client_input_id: &'a str,
    kind: SubagentControlKind,
    content: Option<&'a UserMessage>,
    interrupt: bool,
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
        if expected_subagents <= 0 {
            anyhow::bail!("expected_subagents must be greater than zero");
        }
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
    /// per-subagent `activity()` calls, and handoff filesystem probes. The
    /// product surface only needs enough state to draw status icons, open a
    /// subagent, and expose task-prompt handoff availability for inspection.
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

    /// A bounded page of delegations for the product Agents outline, newest first.
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

    /// Attempt-fenced cancellation CAS that also cancels any active
    /// partial wakeups for the same delegation attempt in the same transaction.
    ///
    /// This is the user-visible cancellation path. Keeping the status
    /// transition and active partial cancellation in one commit prevents a
    /// crash from leaving a terminal (`cancelled`) delegation with a stale
    /// active running-snapshot wakeup queued on its top-level parent. Only
    /// deterministic partial wakeups for this exact `(delegation_id,
    /// attempt_id)` are cancelled; terminal wakeups and unrelated user
    /// follow-ups do not share the `delegation-steer:{id}:{attempt}:` prefix and
    /// are left alone.
    pub async fn cancel_running_delegation_and_queued_partials(
        &self,
        parent_session_id: &str,
        delegation_id: &str,
        attempt_id: &str,
        reason: &str,
    ) -> Result<(bool, Vec<EventFrame>)> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, parent_session_id).await?;
        let status = sqlx::query_scalar::<_, String>(
            r#"
            select status
            from delegations
            where id=$1 and parent_session_id=$2 and attempt_id=$3
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
            return Ok((false, Vec::new()));
        }
        let updated = sqlx::query(
            r#"
            update delegations
            set status='cancelled', updated_at=now()
            where id=$1
              and parent_session_id=$2
              and attempt_id=$3
              and status='running'
            "#,
        )
        .bind(delegation_id)
        .bind(parent_session_id)
        .bind(attempt_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            tx.commit().await?;
            return Ok((false, Vec::new()));
        }
        let mut events = Vec::new();

        // A terminal delegation has no active child mailbox. Keep the global
        // session -> queue-row order used by output/control reconciliation:
        // after the parent/delegation scope locks, lock every child session in
        // deterministic order before touching any child queue row.
        let child_ids = sqlx::query_scalar::<_, String>(
            r#"
            select id
            from sessions
            where delegation_id=$1
            order by id
            for update
            "#,
        )
        .bind(delegation_id)
        .fetch_all(&mut *tx)
        .await?;
        let child_input_rows = sqlx::query(
            r#"
            update queued_inputs q
            set status='cancelled',
                follow_up_position=null,
                updated_at=now(),
                origin=coalesce(q.origin, '{}'::jsonb)
                    || jsonb_build_object(
                        'control_phase',
                            case
                                when q.origin->>'control_kind' in (
                                    'scoped_subagent_steer',
                                    'scoped_subagent_interrupt'
                                )
                                then 'cancelled'
                                else coalesce(q.origin->>'control_phase', 'cancelled')
                            end,
                        'cancelled_at', clock_timestamp()::text,
                        'cancel_reason', $2
                    )
            from sessions s
            where q.session_id=s.id
              and s.delegation_id=$1
              and q.status in ('queued', 'consuming')
            returning q.session_id, q.id
            "#,
        )
        .bind(delegation_id)
        .bind(reason)
        .fetch_all(&mut *tx)
        .await?;
        if !child_input_rows.is_empty() {
            for child_id in child_ids {
                let child_input_ids = child_input_rows
                    .iter()
                    .filter(|row| row.get::<String, _>("session_id") == child_id)
                    .map(|row| row.get::<String, _>("id"))
                    .collect::<Vec<_>>();
                if child_input_ids.is_empty() {
                    continue;
                }
                bump_revisions_tx(&mut tx, &child_id, true, false).await?;
                let queue = queue_state_tx(&mut tx, &child_id).await?;
                events.push(
                    insert_event_tx(
                        &mut tx,
                        &child_id,
                        EventType::InputCancelled,
                        queue_event_payload(
                            &queue,
                            json!({
                                "input_ids": child_input_ids,
                                "reason": reason,
                                "delegation_id": delegation_id,
                            }),
                        ),
                    )
                    .await?,
                );
            }
        }

        let input_ids = cancel_active_partial_delegation_wakeups_tx(
            &mut tx,
            parent_session_id,
            delegation_id,
            attempt_id,
            reason,
        )
        .await?;
        events.extend(
            partial_wakeup_cancellation_events_tx(
                &mut tx,
                parent_session_id,
                delegation_id,
                attempt_id,
                reason,
                input_ids,
            )
            .await?,
        );
        tx.commit().await?;
        Ok((true, events))
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

    /// Cancel active daemon-authored partial wakeups for a delegation attempt.
    ///
    /// Partial wakeups are parent decision points. If the parent cancels the
    /// delegation, or if final completion wins before the parent consumes an
    /// older partial, any queued/consuming partial for the same attempt is stale
    /// and must not be observed after terminal status. Already-consumed rows are
    /// left alone because they have reached the transcript.
    pub async fn cancel_queued_partial_delegation_wakeups(
        &self,
        parent_session_id: &str,
        delegation_id: &str,
        attempt_id: &str,
        reason: &str,
    ) -> Result<Vec<EventFrame>> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, parent_session_id).await?;
        let input_ids = cancel_active_partial_delegation_wakeups_tx(
            &mut tx,
            parent_session_id,
            delegation_id,
            attempt_id,
            reason,
        )
        .await?;
        let events = partial_wakeup_cancellation_events_tx(
            &mut tx,
            parent_session_id,
            delegation_id,
            attempt_id,
            reason,
            input_ids,
        )
        .await?;
        tx.commit().await?;
        Ok(events)
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
        let mut tx = self.pool.begin().await?;
        let Some(expected_subagents) = sqlx::query_scalar::<_, i32>(
            r#"
            select expected_subagents
            from delegations
            where id=$1 and attempt_id=$2 and status='running'
            for update
            "#,
        )
        .bind(delegation_id)
        .bind(attempt_id)
        .fetch_optional(&mut *tx)
        .await?
        else {
            tx.commit().await?;
            return Ok(false);
        };

        // This statement runs after the delegation row lock is acquired. A
        // scoped child control also locks the delegation before its child
        // session, so either completion wins and the control rejects, or the
        // control commits and this fresh check observes its active state.
        let unfinished_actions = action_is_unfinished(Some("a"));
        let active_queue = queued_input_is_active(Some("q"));
        let query = format!(
            r#"
            select
                count(*) = $2
                and coalesce(bool_and(
                    not exists (
                        select 1
                        from actions a
                        where a.session_id = s.id and {unfinished_actions}
                    )
                    and not exists (
                        select 1
                        from queued_inputs q
                        where q.session_id = s.id and {active_queue}
                    )
                    and (
                        s.active_leaf_id is null
                        or exists (
                            select 1
                            from transcript_entries t
                            where t.session_id = s.id
                              and t.id = s.active_leaf_id
                              and t.item->>'type' in ('turn_finished', 'compaction_summary')
                        )
                    )
                ), false)
            from sessions s
            where s.delegation_id=$1
            "#
        );
        let ready: bool = sqlx::query_scalar(&query)
            .bind(delegation_id)
            .bind(i64::from(expected_subagents))
            .fetch_one(&mut *tx)
            .await?;
        if !ready {
            tx.commit().await?;
            return Ok(false);
        }
        let updated = sqlx::query(
            r#"
            update delegations
            set status=$3, updated_at=now()
            where id=$1 and attempt_id=$2 and status='running'
            "#,
        )
        .bind(delegation_id)
        .bind(attempt_id)
        .bind(status.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        tx.commit().await?;
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
        control_interrupt: bool,
    ) -> Result<Option<EnqueueUserInputResult>> {
        self.enqueue_scoped_subagent_control(ScopedSubagentControlRequest {
            parent_session_id,
            delegation_id,
            subagent_id,
            client_input_id,
            kind: SubagentControlKind::Steer,
            content: Some(content),
            interrupt: control_interrupt,
        })
        .await
    }

    /// Enqueue a durable exact-child interrupt ledger record. Unlike a steer,
    /// this row carries no user message and is never dispatchable to the model.
    pub async fn enqueue_scoped_subagent_interrupt(
        &self,
        parent_session_id: &str,
        delegation_id: &str,
        subagent_id: &str,
        client_input_id: &str,
    ) -> Result<Option<EnqueueUserInputResult>> {
        self.enqueue_scoped_subagent_control(ScopedSubagentControlRequest {
            parent_session_id,
            delegation_id,
            subagent_id,
            client_input_id,
            kind: SubagentControlKind::Interrupt,
            content: None,
            interrupt: true,
        })
        .await
    }

    async fn enqueue_scoped_subagent_control(
        &self,
        request: ScopedSubagentControlRequest<'_>,
    ) -> Result<Option<EnqueueUserInputResult>> {
        let ScopedSubagentControlRequest {
            parent_session_id,
            delegation_id,
            subagent_id,
            client_input_id,
            kind: control_kind,
            content,
            interrupt: control_interrupt,
        } = request;
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
        if delegation_parent != parent_session_id {
            tx.commit().await?;
            return Ok(None);
        }

        let reused_by_other_child: bool = sqlx::query_scalar(
            r#"
            select exists(
                select 1
                from queued_inputs q
                join sessions s on s.id=q.session_id
                where s.delegation_id=$1
                  and q.client_input_id=$2
                  and q.session_id<>$3
            )
            "#,
        )
        .bind(delegation_id)
        .bind(client_input_id)
        .bind(subagent_id)
        .fetch_one(&mut *tx)
        .await?;
        if reused_by_other_child {
            anyhow::bail!(
                "client_control_id_conflict: client_control_id was already used for a different subagent control"
            );
        }

        let Some(session_row) = sqlx::query(
            r#"
            select s.parent_session_id,
                   s.delegation_id,
                   s.subagent_type,
                   s.active_leaf_id,
                   coalesce(generation.provider_config, s.provider_config) as provider_config,
                   generation.turn_id as target_turn_id,
                   coalesce(generation.attempt_ids, '[]'::jsonb) as target_action_attempt_ids
            from sessions s
            left join lateral (
                select newest.turn_id,
                       newest.provider_config,
                       (
                           select jsonb_agg(a.attempt_id order by a.created_at, a.id)
                           from actions a
                           where a.session_id=s.id
                             and a.status in ('pending', 'blocked', 'running')
                             and a.turn_id is not distinct from newest.turn_id
                       ) as attempt_ids
                from (
                    select a.turn_id, a.provider_config
                    from actions a
                    where a.session_id=s.id
                      and a.status in ('pending', 'blocked', 'running')
                    order by a.created_at desc, a.id desc
                    limit 1
                ) newest
            ) generation on true
            where s.id=$1
            for update of s
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
        let target_active_leaf_id: Option<String> = session_row.get("active_leaf_id");
        let target_turn_id: Option<i64> = session_row.get("target_turn_id");
        let target_action_attempt_ids: Value = session_row.get("target_action_attempt_ids");
        let provider_config: Value = session_row.get("provider_config");
        if child_parent.as_deref() != Some(parent_session_id)
            || child_delegation.as_deref() != Some(delegation_id)
            || !matches!(subagent_type.as_deref(), Some("full") | Some("read_only"))
        {
            tx.commit().await?;
            return Ok(None);
        }

        if let Some(row) = sqlx::query(
            r#"
            select id, priority, content, status,
                   coalesce((origin->>'control_interrupt')::boolean, false) as control_interrupt,
                   origin->>'control_phase' as control_phase,
                   coalesce((origin->>'control_interrupted')::boolean, false) as control_interrupted,
                   origin->>'control_interrupt_outcome' as control_interrupt_outcome,
                   origin->>'control_kind' as control_kind,
                   origin->>'delegation_id' as control_delegation_id,
                   origin->>'parent_session_id' as control_parent_session_id,
                   origin->>'subagent_id' as control_subagent_id,
                   nullif(origin->>'target_turn_id', '')::bigint as target_turn_id,
                   origin->>'target_active_leaf_id' as target_active_leaf_id,
                   coalesce(origin->'target_action_attempt_ids', '[]'::jsonb)
                       as target_action_attempt_ids
            from queued_inputs
            where session_id=$1 and client_input_id=$2::text
            "#,
        )
        .bind(subagent_id)
        .bind(client_input_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            let existing_content: QueuedInputContent =
                serde_json::from_value(row.get::<Value, _>("content"))?;
            let existing_interrupt: bool = row.get("control_interrupt");
            let priority = super::rows::row_text::<InputPriority>(&row, "priority")?;
            let existing_control_kind: Option<String> = row.get("control_kind");
            let control_delegation_id: Option<String> = row.get("control_delegation_id");
            let control_parent_session_id: Option<String> = row.get("control_parent_session_id");
            let control_subagent_id: Option<String> = row.get("control_subagent_id");
            let status = super::rows::row_text::<QueuedInputStatus>(&row, "status")?;
            let requested_content = content
                .cloned()
                .map(QueuedInputContent::user_message)
                .unwrap_or(QueuedInputContent::SubagentControl);
            if priority != InputPriority::Steer
                || existing_control_kind.as_deref() != Some(control_kind.as_str())
                || control_delegation_id.as_deref() != Some(delegation_id)
                || control_parent_session_id.as_deref() != Some(parent_session_id)
                || control_subagent_id.as_deref() != Some(subagent_id)
                || existing_content != requested_content
                || existing_interrupt != control_interrupt
            {
                anyhow::bail!(
                    "client_control_id_conflict: client_control_id was already used for a different subagent control"
                );
            }
            let control_phase = row
                .get::<Option<String>, _>("control_phase")
                .ok_or_else(|| anyhow::anyhow!("scoped subagent control is missing its phase"))?
                .parse::<SubagentControlPhase>()
                .map_err(anyhow::Error::msg)?;
            let control_interrupt_applied =
                control_phase != SubagentControlPhase::PendingInterrupt;
            let input_id = row.get("id");
            let queue = queue_state_tx(&mut tx, subagent_id).await?;
            tx.commit().await?;
            return Ok(Some(EnqueueUserInputResult {
                input_id,
                event: None,
                queue: Some(queue),
                replayed: true,
                status,
                control_interrupt_applied,
                delegation_running: delegation_status == "running",
                control_phase: Some(control_phase),
                control_interrupt_outcome: row.get("control_interrupt_outcome"),
            }));
        }
        if delegation_status != "running" {
            tx.commit().await?;
            return Ok(None);
        }

        let id = format!("input_{}", Uuid::new_v4());
        let inserted = sqlx::query(
            r#"
            insert into queued_inputs (
                id, session_id, priority, content, status, client_input_id, origin,
                provider_config
            )
            values (
                $1, $2, 'steer', $3, 'queued', $4,
                jsonb_build_object(
                    'promoted_at', clock_timestamp()::text,
                    'control_kind', $11::text,
                    'delegation_id', $6::text,
                    'parent_session_id', $7::text,
                    'subagent_id', $2::text,
                    'control_interrupt', $5,
                    'control_phase',
                        case when $5 then 'pending_interrupt' else 'ready' end,
                    'target_active_leaf_id', $8::text,
                    'target_turn_id', $9::bigint,
                    'target_action_attempt_ids', $10::jsonb
                ),
                $12
            )
            on conflict (session_id, client_input_id) where client_input_id is not null
            do nothing
            returning id
            "#,
        )
        .bind(&id)
        .bind(subagent_id)
        .bind(serde_json::to_value(
            content
                .cloned()
                .map(QueuedInputContent::user_message)
                .unwrap_or(QueuedInputContent::SubagentControl),
        )?)
        .bind(client_input_id)
        .bind(control_interrupt)
        .bind(delegation_id)
        .bind(parent_session_id)
        .bind(target_active_leaf_id)
        .bind(target_turn_id)
        .bind(target_action_attempt_ids)
        .bind(control_kind.as_str())
        .bind(provider_config)
        .fetch_optional(&mut *tx)
        .await?;
        let input_id = if let Some(inserted) = inserted {
            inserted.get("id")
        } else {
            anyhow::bail!(
                "client_control_id_conflict: client_control_id was concurrently used for a different input"
            );
        };
        bump_revisions_tx(&mut tx, subagent_id, true, false).await?;
        let queue = queue_state_tx(&mut tx, subagent_id).await?;
        let event = if let Some(content) = content {
            Some(
                insert_event_tx(
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
                .await?,
            )
        } else {
            None
        };
        tx.commit().await?;
        Ok(Some(EnqueueUserInputResult {
            input_id,
            event,
            queue: Some(queue),
            replayed: false,
            status: QueuedInputStatus::Queued,
            control_interrupt_applied: false,
            delegation_running: true,
            control_phase: Some(if control_interrupt {
                SubagentControlPhase::PendingInterrupt
            } else {
                SubagentControlPhase::Ready
            }),
            control_interrupt_outcome: None,
        }))
    }

    pub async fn get_scoped_subagent_control(
        &self,
        subagent_id: &str,
        client_input_id: &str,
        parent_session_id: &str,
        delegation_id: &str,
        content: &UserMessage,
        interrupt: bool,
    ) -> Result<Option<SubagentControlRecord>> {
        self.get_scoped_subagent_control_matching(ScopedSubagentControlRequest {
            parent_session_id,
            delegation_id,
            subagent_id,
            client_input_id,
            kind: SubagentControlKind::Steer,
            content: Some(content),
            interrupt,
        })
        .await
    }

    pub async fn get_scoped_subagent_interrupt(
        &self,
        subagent_id: &str,
        client_input_id: &str,
        parent_session_id: &str,
        delegation_id: &str,
    ) -> Result<Option<SubagentControlRecord>> {
        self.get_scoped_subagent_control_matching(ScopedSubagentControlRequest {
            parent_session_id,
            delegation_id,
            subagent_id,
            client_input_id,
            kind: SubagentControlKind::Interrupt,
            content: None,
            interrupt: true,
        })
        .await
    }

    async fn get_scoped_subagent_control_matching(
        &self,
        request: ScopedSubagentControlRequest<'_>,
    ) -> Result<Option<SubagentControlRecord>> {
        let ScopedSubagentControlRequest {
            parent_session_id,
            delegation_id,
            subagent_id,
            client_input_id,
            kind: control_kind,
            content,
            interrupt,
        } = request;
        let Some(row) = sqlx::query(
            r#"
            select q.id,
                   q.priority,
                   q.content,
                   q.status,
                   q.origin,
                   d.status as delegation_status
            from queued_inputs q
            join sessions s on s.id=q.session_id
            join delegations d on d.id=s.delegation_id
            where q.session_id=$1 and q.client_input_id=$2
            "#,
        )
        .bind(subagent_id)
        .bind(client_input_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let priority = super::rows::row_text::<InputPriority>(&row, "priority")?;
        let existing_content: QueuedInputContent =
            serde_json::from_value(row.get::<Value, _>("content"))?;
        let origin: Value = row.get("origin");
        let expected_content = content
            .cloned()
            .map(QueuedInputContent::user_message)
            .unwrap_or(QueuedInputContent::SubagentControl);
        let matches = priority == InputPriority::Steer
            && existing_content == expected_content
            && origin.get("control_kind").and_then(Value::as_str) == Some(control_kind.as_str())
            && origin.get("delegation_id").and_then(Value::as_str) == Some(delegation_id)
            && origin.get("parent_session_id").and_then(Value::as_str) == Some(parent_session_id)
            && origin.get("subagent_id").and_then(Value::as_str) == Some(subagent_id)
            && origin
                .get("control_interrupt")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                == interrupt;
        if !matches {
            anyhow::bail!(
                "client_control_id_conflict: client_control_id was already used for a different subagent control"
            );
        }
        Ok(Some(control_record_from_row(&row, &origin)?))
    }

    pub async fn get_subagent_control_by_input_id(
        &self,
        subagent_id: &str,
        input_id: &str,
    ) -> Result<Option<SubagentControlRecord>> {
        let Some(row) = sqlx::query(
            r#"
            select q.id, q.status, q.origin, d.status as delegation_status
            from queued_inputs q
            join sessions s on s.id=q.session_id
            join delegations d on d.id=s.delegation_id
            where q.session_id=$1
              and q.id=$2
              and q.priority='steer'
              and q.origin->>'control_kind' in (
                  'scoped_subagent_steer',
                  'scoped_subagent_interrupt'
              )
            "#,
        )
        .bind(subagent_id)
        .bind(input_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let origin: Value = row.get("origin");
        Ok(Some(control_record_from_row(&row, &origin)?))
    }

    /// Sessions needing bounded live control recovery: interrupt phases always,
    /// plus ready scoped steers that have no unfinished action owner.
    pub async fn sessions_with_recoverable_subagent_controls(&self) -> Result<Vec<String>> {
        let unfinished_actions = action_is_unfinished(Some("a"));
        Ok(sqlx::query_scalar(&format!(
            r#"
            select distinct q.session_id
            from queued_inputs q
            join sessions s on s.id=q.session_id
            join delegations d on d.id=s.delegation_id
            where q.status in ('queued', 'consuming')
              and q.priority='steer'
              and q.origin->>'control_kind' in (
                  'scoped_subagent_steer',
                  'scoped_subagent_interrupt'
              )
              and (
                  q.origin->>'control_phase' in ('pending_interrupt', 'interrupt_applied')
                  or (
                      q.origin->>'control_kind'='scoped_subagent_steer'
                      and q.origin->>'control_phase'='ready'
                      and not exists (
                          select 1
                          from actions a
                          where a.session_id=q.session_id
                            and {unfinished_actions}
                      )
                  )
              )
              and d.status='running'
            order by q.session_id
            limit 100
            "#
        ))
        .fetch_all(&self.pool)
        .await?)
    }

    pub async fn next_pending_subagent_control(
        &self,
        subagent_id: &str,
    ) -> Result<Option<SubagentControlRecord>> {
        let Some(row) = sqlx::query(
            r#"
            select q.id, q.status, q.origin, d.status as delegation_status
            from queued_inputs q
            join sessions s on s.id=q.session_id
            join delegations d on d.id=s.delegation_id
            where q.session_id=$1
              and q.status in ('queued', 'consuming')
              and q.priority='steer'
              and q.origin->>'control_kind' in (
                  'scoped_subagent_steer',
                  'scoped_subagent_interrupt'
              )
              and q.origin->>'control_phase' in ('pending_interrupt', 'interrupt_applied')
              and d.status='running'
            order by q.created_at, q.id
            limit 1
            "#,
        )
        .bind(subagent_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        let origin: Value = row.get("origin");
        Ok(Some(control_record_from_row(&row, &origin)?))
    }

    pub async fn subagent_control_target_is_current(
        &self,
        subagent_id: &str,
        control: &SubagentControlRecord,
    ) -> Result<bool> {
        let row = sqlx::query(
            r#"
            select s.active_leaf_id
            from sessions s
            where s.id=$1
            "#,
        )
        .bind(subagent_id)
        .fetch_one(&self.pool)
        .await?;
        let active_leaf_id: Option<String> = row.get("active_leaf_id");
        let unfinished_actions = sqlx::query(
            r#"
            select attempt_id, turn_id
            from actions
            where session_id=$1
              and status in ('pending', 'blocked', 'running')
            order by created_at, id
            "#,
        )
        .bind(subagent_id)
        .fetch_all(&self.pool)
        .await?;
        if control.target_action_attempt_ids.is_empty() {
            return Ok(
                active_leaf_id == control.target_active_leaf_id && unfinished_actions.is_empty()
            );
        }
        Ok(!unfinished_actions.is_empty()
            && unfinished_actions.iter().all(|action| {
                let attempt_id: String = action.get("attempt_id");
                let turn_id: Option<i64> = action.get("turn_id");
                turn_id == control.target_turn_id
                    && control
                        .target_action_attempt_ids
                        .iter()
                        .any(|captured| captured == &attempt_id)
            }))
    }

    pub async fn mark_subagent_control_ready(
        &self,
        subagent_id: &str,
        input_id: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, subagent_id).await?;
        let updated = sqlx::query(
            r#"
            update queued_inputs
            set origin=origin || jsonb_build_object(
                    'control_phase', 'ready',
                    'control_ready_at', clock_timestamp()::text
                ),
                status=case
                    when origin->>'control_kind'='scoped_subagent_interrupt'
                    then 'consumed'
                    else status
                end,
                updated_at=now()
            where session_id=$1
              and id=$2
              and status='queued'
              and priority='steer'
              and origin->>'control_kind' in (
                  'scoped_subagent_steer',
                  'scoped_subagent_interrupt'
              )
              and origin->>'control_phase'='interrupt_applied'
            "#,
        )
        .bind(subagent_id)
        .bind(input_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            anyhow::bail!(
                "subagent control ready phase update affected {updated} rows for {input_id}"
            );
        }
        bump_revisions_tx(&mut tx, subagent_id, true, false).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn skip_stale_subagent_control_interrupt(
        &self,
        subagent_id: &str,
        input_id: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, subagent_id).await?;
        let updated = sqlx::query(
            r#"
            update queued_inputs
            set origin=origin || jsonb_build_object(
                    'control_phase', 'ready',
                    'control_interrupted', false,
                    'control_interrupt_outcome', 'generation_advanced',
                    'control_interrupt_applied_at', clock_timestamp()::text,
                    'control_ready_at', clock_timestamp()::text
                ),
                status=case
                    when origin->>'control_kind'='scoped_subagent_interrupt'
                    then 'consumed'
                    else status
                end,
                updated_at=now()
            where session_id=$1
              and id=$2
              and status='queued'
              and priority='steer'
              and origin->>'control_kind' in (
                  'scoped_subagent_steer',
                  'scoped_subagent_interrupt'
              )
              and origin->>'control_phase'='pending_interrupt'
            "#,
        )
        .bind(subagent_id)
        .bind(input_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            anyhow::bail!(
                "stale subagent control phase update affected {updated} rows for {input_id}"
            );
        }
        bump_revisions_tx(&mut tx, subagent_id, true, false).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Apply a pending interrupt without synthesizing transcript output when
    /// the captured child generation is already hosted at a durable boundary.
    ///
    /// The child session is locked before its control queue row, matching
    /// ordinary output persistence. The complete unfinished action generation
    /// and the phase update commit atomically. `interrupt_applied` remains a
    /// durable task-abort checkpoint; the daemon advances it to `ready` only
    /// after volatile task ownership has been cancelled.
    pub async fn apply_subagent_control_interrupt_at_boundary(
        &self,
        subagent_id: &str,
        input_id: &str,
    ) -> Result<SubagentBoundaryInterruptResult> {
        let mut tx = self.pool.begin().await?;
        lock_session_tx(&mut tx, subagent_id).await?;
        let active_leaf_id: Option<String> =
            sqlx::query_scalar("select active_leaf_id from sessions where id=$1")
                .bind(subagent_id)
                .fetch_one(&mut *tx)
                .await?;
        let at_boundary = match active_leaf_id.as_deref() {
            None => true,
            Some(active_leaf_id) => {
                sqlx::query_scalar::<_, bool>(
                    r#"
                    select exists(
                        select 1
                        from transcript_entries
                        where session_id=$1
                          and id=$2
                          and item->>'type' in ('turn_finished', 'compaction_summary')
                    )
                    "#,
                )
                .bind(subagent_id)
                .bind(active_leaf_id)
                .fetch_one(&mut *tx)
                .await?
            }
        };
        if !at_boundary {
            tx.commit().await?;
            return Ok(SubagentBoundaryInterruptResult::NotAtBoundary);
        }

        let row = sqlx::query(
            r#"
            select origin
            from queued_inputs
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
            for update
            "#,
        )
        .bind(subagent_id)
        .bind(input_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            anyhow::bail!(
                "subagent boundary interrupt phase update found no pending row for {input_id}"
            );
        };
        let origin: Value = row.get("origin");
        let target_active_leaf_id = origin
            .get("target_active_leaf_id")
            .and_then(Value::as_str)
            .map(str::to_string);
        let target_turn_id = origin.get("target_turn_id").and_then(Value::as_i64);
        let target_attempt_ids = control_target_action_attempt_ids(&origin)?;
        let unfinished_rows = sqlx::query(
            r#"
            select attempt_id, turn_id
            from actions
            where session_id=$1
              and status in ('pending', 'blocked', 'running')
            order by created_at, id
            for update
            "#,
        )
        .bind(subagent_id)
        .fetch_all(&mut *tx)
        .await?;
        let target_is_current = if target_attempt_ids.is_empty() {
            active_leaf_id == target_active_leaf_id && unfinished_rows.is_empty()
        } else {
            !unfinished_rows.is_empty()
                && unfinished_rows.iter().all(|action| {
                    let attempt_id: String = action.get("attempt_id");
                    let turn_id: Option<i64> = action.get("turn_id");
                    turn_id == target_turn_id && target_attempt_ids.contains(&attempt_id)
                })
        };
        if !target_is_current {
            settle_stale_subagent_control_tx(&mut tx, subagent_id, input_id).await?;
            bump_revisions_tx(&mut tx, subagent_id, true, false).await?;
            tx.commit().await?;
            return Ok(SubagentBoundaryInterruptResult::GenerationAdvanced);
        }

        let interrupted = if target_attempt_ids.is_empty() {
            false
        } else {
            sqlx::query(
                r#"
                update actions
                set status='interrupted',
                    result=jsonb_build_object(
                        'reason', 'subagent control at turn boundary',
                        'control_input_id', $2::text
                    ),
                    updated_at=now()
                where session_id=$1
                  and status in ('pending', 'blocked', 'running')
                  and turn_id is not distinct from $3::bigint
                  and attempt_id=any($4::text[])
                "#,
            )
            .bind(subagent_id)
            .bind(input_id)
            .bind(target_turn_id)
            .bind(&target_attempt_ids)
            .execute(&mut *tx)
            .await?
            .rows_affected()
                > 0
        };
        let outcome = if interrupted {
            "interrupted"
        } else {
            "already_between_turns"
        };
        let updated = sqlx::query(
            r#"
            update queued_inputs
            set origin=origin || jsonb_build_object(
                    'control_phase', 'interrupt_applied',
                    'control_interrupted', $3::boolean,
                    'control_interrupt_outcome', $4::text,
                    'control_interrupt_applied_at', clock_timestamp()::text
                ),
                updated_at=now()
            where session_id=$1
              and id=$2
              and status='queued'
              and origin->>'control_phase'='pending_interrupt'
            "#,
        )
        .bind(subagent_id)
        .bind(input_id)
        .bind(interrupted)
        .bind(outcome)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated != 1 {
            anyhow::bail!(
                "subagent boundary interrupt phase update affected {updated} rows for {input_id}"
            );
        }
        bump_revisions_tx(&mut tx, subagent_id, true, false).await?;
        tx.commit().await?;
        Ok(SubagentBoundaryInterruptResult::Applied { interrupted })
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
            // The subagent's task prompt is persisted at spawn and carried in
            // delegation.list so inspection can expose its handoff file.
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

    /// Completed delegations missing their deterministic publication row. The
    /// normal barrier claims terminal status before writing files and enqueueing
    /// the parent observation; a crash in that narrow gap leaves no
    /// `delegation-steer:{delegation_id}:{attempt_id}` input. Starting from the
    /// missing queue row keeps boot work proportional to actual crash gaps
    /// instead of replaying every historical terminal delegation.
    pub async fn list_completed_delegations_for_repair(&self) -> Result<Vec<Delegation>> {
        let rows = sqlx::query(
            r#"
            select d.id,
                   d.parent_session_id,
                   d.workflow,
                   d.label,
                   d.kind,
                   d.status,
                   d.attempt_id,
                   d.expected_subagents
            from delegations d
            where d.status in ('done', 'done_with_failures')
              and not exists (
                  select 1
                  from queued_inputs q
                  where q.session_id=d.parent_session_id
                    and q.client_input_id=
                        'delegation-steer:' || d.id || ':' || d.attempt_id
              )
            order by d.updated_at, d.id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_delegation).collect()
    }
}

async fn cancel_active_partial_delegation_wakeups_tx(
    tx: &mut Transaction<'_, Postgres>,
    parent_session_id: &str,
    delegation_id: &str,
    attempt_id: &str,
    reason: &str,
) -> Result<Vec<String>> {
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
            and status in ('queued', 'consuming')
            and content->>'type' = 'daemon_tool_observation'
            and left(client_input_id, char_length($2)) = $2
        returning id
        "#,
    )
    .bind(parent_session_id)
    .bind(&prefix)
    .bind(reason)
    .fetch_all(&mut **tx)
    .await?;
    Ok(input_ids)
}

async fn partial_wakeup_cancellation_events_tx(
    tx: &mut Transaction<'_, Postgres>,
    parent_session_id: &str,
    delegation_id: &str,
    attempt_id: &str,
    reason: &str,
    input_ids: Vec<String>,
) -> Result<Vec<EventFrame>> {
    if input_ids.is_empty() {
        return Ok(Vec::new());
    }
    bump_revisions_tx(tx, parent_session_id, true, false).await?;
    let queue = queue_state_tx(tx, parent_session_id).await?;
    let event = insert_event_tx(
        tx,
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
    Ok(vec![event])
}

async fn enqueue_steer_content_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    parent_session_id: &str,
    content: QueuedInputContent,
    client_input_id: &str,
) -> Result<bool> {
    lock_session_tx(tx, parent_session_id).await?;
    let provider_config: Value = sqlx::query_scalar(
        r#"
        select coalesce(
            (
                select provider_config
                from actions
                where session_id=$1
                order by created_at desc, id desc
                limit 1
            ),
            provider_config
        )
        from sessions
        where id=$1
        "#,
    )
    .bind(parent_session_id)
    .fetch_one(&mut **tx)
    .await?;
    let id = format!("input_{}", Uuid::new_v4());
    let inserted = sqlx::query(
        r#"
            insert into queued_inputs (
                id, session_id, priority, content, status, client_input_id, origin,
                provider_config
            )
            values (
                $1, $2, 'steer', $3, 'queued', $4,
                jsonb_build_object('promoted_at', clock_timestamp()::text),
                $5
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
    .bind(provider_config)
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

fn control_record_from_row(row: &PgRow, origin: &Value) -> Result<SubagentControlRecord> {
    let phase = origin
        .get("control_phase")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("scoped subagent control is missing its phase"))?
        .parse::<SubagentControlPhase>()
        .map_err(anyhow::Error::msg)?;
    let kind = origin
        .get("control_kind")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("scoped subagent control is missing its kind"))?
        .parse::<SubagentControlKind>()
        .map_err(anyhow::Error::msg)?;
    let target_action_attempt_ids = control_target_action_attempt_ids(origin)?;
    Ok(SubagentControlRecord {
        input_id: row.get("id"),
        status: super::rows::row_text::<QueuedInputStatus>(row, "status")?,
        kind,
        phase,
        interrupt: origin
            .get("control_interrupt")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        interrupted: origin
            .get("control_interrupted")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        interrupt_outcome: origin
            .get("control_interrupt_outcome")
            .and_then(Value::as_str)
            .map(str::to_string),
        target_active_leaf_id: origin
            .get("target_active_leaf_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        target_turn_id: origin.get("target_turn_id").and_then(Value::as_i64),
        target_action_attempt_ids,
        delegation_running: row.get::<String, _>("delegation_status")
            == DelegationStatus::Running.as_str(),
    })
}

fn control_target_action_attempt_ids(origin: &Value) -> Result<Vec<String>> {
    if let Some(attempt_ids) = origin
        .get("target_action_attempt_ids")
        .and_then(Value::as_array)
    {
        attempt_ids
            .iter()
            .map(|attempt_id| {
                attempt_id
                    .as_str()
                    .map(str::to_string)
                    .ok_or_else(|| anyhow::anyhow!("control attempt id must be a string"))
            })
            .collect::<Result<Vec<_>>>()
    } else {
        Ok(origin
            .get("target_action_attempt_id")
            .and_then(Value::as_str)
            .map(|attempt_id| vec![attempt_id.to_string()])
            .unwrap_or_default())
    }
}

async fn settle_stale_subagent_control_tx(
    tx: &mut Transaction<'_, Postgres>,
    subagent_id: &str,
    input_id: &str,
) -> Result<()> {
    let updated = sqlx::query(
        r#"
        update queued_inputs
        set origin=origin || jsonb_build_object(
                'control_phase', 'ready',
                'control_interrupted', false,
                'control_interrupt_outcome', 'generation_advanced',
                'control_interrupt_applied_at', clock_timestamp()::text,
                'control_ready_at', clock_timestamp()::text
            ),
            status=case
                when origin->>'control_kind'='scoped_subagent_interrupt'
                then 'consumed'
                else status
            end,
            updated_at=now()
        where session_id=$1
          and id=$2
          and status='queued'
          and priority='steer'
          and origin->>'control_kind' in (
              'scoped_subagent_steer',
              'scoped_subagent_interrupt'
          )
          and origin->>'control_phase'='pending_interrupt'
        "#,
    )
    .bind(subagent_id)
    .bind(input_id)
    .execute(&mut **tx)
    .await?
    .rows_affected();
    if updated != 1 {
        anyhow::bail!("stale subagent control phase update affected {updated} rows for {input_id}");
    }
    Ok(())
}

#[cfg(test)]
#[path = "delegations_tests.rs"]
mod tests;
