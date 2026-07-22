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
    Delegation, DelegationProgress, DelegationSubagent, DelegationSubagentOverview,
};

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
