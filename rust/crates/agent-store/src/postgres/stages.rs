use agent_vocab::UserMessage;
use anyhow::Result;
use serde_json::{json, Value};
use sqlx::Row;
use uuid::Uuid;

use super::events::insert_event_tx;
use super::queue::bump_revisions_tx;
use super::sql::lock_session_tx;
use super::PostgresAgentStore;
use crate::{EventType, InputPriority, SessionActivity, StageKind, StageStatus, SubagentType};

/// A durable stage row: an ordered unit of work under a parent session that is
/// either one full subagent or a fan-out of read-only subagents.
#[derive(Debug, Clone)]
pub struct Stage {
    pub id: String,
    pub parent_session_id: String,
    pub workflow: Option<String>,
    pub label: Option<String>,
    pub kind: StageKind,
    pub status: StageStatus,
    pub attempt_id: String,
    /// The full subagent set this stage will spawn (1 for a full stage,
    /// `tasks.len()` for a fan-out). The barrier never completes until exactly
    /// this many subagents exist and are all terminal.
    pub expected_subagents: i32,
}

/// A subagent session belonging to a stage, with the fields `stage.status`
/// needs to report per-subagent state.
#[derive(Debug, Clone)]
pub struct StageSubagent {
    pub session_id: String,
    pub activity: SessionActivity,
    pub subagent_type: Option<SubagentType>,
    pub role: Option<String>,
    pub task: Option<String>,
}

impl PostgresAgentStore {
    /// Insert a fresh `running` stage, minting its completion-fencing attempt id.
    /// The stage row is created before its subagents so their `stage_id` FK holds.
    pub async fn create_stage(
        &self,
        parent_session_id: &str,
        kind: StageKind,
        workflow: Option<&str>,
        label: Option<&str>,
        expected_subagents: i32,
    ) -> Result<Stage> {
        let id = format!("stage_{}", Uuid::new_v4());
        let attempt_id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            insert into stages (id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents)
            values ($1, $2, $3, $4, $5, $6, $7, $8)
            "#,
        )
        .bind(&id)
        .bind(parent_session_id)
        .bind(workflow)
        .bind(label)
        .bind(kind.as_str())
        .bind(StageStatus::Running.as_str())
        .bind(&attempt_id)
        .bind(expected_subagents)
        .execute(&self.pool)
        .await?;
        Ok(Stage {
            id,
            parent_session_id: parent_session_id.to_string(),
            workflow: workflow.map(str::to_string),
            label: label.map(str::to_string),
            kind,
            status: StageStatus::Running,
            attempt_id,
            expected_subagents,
        })
    }

    pub async fn get_stage(&self, stage_id: &str) -> Result<Option<Stage>> {
        let Some(row) = sqlx::query(
            r#"
            select id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents
            from stages
            where id=$1
            "#,
        )
        .bind(stage_id)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };
        Ok(Some(row_to_stage(&row)?))
    }

    /// The subagent sessions of a stage, ordered by creation, with the activity
    /// and type `stage.status`/`stage.cancel` need.
    pub async fn list_stage_subagents(&self, stage_id: &str) -> Result<Vec<StageSubagent>> {
        let rows = sqlx::query(
            r#"
            select id, subagent_type, metadata
            from sessions
            where stage_id=$1
            order by created_at, id
            "#,
        )
        .bind(stage_id)
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
            // The subagent's task prompt, persisted at spawn — carried in
            // stage.list so the run board can re-run a stage without the legacy
            // subagent.list surface.
            let task = metadata
                .get("task")
                .and_then(Value::as_str)
                .map(str::to_string);
            let activity = self.activity(&session_id).await?;
            subagents.push(StageSubagent {
                session_id,
                activity,
                subagent_type,
                role,
                task,
            });
        }
        Ok(subagents)
    }

    /// All stages of a parent, oldest first. Backs the per-parent `stage.list`
    /// the run board needs (the spec only defines per-id status).
    pub async fn list_parent_stages(&self, parent_session_id: &str) -> Result<Vec<Stage>> {
        let rows = sqlx::query(
            r#"
            select id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents
            from stages
            where parent_session_id=$1
            order by created_at, id
            "#,
        )
        .bind(parent_session_id)
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_stage).collect()
    }

    /// Whether the parent already owns a `running` stage. Backs the
    /// one-stage-per-parent guard.
    pub async fn parent_has_running_stage(&self, parent_session_id: &str) -> Result<bool> {
        let exists: bool = sqlx::query_scalar(
            r#"
            select exists(
                select 1 from stages
                where parent_session_id=$1 and status=$2
            )
            "#,
        )
        .bind(parent_session_id)
        .bind(StageStatus::Running.as_str())
        .fetch_one(&self.pool)
        .await?;
        Ok(exists)
    }

    /// Mark a stage's status, e.g. when `stage.cancel` cancels it. The barrier's
    /// attempt-fenced completion lives in `finish_stage`.
    pub async fn set_stage_status(&self, stage_id: &str, status: StageStatus) -> Result<()> {
        sqlx::query("update stages set status=$2, updated_at=now() where id=$1")
            .bind(stage_id)
            .bind(status.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// The stage barrier's completion CAS, atomic with the parent steer (FIX B).
    ///
    /// Single-flight via the stage row's `for update` lock; idempotent via the
    /// `status='running'` + `attempt_id` fence. When the CAS wins, the SAME
    /// transaction inserts the parent's steer as a durable `queued_input`
    /// (`InputPriority::Steer`) keyed by a deterministic `client_input_id`
    /// derived from `stage_id`+`attempt_id`. So a commit means the steer is
    /// durably queued: a crash in the old gap between the CAS commit and a
    /// separate enqueue can no longer strand the parent, and a replay/sweep
    /// re-running this with the same key cannot double-enqueue (the unique
    /// `(session_id, client_input_id)` index makes the insert a no-op).
    ///
    /// Returns whether this call won the transition (`rows_affected()==1`), so
    /// exactly one caller renders the handoff and drives the parent. A missing
    /// stage is a benign no-op (a late lifecycle event for a deleted stage).
    pub async fn finish_stage(
        &self,
        stage_id: &str,
        attempt_id: &str,
        status: StageStatus,
        parent_session_id: &str,
        steer_message: &str,
        steer_client_input_id: &str,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let locked = sqlx::query("select id from stages where id=$1 for update")
            .bind(stage_id)
            .fetch_optional(&mut *tx)
            .await?;
        if locked.is_none() {
            tx.commit().await?;
            return Ok(false);
        }
        let updated = sqlx::query(
            "update stages set status=$3, updated_at=now() where id=$1 and attempt_id=$2 and status='running'",
        )
        .bind(stage_id)
        .bind(attempt_id)
        .bind(status.as_str())
        .execute(&mut *tx)
        .await?
        .rows_affected();
        if updated == 1 {
            enqueue_steer_tx(
                &mut tx,
                parent_session_id,
                steer_message,
                steer_client_input_id,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(updated == 1)
    }

    /// Whether every subagent of a stage is terminal. Two fences guard against a
    /// premature completion:
    ///
    /// 1. Expected-count fence (FIX A): the stage must have spawned its FULL set
    ///    of subagents. A fan-out spawns its children in a loop while each child
    ///    drives in a detached task, so subagent #1 can reach terminal before #2
    ///    is even inserted. Requiring `count(sessions where stage_id) ==
    ///    expected_subagents` keeps the barrier closed during that window.
    ///
    /// 2. Transcript-boundary terminality (FIX C): a subagent is terminal only
    ///    when its active leaf is a genuine turn boundary (`TurnFinished` /
    ///    compaction summary). This is independent of action/queue status — so a
    ///    subagent that crashed MID-TURN (boot's `mark_all_unfinished_actions_stale`
    ///    erased its unfinished action, and it had no queued input) is correctly
    ///    NON-terminal and stays in the stage until it is recovered to a boundary
    ///    (where it either continues or settles as a genuine terminal outcome).
    pub async fn stage_subagents_all_terminal(&self, stage_id: &str) -> Result<bool> {
        let session_ids: Vec<String> =
            sqlx::query_scalar("select id from sessions where stage_id=$1")
                .bind(stage_id)
                .fetch_all(&self.pool)
                .await?;
        if (session_ids.len() as i32) != self.stage_expected_subagents(stage_id).await? {
            return Ok(false);
        }
        for session_id in &session_ids {
            if !self.active_leaf_is_turn_boundary(session_id).await? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    async fn stage_expected_subagents(&self, stage_id: &str) -> Result<i32> {
        sqlx::query_scalar("select expected_subagents from stages where id=$1")
            .bind(stage_id)
            .fetch_one(&self.pool)
            .await
            .map_err(Into::into)
    }

    /// Running stages whose subagents are all terminal — the boot-sweep input.
    /// A crash mid-barrier leaves such a stage `running` with every subagent
    /// idle; the sweep re-runs `finish_stage` so it completes exactly once.
    pub async fn sweep_running_stages(&self) -> Result<Vec<Stage>> {
        let running = self.list_running_stages().await?;
        let mut ready = Vec::new();
        for stage in running {
            if self.stage_subagents_all_terminal(&stage.id).await? {
                ready.push(stage);
            }
        }
        Ok(ready)
    }

    async fn list_running_stages(&self) -> Result<Vec<Stage>> {
        let rows = sqlx::query(
            r#"
            select id, parent_session_id, workflow, label, kind, status, attempt_id, expected_subagents
            from stages
            where status='running'
            order by created_at, id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.iter().map(row_to_stage).collect()
    }
}

/// Insert the parent's stage-completion steer as a durable queued input inside
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

fn row_to_stage(row: &sqlx::postgres::PgRow) -> Result<Stage> {
    let kind: String = row.get("kind");
    let status: String = row.get("status");
    Ok(Stage {
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
#[path = "stages_tests.rs"]
mod tests;
