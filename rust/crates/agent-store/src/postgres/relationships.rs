use anyhow::{anyhow, Result};
use sqlx::Row;

use crate::{CreateSessionRelationship, SessionRelationship};

use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn create_session_relationship(
        &self,
        relationship: &CreateSessionRelationship,
    ) -> Result<SessionRelationship> {
        let row = sqlx::query(
            r#"
            insert into session_relationships (
                id,
                parent_session_id,
                child_session_id
            )
            values ($1, $2, $3)
            returning
                id,
                parent_session_id,
                child_session_id,
                created_at::text as created_at,
                updated_at::text as updated_at
            "#,
        )
        .bind(&relationship.relationship_id)
        .bind(&relationship.parent_session_id)
        .bind(&relationship.child_session_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(relationship_from_row(row))
    }

    pub async fn session_relationship(&self, relationship_id: &str) -> Result<SessionRelationship> {
        let row = sqlx::query(RELATIONSHIP_SELECT)
            .bind(relationship_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("session relationship not found: {relationship_id}"))?;
        Ok(relationship_from_row(row))
    }

    pub async fn session_relationship_for_child(
        &self,
        child_session_id: &str,
    ) -> Result<Option<SessionRelationship>> {
        let row = sqlx::query(
            r#"
            select
                id,
                parent_session_id,
                child_session_id,
                created_at::text as created_at,
                updated_at::text as updated_at
            from session_relationships
            where child_session_id=$1
            "#,
        )
        .bind(child_session_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(relationship_from_row))
    }

    pub async fn list_child_session_relationships(
        &self,
        parent_session_id: &str,
    ) -> Result<Vec<SessionRelationship>> {
        let rows = sqlx::query(
            r#"
            select
                id,
                parent_session_id,
                child_session_id,
                created_at::text as created_at,
                updated_at::text as updated_at
            from session_relationships
            where parent_session_id=$1
            order by created_at, id
            "#,
        )
        .bind(parent_session_id)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.into_iter().map(relationship_from_row).collect())
    }
}

const RELATIONSHIP_SELECT: &str = r#"
    select
        id,
        parent_session_id,
        child_session_id,
        created_at::text as created_at,
        updated_at::text as updated_at
    from session_relationships
    where id=$1
"#;

fn relationship_from_row(row: sqlx::postgres::PgRow) -> SessionRelationship {
    SessionRelationship {
        relationship_id: row.get("id"),
        parent_session_id: row.get("parent_session_id"),
        child_session_id: row.get("child_session_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_vocab::{ProviderConfig, ProviderKind, ReasoningEffort};
    use serde_json::json;
    use uuid::Uuid;

    use crate::{CreateSessionRelationship, SessionConfig};

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
            "pi_relay_relationship_test_{}_{}",
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
    async fn parent_relationships_can_be_created_and_listed() {
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

        let created = db
            .store
            .create_session_relationship(&CreateSessionRelationship {
                relationship_id: "rel-parent-child".to_string(),
                parent_session_id: parent_session_id.to_string(),
                child_session_id: child_session_id.to_string(),
            })
            .await
            .expect("create relationship");
        assert_eq!(created.parent_session_id, parent_session_id);
        assert_eq!(created.child_session_id, child_session_id);

        let children = db
            .store
            .list_child_session_relationships(parent_session_id)
            .await
            .expect("list children");
        assert_eq!(children, vec![created.clone()]);
        let by_child = db
            .store
            .session_relationship_for_child(child_session_id)
            .await
            .expect("relationship by child")
            .expect("child has parent");
        assert_eq!(by_child, created);

        db.cleanup().await;
    }
}
