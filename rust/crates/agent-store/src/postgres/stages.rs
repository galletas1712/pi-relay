use anyhow::Result;
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

use super::PostgresAgentStore;
use crate::{SessionActivity, StageKind, StageStatus, SubagentType};

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
}

/// A subagent session belonging to a stage, with the fields `stage.status`
/// needs to report per-subagent state.
#[derive(Debug, Clone)]
pub struct StageSubagent {
    pub session_id: String,
    pub activity: SessionActivity,
    pub subagent_type: Option<SubagentType>,
    pub role: Option<String>,
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
    ) -> Result<Stage> {
        let id = format!("stage_{}", Uuid::new_v4());
        let attempt_id = Uuid::new_v4().to_string();
        sqlx::query(
            r#"
            insert into stages (id, parent_session_id, workflow, label, kind, status, attempt_id)
            values ($1, $2, $3, $4, $5, $6, $7)
            "#,
        )
        .bind(&id)
        .bind(parent_session_id)
        .bind(workflow)
        .bind(label)
        .bind(kind.as_str())
        .bind(StageStatus::Running.as_str())
        .bind(&attempt_id)
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
        })
    }

    pub async fn get_stage(&self, stage_id: &str) -> Result<Option<Stage>> {
        let Some(row) = sqlx::query(
            r#"
            select id, parent_session_id, workflow, label, kind, status, attempt_id
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
        let kind: String = row.get("kind");
        let status: String = row.get("status");
        Ok(Some(Stage {
            id: row.get("id"),
            parent_session_id: row.get("parent_session_id"),
            workflow: row.get("workflow"),
            label: row.get("label"),
            kind: kind.parse().map_err(anyhow::Error::msg)?,
            status: status.parse().map_err(anyhow::Error::msg)?,
            attempt_id: row.get("attempt_id"),
        }))
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
            let activity = self.activity(&session_id).await?;
            subagents.push(StageSubagent {
                session_id,
                activity,
                subagent_type,
                role,
            });
        }
        Ok(subagents)
    }

    /// All stages of a parent, oldest first. Backs the per-parent `stage.list`
    /// the run board needs (the spec only defines per-id status).
    pub async fn list_parent_stages(&self, parent_session_id: &str) -> Result<Vec<Stage>> {
        let rows = sqlx::query(
            r#"
            select id, parent_session_id, workflow, label, kind, status, attempt_id
            from stages
            where parent_session_id=$1
            order by created_at, id
            "#,
        )
        .bind(parent_session_id)
        .fetch_all(&self.pool)
        .await?;
        let mut stages = Vec::with_capacity(rows.len());
        for row in rows {
            let kind: String = row.get("kind");
            let status: String = row.get("status");
            stages.push(Stage {
                id: row.get("id"),
                parent_session_id: row.get("parent_session_id"),
                workflow: row.get("workflow"),
                label: row.get("label"),
                kind: kind.parse().map_err(anyhow::Error::msg)?,
                status: status.parse().map_err(anyhow::Error::msg)?,
                attempt_id: row.get("attempt_id"),
            });
        }
        Ok(stages)
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
    /// attempt-fenced completion CAS lands in Phase 3.
    pub async fn set_stage_status(&self, stage_id: &str, status: StageStatus) -> Result<()> {
        sqlx::query("update stages set status=$2, updated_at=now() where id=$1")
            .bind(stage_id)
            .bind(status.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
#[path = "stages_tests.rs"]
mod tests;
