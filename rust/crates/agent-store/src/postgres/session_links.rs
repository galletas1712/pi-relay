use anyhow::{anyhow, Result};
use sqlx::Row;

use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn set_session_parent(
        &self,
        child_session_id: &str,
        parent_session_id: &str,
    ) -> Result<()> {
        if child_session_id == parent_session_id {
            return Err(anyhow!(
                "child session id must differ from parent session id"
            ));
        }
        let updated = sqlx::query(
            r#"
            update sessions
            set parent_session_id=$2::text,
                updated_at=now()
            where id=$1::text
            "#,
        )
        .bind(child_session_id)
        .bind(parent_session_id)
        .execute(&self.pool)
        .await?;
        if updated.rows_affected() == 0 {
            return Err(anyhow!("session not found: {child_session_id}"));
        }
        Ok(())
    }

    pub async fn session_parent_id(&self, child_session_id: &str) -> Result<Option<String>> {
        let row = sqlx::query("select parent_session_id from sessions where id=$1::text")
            .bind(child_session_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("session not found: {child_session_id}"))?;
        Ok(row.get("parent_session_id"))
    }

    pub async fn list_child_session_ids(&self, parent_session_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            r#"
            select id as child_session_id
            from sessions
            where parent_session_id=$1::text
            order by created_at, id
            "#,
        )
        .bind(parent_session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| row.get("child_session_id"))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_vocab::{ProviderConfig, ProviderKind, ReasoningEffort};
    use serde_json::json;
    use uuid::Uuid;

    use crate::SessionConfig;

    use super::*;

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(30_000);

    struct TestDb {
        store: PostgresAgentStore,
        admin_url: String,
        name: String,
    }

    impl TestDb {
        async fn cleanup(self) {
            self.store.close().await;
            if let Ok(admin) = sqlx::PgPool::connect(&self.admin_url).await {
                let _ = sqlx::query(&format!(r#"drop database if exists \"{}\""#, self.name))
                    .execute(&admin)
                    .await;
                admin.close().await;
            }
        }
    }

    async fn test_store() -> Option<TestDb> {
        let admin_url = std::env::var("PI_RELAY_TEST_DATABASE_URL").ok()?;
        let name = format!(
            "pi_relay_session_parent_test_{}_{}",
            std::process::id(),
            TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = sqlx::PgPool::connect(&admin_url)
            .await
            .expect("connect to PI_RELAY_TEST_DATABASE_URL");
        sqlx::query(&format!(r#"create database \"{name}\""#))
            .execute(&admin)
            .await
            .expect("create isolated test database");
        admin.close().await;
        let database_url = database_url_with_name(&admin_url, &name);
        let store = PostgresAgentStore::connect(&database_url)
            .await
            .expect("connect isolated test database");
        store
            .migrate()
            .await
            .expect("migrate isolated test database");
        Some(TestDb {
            store,
            admin_url,
            name,
        })
    }

    fn database_url_with_name(base: &str, name: &str) -> String {
        let (prefix, query) = base
            .split_once('?')
            .map(|(prefix, query)| (prefix, format!("?{query}")))
            .unwrap_or((base, String::new()));
        let Some((root, _)) = prefix.rsplit_once('/') else {
            return format!("{base}_{name}");
        };
        format!("{root}/{name}{query}")
    }

    fn session_config(project_id: Uuid) -> SessionConfig {
        SessionConfig {
            project_id: Some(project_id),
            outer_cwd: "/tmp/pi-relay-test".to_string(),
            workspaces: Vec::new(),
            system_prompt: String::new(),
            provider: ProviderConfig {
                kind: ProviderKind::OpenAi,
                model: "gpt-5".to_string(),
                reasoning_effort: ReasoningEffort::Medium,
                max_tokens: None,
                prompt_cache: None,
            },
            metadata: json!({}),
        }
    }

    #[tokio::test]
    async fn parent_session_ids_can_be_set_and_listed() {
        let Some(db) = test_store().await else { return };
        let project_id = Uuid::new_v4();
        let parent_session_id = "parent-session";
        let child_session_id = "child-session";
        db.store
            .start_session_outputs(
                parent_session_id,
                &session_config(project_id),
                &[],
                None,
                &[],
                &[],
                crate::InputPriority::FollowUp,
                &agent_vocab::UserMessage {
                    content: Vec::new(),
                },
                None,
            )
            .await
            .expect("create parent session");
        db.store
            .start_session_outputs(
                child_session_id,
                &session_config(project_id),
                &[],
                None,
                &[],
                &[],
                crate::InputPriority::FollowUp,
                &agent_vocab::UserMessage {
                    content: Vec::new(),
                },
                None,
            )
            .await
            .expect("create child session");

        db.store
            .set_session_parent(child_session_id, parent_session_id)
            .await
            .expect("set parent session id");

        let children = db
            .store
            .list_child_session_ids(parent_session_id)
            .await
            .expect("list children");
        assert_eq!(children, vec![child_session_id.to_string()]);
        let parent_id = db
            .store
            .session_parent_id(child_session_id)
            .await
            .expect("parent id loads");
        assert_eq!(parent_id.as_deref(), Some(parent_session_id));

        db.cleanup().await;
    }
}
