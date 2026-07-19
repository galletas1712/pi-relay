use agent_runtime_protocol::RuntimeRecord;
use anyhow::{anyhow, Result};
use sqlx::Row;

use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn session_runtime_id(&self, session_id: &str) -> Result<String> {
        sqlx::query_scalar("select runtime_id from sessions where id=$1")
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("session not found: {session_id}"))
    }

    pub async fn register_runtime(&self, runtime_id: &str, name: &str) -> Result<()> {
        sqlx::query(
            r#"
            insert into runtimes (id, name, last_seen_at)
            values ($1, $2, now())
            on conflict (id) do update
            set name=excluded.name, last_seen_at=now(), updated_at=now()
            "#,
        )
        .bind(runtime_id)
        .bind(name)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn runtime_heartbeat(&self, runtime_id: &str) -> Result<()> {
        let updated =
            sqlx::query("update runtimes set last_seen_at=now(), updated_at=now() where id=$1")
                .bind(runtime_id)
                .execute(&self.pool)
                .await?;
        if updated.rows_affected() != 1 {
            return Err(anyhow!("runtime not registered: {runtime_id}"));
        }
        Ok(())
    }

    pub async fn list_runtimes(&self) -> Result<Vec<RuntimeRecord>> {
        let rows = sqlx::query(
            r#"
            select id, name, last_seen_at::text as last_seen_at
            from runtimes
            order by name, id
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| RuntimeRecord {
                runtime_id: row.get("id"),
                name: row.get("name"),
                online: false,
                last_seen_at: row.get("last_seen_at"),
            })
            .collect())
    }
}
