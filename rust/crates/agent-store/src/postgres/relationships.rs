use anyhow::{anyhow, Result};
use serde_json::Value;
use sqlx::Row;

use crate::{
    CreateSessionRelationship, SessionRelationship, SessionRelationshipKind,
    SessionRelationshipPatch,
};

use super::rows::row_text;
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
                source_session_id,
                target_session_id,
                root_session_id,
                kind,
                control_mode,
                visibility,
                role_name,
                role_workspace,
                display_name,
                task,
                spawned_from_leaf_id,
                spawned_from_action_row_id,
                workflow_id,
                result_variable,
                status,
                filesystem_mode,
                baseline_cwd,
                metadata
            )
            values (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10,
                $11, $12, $13, $14, $15, $16, $17, $18, $19
            )
            returning
                id,
                source_session_id,
                target_session_id,
                root_session_id,
                kind,
                control_mode,
                visibility,
                role_name,
                role_workspace,
                display_name,
                task,
                spawned_from_leaf_id,
                spawned_from_action_row_id,
                workflow_id,
                result_variable,
                status,
                filesystem_mode,
                baseline_cwd,
                metadata,
                created_at::text as created_at,
                updated_at::text as updated_at
            "#,
        )
        .bind(&relationship.relationship_id)
        .bind(&relationship.source_session_id)
        .bind(&relationship.target_session_id)
        .bind(&relationship.root_session_id)
        .bind(relationship.kind.as_str())
        .bind(relationship.control_mode.as_str())
        .bind(relationship.visibility.as_str())
        .bind(&relationship.role_name)
        .bind(&relationship.role_workspace)
        .bind(&relationship.display_name)
        .bind(&relationship.task)
        .bind(&relationship.spawned_from_leaf_id)
        .bind(&relationship.spawned_from_action_row_id)
        .bind(&relationship.workflow_id)
        .bind(&relationship.result_variable)
        .bind(relationship.status.as_str())
        .bind(relationship.filesystem_mode.map(|mode| mode.as_str()))
        .bind(&relationship.baseline_cwd)
        .bind(&relationship.metadata)
        .fetch_one(&self.pool)
        .await?;
        relationship_from_row(row)
    }

    pub async fn session_relationship(&self, relationship_id: &str) -> Result<SessionRelationship> {
        let row = sqlx::query(RELATIONSHIP_SELECT)
            .bind(relationship_id)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| anyhow!("session relationship not found: {relationship_id}"))?;
        relationship_from_row(row)
    }

    pub async fn session_relationship_for_target(
        &self,
        target_session_id: &str,
    ) -> Result<Option<SessionRelationship>> {
        let row = sqlx::query(
            r#"
            select
                id,
                source_session_id,
                target_session_id,
                root_session_id,
                kind,
                control_mode,
                visibility,
                role_name,
                role_workspace,
                display_name,
                task,
                spawned_from_leaf_id,
                spawned_from_action_row_id,
                workflow_id,
                result_variable,
                status,
                filesystem_mode,
                baseline_cwd,
                metadata,
                created_at::text as created_at,
                updated_at::text as updated_at
            from session_relationships
            where target_session_id=$1
            "#,
        )
        .bind(target_session_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(relationship_from_row).transpose()
    }

    pub async fn list_session_relationships_by_source(
        &self,
        source_session_id: &str,
        kind: Option<SessionRelationshipKind>,
    ) -> Result<Vec<SessionRelationship>> {
        let rows = sqlx::query(
            r#"
            select
                id,
                source_session_id,
                target_session_id,
                root_session_id,
                kind,
                control_mode,
                visibility,
                role_name,
                role_workspace,
                display_name,
                task,
                spawned_from_leaf_id,
                spawned_from_action_row_id,
                workflow_id,
                result_variable,
                status,
                filesystem_mode,
                baseline_cwd,
                metadata,
                created_at::text as created_at,
                updated_at::text as updated_at
            from session_relationships
            where source_session_id=$1
                and ($2::text is null or kind=$2)
            order by created_at, id
            "#,
        )
        .bind(source_session_id)
        .bind(kind.map(|kind| kind.as_str()))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(relationship_from_row).collect()
    }

    pub async fn update_session_relationship(
        &self,
        relationship_id: &str,
        patch: SessionRelationshipPatch,
    ) -> Result<SessionRelationship> {
        let row = sqlx::query(
            r#"
            update session_relationships
            set
                status=coalesce($2, status),
                display_name=case when $3 then $4 else display_name end,
                metadata=coalesce($5, metadata),
                updated_at=now()
            where id=$1
            returning
                id,
                source_session_id,
                target_session_id,
                root_session_id,
                kind,
                control_mode,
                visibility,
                role_name,
                role_workspace,
                display_name,
                task,
                spawned_from_leaf_id,
                spawned_from_action_row_id,
                workflow_id,
                result_variable,
                status,
                filesystem_mode,
                baseline_cwd,
                metadata,
                created_at::text as created_at,
                updated_at::text as updated_at
            "#,
        )
        .bind(relationship_id)
        .bind(patch.status.map(|status| status.as_str()))
        .bind(patch.display_name.is_some())
        .bind(patch.display_name.flatten())
        .bind(patch.metadata)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| anyhow!("session relationship not found: {relationship_id}"))?;
        relationship_from_row(row)
    }
}

const RELATIONSHIP_SELECT: &str = r#"
    select
        id,
        source_session_id,
        target_session_id,
        root_session_id,
        kind,
        control_mode,
        visibility,
        role_name,
        role_workspace,
        display_name,
        task,
        spawned_from_leaf_id,
        spawned_from_action_row_id,
        workflow_id,
        result_variable,
        status,
        filesystem_mode,
        baseline_cwd,
        metadata,
        created_at::text as created_at,
        updated_at::text as updated_at
    from session_relationships
    where id=$1
"#;

fn relationship_from_row(row: sqlx::postgres::PgRow) -> Result<SessionRelationship> {
    Ok(SessionRelationship {
        relationship_id: row.get("id"),
        source_session_id: row.get("source_session_id"),
        target_session_id: row.get("target_session_id"),
        root_session_id: row.get("root_session_id"),
        kind: row_text(&row, "kind")?,
        control_mode: row_text(&row, "control_mode")?,
        visibility: row_text(&row, "visibility")?,
        role_name: row.get("role_name"),
        role_workspace: row.get("role_workspace"),
        display_name: row.get("display_name"),
        task: row.get("task"),
        spawned_from_leaf_id: row.get("spawned_from_leaf_id"),
        spawned_from_action_row_id: row.get("spawned_from_action_row_id"),
        workflow_id: row.get("workflow_id"),
        result_variable: row.get("result_variable"),
        status: row_text(&row, "status")?,
        filesystem_mode: row
            .get::<Option<String>, _>("filesystem_mode")
            .map(|value| value.parse())
            .transpose()
            .map_err(anyhow::Error::msg)?,
        baseline_cwd: row.get("baseline_cwd"),
        metadata: row.get::<Value, _>("metadata"),
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

    use crate::{
        CreateSessionRelationship, SessionConfig, SessionRelationshipControlMode,
        SessionRelationshipFilesystemMode, SessionRelationshipKind, SessionRelationshipPatch,
        SessionRelationshipStatus, SessionRelationshipVisibility,
    };

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
            "pi_relay_relationship_test_{}_{}",
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
            .create_project(project_id, "relationship test", &[], json!({}))
            .await
            .expect("project creates");
        store
            .create_session(session_id, &session_config(project_id))
            .await
            .expect("session creates");
    }

    #[tokio::test]
    async fn session_relationships_can_be_created_listed_and_updated() {
        let Some(db) = test_store().await else {
            return;
        };
        let store = &db.store;
        create_session(store, "source").await;
        create_session(store, "child").await;

        let created = store
            .create_session_relationship(&CreateSessionRelationship {
                relationship_id: "rel_1".to_string(),
                source_session_id: "source".to_string(),
                target_session_id: "child".to_string(),
                root_session_id: "source".to_string(),
                kind: SessionRelationshipKind::Subagent,
                control_mode: SessionRelationshipControlMode::ParentControlled,
                visibility: SessionRelationshipVisibility::Hidden,
                role_name: Some("reviewer".to_string()),
                role_workspace: Some("repo".to_string()),
                display_name: Some("Reviewer".to_string()),
                task: "Review current work".to_string(),
                spawned_from_leaf_id: Some("leaf_1".to_string()),
                spawned_from_action_row_id: Some("action_1".to_string()),
                workflow_id: Some("workflow_1".to_string()),
                result_variable: Some("review".to_string()),
                status: SessionRelationshipStatus::Running,
                filesystem_mode: Some(SessionRelationshipFilesystemMode::PlainCopy),
                baseline_cwd: Some("/tmp/baseline".to_string()),
                metadata: json!({ "phase": "review" }),
            })
            .await
            .expect("relationship creates");
        assert_eq!(created.relationship_id, "rel_1");
        assert_eq!(created.kind, SessionRelationshipKind::Subagent);

        let relationships = store
            .list_session_relationships_by_source("source", Some(SessionRelationshipKind::Subagent))
            .await
            .expect("relationships list");
        assert_eq!(relationships.len(), 1);
        assert_eq!(relationships[0].target_session_id, "child");

        let target = store
            .session_relationship_for_target("child")
            .await
            .expect("target relationship loads")
            .expect("target relationship exists");
        assert_eq!(target.relationship_id, "rel_1");

        let updated = store
            .update_session_relationship(
                "rel_1",
                SessionRelationshipPatch {
                    status: Some(SessionRelationshipStatus::Completed),
                    display_name: Some(None),
                    metadata: Some(json!({ "done": true })),
                },
            )
            .await
            .expect("relationship updates");
        assert_eq!(updated.status, SessionRelationshipStatus::Completed);
        assert_eq!(updated.display_name, None);
        assert_eq!(updated.metadata, json!({ "done": true }));

        db.cleanup().await;
    }
}
