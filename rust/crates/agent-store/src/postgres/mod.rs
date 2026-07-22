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

/// Maximum transcript ancestry depth accepted by recursive reads. Recursive
/// queries fetch one sentinel row beyond this budget so malformed/cyclic data
/// is reported instead of silently truncated.
pub(super) const TRANSCRIPT_RECURSION_LIMIT: i64 = 10_000;

pub use delegations::{
    Delegation, DelegationProgress, DelegationSubagent, DelegationSubagentOverview,
};

use anyhow::Result;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::time::Duration;

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
