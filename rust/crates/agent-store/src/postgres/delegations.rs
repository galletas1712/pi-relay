use agent_vocab::{TranscriptItem, UserMessage};
use anyhow::Result;
use serde_json::{json, Value};
use sqlx::{postgres::PgRow, Row};
use uuid::Uuid;

use super::events::insert_event_tx;
use super::queue::bump_revisions_tx;
use super::sql::lock_session_tx;
use super::PostgresAgentStore;
use crate::{
    DelegationKind, DelegationStatus, EventType, InputPriority, SessionActivity, SubagentType,
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

    /// All delegations of a parent, oldest first. Backs the per-parent
    /// `delegation.list` the run board needs (the spec only defines per-id
    /// status).
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

    /// Current/recent delegations for compact model-context recovery.
    ///
    /// There is no durable parent-acknowledgement state today, so this returns
    /// every running delegation plus a bounded window of the most recently
    /// updated terminal delegations. The running set is always included even if
    /// it exceeds the terminal window.
    pub async fn list_parent_current_delegations(
        &self,
        parent_session_id: &str,
        recent_terminal_limit: i64,
    ) -> Result<Vec<Delegation>> {
        let terminal_limit = recent_terminal_limit.max(0);
        let rows = sqlx::query(
            r#"
            with current_delegations as (
                select id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents, updated_at, created_at, 0 as group_order
                from delegations
                where parent_session_id=$1 and status='running'
                union all
                select id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents, updated_at, created_at, 1 as group_order
                from (
                    select id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents, updated_at, created_at
                    from delegations
                    where parent_session_id=$1 and status <> 'running'
                    order by updated_at desc, id desc
                    limit $2
                ) recent_terminal
            )
            select id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents
            from current_delegations
            order by group_order, updated_at desc, created_at desc, id desc
            "#,
        )
        .bind(parent_session_id)
        .bind(terminal_limit)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_delegation).collect()
    }

    /// Compute compact progress counts for a delegation without materializing
    /// active branches. Terminality comes from each subagent's active leaf; a
    /// `TurnFinished` leaf with `Graceful` is a terminal success, other
    /// `TurnFinished` outcomes are terminal failures, and a compaction summary
    /// is terminal success.
    pub async fn delegation_progress(&self, delegation: &Delegation) -> Result<DelegationProgress> {
        let rows = sqlx::query(
            r#"
            select s.id, s.active_leaf_id, te.item
            from sessions s
            left join transcript_entries te
                on te.session_id = s.id
               and te.id = s.active_leaf_id
            where s.delegation_id=$1
            "#,
        )
        .bind(&delegation.id)
        .fetch_all(&self.pool)
        .await?;

        let mut terminal = 0i32;
        let mut failed = 0i32;
        for row in &rows {
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

    /// Claim the delegation barrier's terminal status transition.
    ///
    /// This is an attempt-fenced `running -> done|done_with_failures` CAS. The
    /// CAS must run before normal handoff publishing so a concurrent
    /// `delegation.cancel` cannot win and then receive a normal completed
    /// handoff. The parent steer is intentionally NOT enqueued here: the runner
    /// publishes handoff files first, then enqueues the deterministic steer, so
    /// the parent is never pointed at missing files during normal operation.
    ///
    /// A crash after this CAS but before file/steer publication leaves a
    /// terminal delegation with no steer. The daemon boot sweep repairs that by
    /// re-rendering terminal handoffs and idempotently enqueueing any missing
    /// deterministic steer for completed delegations.
    pub async fn finish_delegation(
        &self,
        delegation_id: &str,
        attempt_id: &str,
        status: DelegationStatus,
    ) -> Result<bool> {
        let updated = sqlx::query(
            "update delegations set status=$3, updated_at=now() where id=$1 and attempt_id=$2 and status='running'",
        )
        .bind(delegation_id)
        .bind(attempt_id)
        .bind(status.as_str())
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(updated == 1)
    }

    /// Enqueue the parent's delegation-completion steer with the deterministic
    /// delegation/attempt key. This is idempotent via the unique
    /// `(session_id, client_input_id)` index, so boot repair or a replay can call
    /// it again without creating a duplicate. The runner calls this only after
    /// normal handoff files exist, so the parent steer never races ahead of the
    /// files it references.
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

    /// Whether every subagent of a delegation is terminal. Two fences guard
    /// against a premature completion:
    ///
    /// 1. Expected-count fence: the delegation must have spawned its FULL set of
    ///    subagents. A fan-out spawns its children in a loop while each child
    ///    drives in a detached task, so subagent #1 can reach terminal before #2
    ///    is even inserted. Requiring `count(sessions where delegation_id) ==
    ///    expected_subagents` keeps the barrier closed during that window.
    ///
    /// 2. Transcript-boundary terminality: a subagent is terminal only when its
    ///    active leaf is a genuine turn boundary (`TurnFinished` / compaction
    ///    summary). This is independent of action/queue status — so a subagent
    ///    that crashed MID-TURN (boot's `mark_all_unfinished_actions_stale`
    ///    erased its unfinished action, and it had no queued input) is correctly
    ///    NON-terminal and stays in the delegation until it is recovered to a
    ///    boundary (where it either continues or settles as a genuine terminal
    ///    outcome).
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

    async fn list_running_delegations(&self) -> Result<Vec<Delegation>> {
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
    /// the parent steer; if the daemon crashes in that narrow gap, these rows
    /// are no longer `running` and therefore are not covered by the ordinary
    /// running-delegation sweep.
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

/// Insert the parent's delegation-completion steer as a durable queued input inside
/// the caller's transaction, idempotent on `(session_id, client_input_id)`. A
/// re-run with the same key (replay/boot sweep) inserts nothing and emits no
/// duplicate event. Mirrors the steer branch of `enqueue_user_input`.
async fn enqueue_steer_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    parent_session_id: &str,
    message: &str,
    client_input_id: &str,
) -> Result<()> {
    lock_session_tx(tx, parent_session_id).await?;
    let content = UserMessage::text(message);
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
        return Ok(());
    };
    bump_revisions_tx(tx, parent_session_id, true, false).await?;
    let input_id: String = inserted.get("id");
    insert_event_tx(
        tx,
        parent_session_id,
        EventType::InputQueued,
        json!({
            "input_id": input_id,
            "priority": InputPriority::Steer,
            "client_input_id": client_input_id,
            "content": content.content.clone(),
        }),
    )
    .await?;
    Ok(())
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
