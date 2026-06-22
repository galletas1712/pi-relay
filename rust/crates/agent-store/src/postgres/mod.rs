mod action_records;
mod actions;
mod compaction;
mod delegations;
mod events;
mod outputs;
mod projects;
mod queue;
mod rows;
mod schema;
mod session_links;
mod sessions;
mod snapshots;
mod sql;
mod token_usage;
mod transcript;
mod turn_cards;

pub use delegations::{Delegation, DelegationProgress, DelegationSubagent};

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
