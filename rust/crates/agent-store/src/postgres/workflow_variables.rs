use anyhow::Result;
use serde_json::Value;
use sqlx::Row;

use crate::{WorkflowVariable, WorkflowVariableWrite};

use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn write_workflow_variable(
        &self,
        variable: &WorkflowVariableWrite,
    ) -> Result<WorkflowVariable> {
        let row = sqlx::query(
            r#"
            insert into workflow_variables (
                owner_session_id,
                workflow_id,
                name,
                value_json,
                value_text,
                producer_session_id,
                producer_action_id
            )
            values ($1, $2, $3, $4, $5, $6, $7)
            on conflict (owner_session_id, workflow_id, name)
            do update set
                value_json=excluded.value_json,
                value_text=excluded.value_text,
                producer_session_id=excluded.producer_session_id,
                producer_action_id=excluded.producer_action_id,
                updated_at=now()
            returning
                owner_session_id,
                workflow_id,
                name,
                value_json,
                value_text,
                producer_session_id,
                producer_action_id,
                created_at::text as created_at,
                updated_at::text as updated_at
            "#,
        )
        .bind(&variable.owner_session_id)
        .bind(&variable.workflow_id)
        .bind(&variable.name)
        .bind(&variable.value_json)
        .bind(&variable.value_text)
        .bind(&variable.producer_session_id)
        .bind(&variable.producer_action_id)
        .fetch_one(&self.pool)
        .await?;
        workflow_variable_from_row(row)
    }

    pub async fn workflow_variable(
        &self,
        owner_session_id: &str,
        workflow_id: &str,
        name: &str,
    ) -> Result<Option<WorkflowVariable>> {
        let row = sqlx::query(
            r#"
            select
                owner_session_id,
                workflow_id,
                name,
                value_json,
                value_text,
                producer_session_id,
                producer_action_id,
                created_at::text as created_at,
                updated_at::text as updated_at
            from workflow_variables
            where owner_session_id=$1 and workflow_id=$2 and name=$3
            "#,
        )
        .bind(owner_session_id)
        .bind(workflow_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        row.map(workflow_variable_from_row).transpose()
    }

    pub async fn list_workflow_variables(
        &self,
        owner_session_id: &str,
        workflow_id: &str,
        limit: i64,
    ) -> Result<Vec<WorkflowVariable>> {
        let rows = sqlx::query(
            r#"
            select
                owner_session_id,
                workflow_id,
                name,
                value_json,
                value_text,
                producer_session_id,
                producer_action_id,
                created_at::text as created_at,
                updated_at::text as updated_at
            from workflow_variables
            where owner_session_id=$1 and workflow_id=$2
            order by name
            limit $3
            "#,
        )
        .bind(owner_session_id)
        .bind(workflow_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(workflow_variable_from_row).collect()
    }
}

fn workflow_variable_from_row(row: sqlx::postgres::PgRow) -> Result<WorkflowVariable> {
    Ok(WorkflowVariable {
        owner_session_id: row.get("owner_session_id"),
        workflow_id: row.get("workflow_id"),
        name: row.get("name"),
        value_json: row.get::<Option<Value>, _>("value_json"),
        value_text: row.get("value_text"),
        producer_session_id: row.get("producer_session_id"),
        producer_action_id: row.get("producer_action_id"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use agent_vocab::{ProviderConfig, ProviderKind, ReasoningEffort};
    use serde_json::json;
    use uuid::Uuid;

    use crate::{SessionConfig, WorkflowVariableWrite};

    use super::*;

    static TEST_DB_COUNTER: AtomicU64 = AtomicU64::new(40_000);

    struct TestDb {
        store: PostgresAgentStore,
        admin_url: String,
        name: String,
    }

    impl TestDb {
        async fn cleanup(self) {
            self.store.close().await;
            if let Ok(admin) = sqlx::PgPool::connect(&self.admin_url).await {
                let _ = sqlx::query(&format!(r#"drop database if exists "{}""#, self.name))
                    .execute(&admin)
                    .await;
                admin.close().await;
            }
        }
    }

    async fn test_store() -> Option<TestDb> {
        let admin_url = std::env::var("PI_RELAY_TEST_DATABASE_URL").ok()?;
        let name = format!(
            "pi_relay_workflow_variable_test_{}_{}",
            std::process::id(),
            TEST_DB_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let admin = sqlx::PgPool::connect(&admin_url)
            .await
            .expect("connect to PI_RELAY_TEST_DATABASE_URL");
        sqlx::query(&format!(r#"create database "{name}""#))
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
            outer_cwd: "/tmp".to_string(),
            workspaces: Vec::new(),
            system_prompt: "test prompt".to_string(),
            provider: ProviderConfig {
                kind: ProviderKind::OpenAi,
                model: "test-model".to_string(),
                reasoning_effort: ReasoningEffort::Medium,
                max_tokens: None,
                prompt_cache: None,
            },
            metadata: json!({}),
        }
    }

    async fn create_session(store: &PostgresAgentStore, session_id: &str) {
        let project_id = Uuid::new_v4();
        store
            .create_project(project_id, "workflow variable test", &[], json!({}))
            .await
            .expect("project creates");
        store
            .create_session(session_id, &session_config(project_id))
            .await
            .expect("session creates");
    }

    #[tokio::test]
    async fn workflow_variables_can_be_written_listed_read_and_rewritten() {
        let Some(db) = test_store().await else {
            return;
        };
        let store = &db.store;
        create_session(store, "producer").await;

        let first = store
            .write_workflow_variable(&WorkflowVariableWrite {
                owner_session_id: "producer".to_string(),
                workflow_id: "workflow_1".to_string(),
                name: "review".to_string(),
                value_json: Some(json!({ "issues": [] })),
                value_text: Some("No issues.".to_string()),
                producer_session_id: Some("producer".to_string()),
                producer_action_id: Some("action_1".to_string()),
            })
            .await
            .expect("variable writes");
        assert_eq!(first.owner_session_id, "producer");
        assert_eq!(first.workflow_id, "workflow_1");
        assert_eq!(first.name, "review");

        let listed = store
            .list_workflow_variables("producer", "workflow_1", 100)
            .await
            .expect("variables list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].value_text.as_deref(), Some("No issues."));

        let updated = store
            .write_workflow_variable(&WorkflowVariableWrite {
                owner_session_id: "producer".to_string(),
                workflow_id: "workflow_1".to_string(),
                name: "review".to_string(),
                value_json: Some(json!({ "issues": ["one"] })),
                value_text: Some("One issue.".to_string()),
                producer_session_id: Some("producer".to_string()),
                producer_action_id: Some("action_2".to_string()),
            })
            .await
            .expect("variable rewrites");
        assert_eq!(updated.value_json, Some(json!({ "issues": ["one"] })));
        assert_eq!(updated.producer_action_id.as_deref(), Some("action_2"));

        let read = store
            .workflow_variable("producer", "workflow_1", "review")
            .await
            .expect("variable reads")
            .expect("variable exists");
        assert_eq!(read.value_text.as_deref(), Some("One issue."));

        db.cleanup().await;
    }

    #[tokio::test]
    async fn workflow_variables_are_deleted_with_owner_session() {
        let Some(db) = test_store().await else {
            return;
        };
        let store = &db.store;
        create_session(store, "owner").await;

        store
            .write_workflow_variable(&WorkflowVariableWrite {
                owner_session_id: "owner".to_string(),
                workflow_id: "workflow_1".to_string(),
                name: "result".to_string(),
                value_json: Some(json!({ "ok": true })),
                value_text: Some("done".to_string()),
                producer_session_id: Some("owner".to_string()),
                producer_action_id: Some("action_1".to_string()),
            })
            .await
            .expect("variable writes");

        let deleted = store
            .delete_session("owner")
            .await
            .expect("session deletes");
        assert!(deleted);

        let variables = store
            .list_workflow_variables("owner", "workflow_1", 100)
            .await
            .expect("variables list after owner delete");
        assert!(variables.is_empty());

        db.cleanup().await;
    }
}
