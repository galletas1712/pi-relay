mod action_records;
mod actions;
mod compaction;
mod delegations;
mod events;
mod history_fork;
mod history_target;
mod mcp;
mod outputs;
mod projects;
mod queue;
mod queue_mutations;
mod queue_projection;
mod rows;
mod runtimes;
mod schema;
mod session_links;
mod sessions;
mod snapshots;
mod sql;
mod token_usage;
mod transcript;
mod turn_cards;

pub use delegations::{
    CreateDelegationRequest, Delegation, DelegationLaunchGuard, DelegationProgress,
    DelegationSubagent, DelegationSubagentOverview, MAX_RESERVED_READONLY_SLOTS,
};

use crate::ContextForkWorkspaceReservation;
use anyhow::{anyhow, Result};
use sqlx::postgres::{PgPoolOptions, PgRow};
use sqlx::{PgPool, Row};
use std::time::Duration;

fn ensure_valid_transcript_ancestry(rows: &[PgRow]) -> Result<()> {
    if rows
        .iter()
        .any(|row| row.get::<bool, _>("ancestry_invalid"))
    {
        return Err(anyhow!("transcript ancestry contains a cycle"));
    }
    Ok(())
}

pub struct PostgresAgentStore {
    pub(crate) pool: PgPool,
}

impl PostgresAgentStore {
    pub async fn reserve_context_fork_workspace(
        &self,
        child_session_id: &str,
        parent_session_id: &str,
        runtime_id: &str,
        workspace_id: &str,
        owner_id: &str,
    ) -> Result<()> {
        let updated = sqlx::query(
            r#"
            insert into context_fork_workspace_reservations
                (child_session_id, parent_session_id, runtime_id, workspace_id, owner_id, state)
            values ($1, $2, $3, $4, $5, 'materializing')
            on conflict (child_session_id) do update
            set parent_session_id=$2, runtime_id=$3, workspace_id=$4
            where context_fork_workspace_reservations.parent_session_id=$2
              and context_fork_workspace_reservations.runtime_id=$3
              and context_fork_workspace_reservations.workspace_id=$4
              and context_fork_workspace_reservations.owner_id=$5
              and context_fork_workspace_reservations.state='materializing'
            "#,
        )
        .bind(child_session_id)
        .bind(parent_session_id)
        .bind(runtime_id)
        .bind(workspace_id)
        .bind(owner_id)
        .execute(&self.pool)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(anyhow!(
                "context fork workspace reservation is owned by another daemon or has different lifecycle identity"
            ));
        }
        Ok(())
    }

    pub async fn context_fork_workspace_reservations(
        &self,
        runtime_id: Option<&str>,
        excluding_owner_id: &str,
    ) -> Result<Vec<ContextForkWorkspaceReservation>> {
        let rows = if let Some(runtime_id) = runtime_id {
            sqlx::query(
                r#"
                select child_session_id, parent_session_id, runtime_id,
                       workspace_id, owner_id, state, remove_session
                from context_fork_workspace_reservations
                where runtime_id=$1 and (owner_id<>$2 or state='cleanup_pending')
                order by created_at, child_session_id
                "#,
            )
            .bind(runtime_id)
            .bind(excluding_owner_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                r#"
                select child_session_id, parent_session_id, runtime_id,
                       workspace_id, owner_id, state, remove_session
                from context_fork_workspace_reservations
                where owner_id<>$1 or state='cleanup_pending'
                order by created_at, child_session_id
                "#,
            )
            .bind(excluding_owner_id)
            .fetch_all(&self.pool)
            .await?
        };
        use sqlx::Row;
        Ok(rows
            .into_iter()
            .map(|row| ContextForkWorkspaceReservation {
                child_session_id: row.get("child_session_id"),
                parent_session_id: row.get("parent_session_id"),
                runtime_id: row.get("runtime_id"),
                workspace_id: row.get("workspace_id"),
                owner_id: row.get("owner_id"),
                state: row.get("state"),
                remove_session: row.get("remove_session"),
            })
            .collect())
    }

    pub async fn mark_context_fork_workspace_cleanup_pending(
        &self,
        child_session_id: &str,
        parent_session_id: &str,
        runtime_id: &str,
        workspace_id: &str,
        owner_id: &str,
    ) -> Result<()> {
        self.mark_context_fork_workspace_cleanup_pending_with_policy(
            child_session_id,
            parent_session_id,
            runtime_id,
            workspace_id,
            owner_id,
            true,
        )
        .await
    }

    pub async fn mark_context_fork_workspace_cleanup_pending_with_policy(
        &self,
        child_session_id: &str,
        parent_session_id: &str,
        runtime_id: &str,
        workspace_id: &str,
        owner_id: &str,
        remove_session: bool,
    ) -> Result<()> {
        let updated = sqlx::query(
            r#"
            insert into context_fork_workspace_reservations
                (child_session_id, parent_session_id, runtime_id, workspace_id, owner_id, state, remove_session)
            values ($1, $2, $3, $4, $5, 'cleanup_pending', $6)
            on conflict (child_session_id) do update
            set parent_session_id=$2, runtime_id=$3, workspace_id=$4,
                state='cleanup_pending', remove_session=$6
            where context_fork_workspace_reservations.owner_id=$5
              and context_fork_workspace_reservations.parent_session_id=$2
              and context_fork_workspace_reservations.runtime_id=$3
              and context_fork_workspace_reservations.workspace_id=$4
            "#,
        )
        .bind(child_session_id)
        .bind(parent_session_id)
        .bind(runtime_id)
        .bind(workspace_id)
        .bind(owner_id)
        .bind(remove_session)
        .execute(&self.pool)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(anyhow!(
                "context fork cleanup reservation is owned by another daemon or has different workspace identity"
            ));
        }
        Ok(())
    }

    pub async fn release_context_fork_workspace_reservation(
        &self,
        child_session_id: &str,
        runtime_id: &str,
        workspace_id: &str,
        owner_id: &str,
    ) -> Result<bool> {
        Ok(sqlx::query(
            "delete from context_fork_workspace_reservations
             where child_session_id=$1 and runtime_id=$2 and workspace_id=$3
               and owner_id=$4 and state='materializing'",
        )
        .bind(child_session_id)
        .bind(runtime_id)
        .bind(workspace_id)
        .bind(owner_id)
        .execute(&self.pool)
        .await?
        .rows_affected()
            == 1)
    }

    /// Finish an explicitly marked cleanup only after its exact workspace has
    /// been destroyed. Keeping these deletes in one transaction prevents a
    /// child or reservation from disappearing between retries.
    pub async fn complete_context_fork_workspace_cleanup(
        &self,
        child_session_id: &str,
        parent_session_id: &str,
        runtime_id: &str,
        workspace_id: &str,
        owner_id: &str,
    ) -> Result<bool> {
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "delete from context_fork_workspace_reservations
             where child_session_id=$1 and parent_session_id=$2 and runtime_id=$3
               and workspace_id=$4 and owner_id=$5 and state='cleanup_pending'
             returning remove_session",
        )
        .bind(child_session_id)
        .bind(parent_session_id)
        .bind(runtime_id)
        .bind(workspace_id)
        .bind(owner_id)
        .fetch_optional(&mut *tx)
        .await?;
        let Some(row) = row else {
            tx.rollback().await?;
            return Ok(false);
        };
        if row.get::<bool, _>("remove_session") {
            sqlx::query(
                "delete from sessions
                 where id=$1 and parent_session_id=$2 and runtime_id=$3 and workspace_id=$4",
            )
            .bind(child_session_id)
            .bind(parent_session_id)
            .bind(runtime_id)
            .bind(workspace_id)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Claim only old-owner reservations that have remained in materializing
    /// longer than the lease. The child absence fence prevents a stale daemon
    /// from destroying a session whose creation already committed.
    pub async fn claim_stale_context_fork_workspace_reservations(
        &self,
        runtime_id: &str,
        owner_id: &str,
        stale_after_secs: i64,
    ) -> Result<Vec<ContextForkWorkspaceReservation>> {
        let rows = sqlx::query(
            r#"
            update context_fork_workspace_reservations r
            set owner_id=$2, state='cleanup_pending', remove_session=true
            where r.runtime_id=$1 and r.owner_id<>$2 and r.state='materializing'
              and r.created_at < now() - make_interval(secs => $3)
              and not exists (select 1 from sessions s where s.id=r.child_session_id)
            returning child_session_id, parent_session_id, runtime_id, workspace_id,
                      owner_id, state, remove_session
            "#,
        )
        .bind(runtime_id)
        .bind(owner_id)
        .bind(stale_after_secs as f64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ContextForkWorkspaceReservation {
                child_session_id: row.get("child_session_id"),
                parent_session_id: row.get("parent_session_id"),
                runtime_id: row.get("runtime_id"),
                workspace_id: row.get("workspace_id"),
                owner_id: row.get("owner_id"),
                state: row.get("state"),
                remove_session: row.get("remove_session"),
            })
            .collect())
    }

    /// Adopt only stale materializations whose committed child exactly matches
    /// the reservation identity. Adoption leaves the row materializing so the
    /// normal owner-fenced child cleanup path can transition it safely.
    pub async fn adopt_stale_context_fork_workspace_reservations(
        &self,
        runtime_id: &str,
        owner_id: &str,
        stale_after_secs: i64,
    ) -> Result<Vec<ContextForkWorkspaceReservation>> {
        let rows = sqlx::query(
            r#"
            update context_fork_workspace_reservations r
            set owner_id=$2
            where r.runtime_id=$1 and r.owner_id<>$2 and r.state='materializing'
              and r.created_at < now() - make_interval(secs => $3)
              and exists (
                  select 1
                  from sessions s
                  where s.id=r.child_session_id
                    and s.parent_session_id=r.parent_session_id
                    and s.runtime_id=r.runtime_id
                    and s.workspace_id=r.workspace_id
                    and s.subagent_type='read_only'
              )
            returning child_session_id, parent_session_id, runtime_id, workspace_id,
                      owner_id, state, remove_session
            "#,
        )
        .bind(runtime_id)
        .bind(owner_id)
        .bind(stale_after_secs as f64)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| ContextForkWorkspaceReservation {
                child_session_id: row.get("child_session_id"),
                parent_session_id: row.get("parent_session_id"),
                runtime_id: row.get("runtime_id"),
                workspace_id: row.get("workspace_id"),
                owner_id: row.get("owner_id"),
                state: row.get("state"),
                remove_session: row.get("remove_session"),
            })
            .collect())
    }

    pub async fn connect(database_url: &str) -> Result<Self> {
        Ok(Self {
            pool: PgPoolOptions::new()
                .max_connections(8)
                .acquire_timeout(Duration::from_secs(5))
                .idle_timeout(Duration::from_secs(300))
                .connect(database_url)
                .await?,
        })
    }

    pub async fn migrate(&self) -> Result<()> {
        schema::migrate(&self.pool).await
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}
