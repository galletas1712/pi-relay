use anyhow::{anyhow, Result};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};

use crate::{
    PreparedWorkspace, SessionWorkspace, WorkspaceAttachment, WorkspaceCleanupMode,
    WorkspaceOwnerKind, WorkspaceResource, WorkspaceResourceState,
};

use super::PostgresAgentStore;

const CLEANUP_RETRY_SECS: f64 = 5.0;
const CLEANUP_CLAIM_SECS: f64 = 30.0;

fn workspace_resource_from_row(row: sqlx::postgres::PgRow) -> Result<WorkspaceResource> {
    Ok(WorkspaceResource {
        owner_session_id: row.get("owner_session_id"),
        runtime_id: row.get("runtime_id"),
        workspace_id: row.get("workspace_id"),
        generation: row.get("generation"),
        owner_kind: row
            .get::<String, _>("owner_kind")
            .parse()
            .map_err(|error: String| anyhow!(error))?,
        state: row
            .get::<String, _>("state")
            .parse()
            .map_err(|error: String| anyhow!(error))?,
        cleanup_mode: row
            .get::<Option<String>, _>("cleanup_mode")
            .map(|raw| raw.parse().map_err(|error: String| anyhow!(error)))
            .transpose()?,
        workspaces: row
            .get::<Option<Value>, _>("workspaces")
            .map(serde_json::from_value)
            .transpose()?,
    })
}

impl PostgresAgentStore {
    /// Persist a private workspace identity before asking a runtime to create it.
    pub async fn begin_workspace_provisioning(
        &self,
        owner_session_id: &str,
        runtime_id: &str,
        workspace_id: &str,
        generation: &str,
        owner_kind: WorkspaceOwnerKind,
        lease_secs: i64,
    ) -> Result<()> {
        let inserted = sqlx::query(
            r#"
            insert into workspace_resources (
                workspace_id, owner_session_id, runtime_id, generation,
                owner_kind, state, lease_expires_at
            )
            values ($1, $2, $3, $4, $5, 'provisioning',
                    now() + make_interval(secs => $6))
            on conflict do nothing
            "#,
        )
        .bind(workspace_id)
        .bind(owner_session_id)
        .bind(runtime_id)
        .bind(generation)
        .bind(owner_kind.as_str())
        .bind(lease_secs as f64)
        .execute(&self.pool)
        .await?;
        if inserted.rows_affected() == 1 {
            return Ok(());
        }
        let exact: bool = sqlx::query_scalar(
            r#"
            select exists(
                select 1 from workspace_resources
                where workspace_id=$1 and owner_session_id=$2 and runtime_id=$3
                  and generation=$4 and owner_kind=$5 and state='provisioning'
            )
            "#,
        )
        .bind(workspace_id)
        .bind(owner_session_id)
        .bind(runtime_id)
        .bind(generation)
        .bind(owner_kind.as_str())
        .fetch_one(&self.pool)
        .await?;
        if exact {
            Ok(())
        } else {
            Err(anyhow!(
                "private workspace identity conflicts with an existing lifecycle"
            ))
        }
    }

    /// Record the exact materialization result. Session creation consumes this
    /// ready generation atomically with its session insert.
    pub async fn finish_workspace_provisioning(
        &self,
        owner_session_id: &str,
        runtime_id: &str,
        workspace_id: &str,
        generation: &str,
        workspaces: &[SessionWorkspace],
    ) -> Result<PreparedWorkspace> {
        let row = sqlx::query(
            r#"
            update workspace_resources
            set state='ready', workspaces=$5, last_error=null,
                retry_at=now(), updated_at=now()
            where owner_session_id=$1 and runtime_id=$2 and workspace_id=$3
              and generation=$4 and state='provisioning'
            returning owner_session_id, runtime_id, workspace_id, generation,
                      owner_kind, state, cleanup_mode, workspaces
            "#,
        )
        .bind(owner_session_id)
        .bind(runtime_id)
        .bind(workspace_id)
        .bind(generation)
        .bind(serde_json::to_value(workspaces)?)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| anyhow!("workspace provisioning generation is no longer current"))?;
        let resource = workspace_resource_from_row(row)?;
        Ok(PreparedWorkspace {
            owner_session_id: resource.owner_session_id,
            runtime_id: resource.runtime_id,
            workspace_id: resource.workspace_id,
            generation: resource.generation,
            owner_kind: resource.owner_kind,
            workspaces: resource
                .workspaces
                .expect("ready workspace resource has materialized workspaces"),
        })
    }

    /// Persist cleanup intent for an exact prepared generation. This is safe to
    /// call whether the remote create failed before or after taking effect.
    pub async fn abandon_prepared_workspace(&self, workspace: &PreparedWorkspace) -> Result<bool> {
        self.request_workspace_cleanup_exact(
            &workspace.owner_session_id,
            &workspace.runtime_id,
            &workspace.workspace_id,
            &workspace.generation,
            WorkspaceCleanupMode::DeleteSession,
        )
        .await
    }

    pub async fn request_workspace_cleanup_exact(
        &self,
        owner_session_id: &str,
        runtime_id: &str,
        workspace_id: &str,
        generation: &str,
        cleanup_mode: WorkspaceCleanupMode,
    ) -> Result<bool> {
        let updated = sqlx::query(
            r#"
            update workspace_resources
            set state='deleting',
                cleanup_mode=case
                    when cleanup_mode='delete_session' or $5='delete_session'
                        then 'delete_session'
                    else 'retain_session'
                end,
                retry_at=now(), last_error=null, updated_at=now()
            where owner_session_id=$1 and runtime_id=$2 and workspace_id=$3
              and generation=$4
              and state in ('provisioning','ready','deleting')
            "#,
        )
        .bind(owner_session_id)
        .bind(runtime_id)
        .bind(workspace_id)
        .bind(generation)
        .bind(cleanup_mode.as_str())
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Request physical cleanup while preserving the session identity until an
    /// exact runtime destroy succeeds. Returns false when the session has no
    /// private workspace (for example a full subagent).
    pub async fn request_session_workspace_cleanup(
        &self,
        session_id: &str,
        cleanup_mode: WorkspaceCleanupMode,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            r#"
            select r.state
            from workspace_resources r
            join sessions s on s.id=r.owner_session_id
            where r.owner_session_id=$1 and s.runtime_id=r.runtime_id
              and s.workspace_id=r.workspace_id
            for update of r, s
            "#,
        )
        .bind(session_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.commit().await?;
            return Ok(false);
        };
        let state: WorkspaceResourceState = row
            .get::<String, _>("state")
            .parse()
            .map_err(|error: String| anyhow!(error))?;
        if state == WorkspaceResourceState::Deleted {
            if cleanup_mode == WorkspaceCleanupMode::DeleteSession {
                sqlx::query("delete from sessions where id=$1")
                    .bind(session_id)
                    .execute(&mut *tx)
                    .await?;
                sqlx::query(
                    "delete from workspace_resources
                     where owner_session_id=$1 and state='deleted'",
                )
                .bind(session_id)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            return Ok(true);
        }
        let updated = sqlx::query(
            r#"
            update workspace_resources r
            set state='deleting',
                cleanup_mode=case
                    when r.cleanup_mode='delete_session' or $2='delete_session'
                        then 'delete_session'
                    else 'retain_session'
                end,
                retry_at=now(), last_error=null, updated_at=now()
            from sessions s
            where r.owner_session_id=$1 and s.id=$1
              and s.runtime_id=r.runtime_id and s.workspace_id=r.workspace_id
              and r.state in ('provisioning','ready','deleting')
            "#,
        )
        .bind(session_id)
        .bind(cleanup_mode.as_str())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(updated.rows_affected() == 1)
    }

    /// Adopt a committed exact owner immediately, and expire only unattached
    /// preparations whose bounded lease elapsed. Safe to run periodically.
    pub async fn reconcile_workspace_resources(&self) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        sqlx::query(
            r#"
            update workspace_resources r
            set state='ready', attached_at=coalesce(r.attached_at, now()),
                updated_at=now(), last_error=null
            from sessions s
            where r.owner_session_id=s.id and r.runtime_id=s.runtime_id
              and r.workspace_id=s.workspace_id and r.workspaces=s.workspaces
              and r.workspaces is not null
              and r.state in ('provisioning','ready')
              and (
                  (r.owner_kind='read_only' and s.subagent_type='read_only')
                  or (r.owner_kind='history_fork' and s.parent_session_id is null
                      and s.subagent_type is null and s.metadata ? 'fork')
                  or (r.owner_kind='root' and s.parent_session_id is null
                      and s.subagent_type is null and not (s.metadata ? 'fork'))
              )
            "#,
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            r#"
            update workspace_resources r
            set state='deleting', cleanup_mode='delete_session',
                retry_at=now(), updated_at=now(),
                last_error='workspace provisioning lease expired before session attachment'
            where r.state in ('provisioning','ready')
              and r.attached_at is null and r.lease_expires_at <= now()
              and not exists (
                  select 1 from sessions s
                  where s.id=r.owner_session_id and s.runtime_id=r.runtime_id
                    and s.workspace_id=r.workspace_id
              )
            "#,
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Lease due deletion attempts so periodic and Hello-triggered sweeps do
    /// not issue the same command concurrently.
    pub async fn claim_due_workspace_deletions(
        &self,
        runtime_id: Option<&str>,
    ) -> Result<Vec<WorkspaceResource>> {
        let rows = sqlx::query(
            r#"
            update workspace_resources
            set retry_at=now() + make_interval(secs => $2), updated_at=now()
            where workspace_id in (
                select workspace_id
                from workspace_resources
                where state='deleting' and retry_at <= now()
                  and ($1::text is null or runtime_id=$1)
                order by retry_at, created_at
                for update skip locked
                limit 32
            )
            returning owner_session_id, runtime_id, workspace_id, generation,
                      owner_kind, state, cleanup_mode, workspaces
            "#,
        )
        .bind(runtime_id)
        .bind(CLEANUP_CLAIM_SECS)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(workspace_resource_from_row).collect()
    }

    pub async fn record_workspace_cleanup_failure(
        &self,
        resource: &WorkspaceResource,
        error: &str,
    ) -> Result<()> {
        sqlx::query(
            r#"
            update workspace_resources
            set retry_at=now() + make_interval(secs => $5),
                last_error=$6, updated_at=now()
            where owner_session_id=$1 and runtime_id=$2 and workspace_id=$3
              and generation=$4 and state='deleting'
            "#,
        )
        .bind(&resource.owner_session_id)
        .bind(&resource.runtime_id)
        .bind(&resource.workspace_id)
        .bind(&resource.generation)
        .bind(CLEANUP_RETRY_SECS)
        .bind(error)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Complete the exact generation after the runtime confirmed destruction.
    /// Delete-session cleanup retains identity until this transaction.
    pub async fn complete_workspace_cleanup(&self, resource: &WorkspaceResource) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let cleanup_mode: Option<String> = sqlx::query_scalar(
            r#"
            select cleanup_mode from workspace_resources
            where owner_session_id=$1 and runtime_id=$2 and workspace_id=$3
              and generation=$4 and state='deleting'
            for update
            "#,
        )
        .bind(&resource.owner_session_id)
        .bind(&resource.runtime_id)
        .bind(&resource.workspace_id)
        .bind(&resource.generation)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        let Some(cleanup_mode) = cleanup_mode else {
            tx.rollback().await?;
            return Ok(false);
        };
        let cleanup_mode: WorkspaceCleanupMode = cleanup_mode
            .parse()
            .map_err(|error: String| anyhow!(error))?;
        if cleanup_mode == WorkspaceCleanupMode::DeleteSession {
            sqlx::query(
                r#"
                delete from sessions
                where id=$1 and runtime_id=$2 and workspace_id=$3
                "#,
            )
            .bind(&resource.owner_session_id)
            .bind(&resource.runtime_id)
            .bind(&resource.workspace_id)
            .execute(&mut *tx)
            .await?;
            let conflicting_owner_exists: bool =
                sqlx::query_scalar("select exists(select 1 from sessions where id=$1)")
                    .bind(&resource.owner_session_id)
                    .fetch_one(&mut *tx)
                    .await?;
            if conflicting_owner_exists {
                return Err(anyhow!(
                    "workspace owner identity changed before cleanup completion"
                ));
            }
            let deleted = sqlx::query(
                r#"
                delete from workspace_resources
                where owner_session_id=$1 and runtime_id=$2 and workspace_id=$3
                  and generation=$4 and state='deleting'
                "#,
            )
            .bind(&resource.owner_session_id)
            .bind(&resource.runtime_id)
            .bind(&resource.workspace_id)
            .bind(&resource.generation)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Ok(deleted.rows_affected() == 1);
        }
        let retained = sqlx::query(
            r#"
            update workspace_resources
            set state='deleted', cleanup_mode='retain_session',
                retry_at=now(), last_error=null, updated_at=now()
            where owner_session_id=$1 and runtime_id=$2 and workspace_id=$3
              and generation=$4 and state='deleting'
            "#,
        )
        .bind(&resource.owner_session_id)
        .bind(&resource.runtime_id)
        .bind(&resource.workspace_id)
        .bind(&resource.generation)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(retained.rows_affected() == 1)
    }

    pub async fn workspace_resource_for_session(
        &self,
        session_id: &str,
    ) -> Result<Option<WorkspaceResource>> {
        sqlx::query(
            r#"
            select owner_session_id, runtime_id, workspace_id, generation,
                   owner_kind, state, cleanup_mode, workspaces
            from workspace_resources where owner_session_id=$1
            "#,
        )
        .bind(session_id)
        .fetch_optional(&self.pool)
        .await?
        .map(workspace_resource_from_row)
        .transpose()
    }
}

pub(super) async fn attach_workspace_resource_tx(
    tx: &mut Transaction<'_, Postgres>,
    owner_session_id: &str,
    runtime_id: &str,
    config_workspace_id: &str,
    config_workspaces: &[SessionWorkspace],
    attachment: WorkspaceAttachment<'_>,
) -> Result<()> {
    if attachment.workspace_id != config_workspace_id {
        return Err(anyhow!(
            "prepared workspace does not match the session workspace"
        ));
    }
    let attached = sqlx::query(
        r#"
        update workspace_resources
        set attached_at=coalesce(attached_at, now()), updated_at=now()
        where owner_session_id=$1 and runtime_id=$2 and workspace_id=$3
          and generation=$4 and owner_kind=$5 and state='ready'
          and workspaces=$6
        "#,
    )
    .bind(owner_session_id)
    .bind(runtime_id)
    .bind(attachment.workspace_id)
    .bind(attachment.generation)
    .bind(attachment.owner_kind.as_str())
    .bind(serde_json::to_value(config_workspaces)?)
    .execute(&mut **tx)
    .await?;
    if attached.rows_affected() != 1 {
        return Err(anyhow!(
            "prepared workspace generation is missing or does not match the session"
        ));
    }
    Ok(())
}
