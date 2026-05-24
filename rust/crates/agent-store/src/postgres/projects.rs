use anyhow::{anyhow, Result};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

use crate::{Project, WorkspaceMount};

use super::PostgresAgentStore;

impl PostgresAgentStore {
    pub async fn create_project(
        &self,
        project_id: Uuid,
        name: &str,
        workspaces: &[WorkspaceMount],
        metadata: Value,
    ) -> Result<Project> {
        let row = sqlx::query(
            r#"
            insert into projects (id, name, workspaces, metadata)
            values ($1, $2, $3, $4)
            returning
                id,
                name,
                workspaces,
                metadata,
                created_at::text as created_at,
                updated_at::text as updated_at
            "#,
        )
        .bind(project_id)
        .bind(name)
        .bind(serde_json::to_value(workspaces)?)
        .bind(metadata)
        .fetch_one(&self.pool)
        .await?;
        project_from_row(row)
    }

    pub async fn list_projects(&self) -> Result<Vec<Project>> {
        let rows = sqlx::query(
            r#"
            select
                id,
                name,
                workspaces,
                metadata,
                created_at::text as created_at,
                updated_at::text as updated_at
            from projects
            where metadata->>'hidden' is distinct from 'true'
            order by
                name asc,
                created_at asc
            "#,
        )
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(project_from_row).collect()
    }

    pub async fn get_project(&self, project_id: Uuid) -> Result<Project> {
        let row = sqlx::query(
            r#"
            select
                id,
                name,
                workspaces,
                metadata,
                created_at::text as created_at,
                updated_at::text as updated_at
            from projects
            where id=$1
            "#,
        )
        .bind(project_id)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| anyhow!("project not found: {project_id}"))?;
        project_from_row(row)
    }

    pub async fn update_project(
        &self,
        project_id: Uuid,
        name: &str,
        workspaces: &[WorkspaceMount],
    ) -> Result<Project> {
        let row = sqlx::query(
            r#"
            update projects
            set name=$2, workspaces=$3, updated_at=now()
            where id=$1
            returning
                id,
                name,
                workspaces,
                metadata,
                created_at::text as created_at,
                updated_at::text as updated_at
            "#,
        )
        .bind(project_id)
        .bind(name)
        .bind(serde_json::to_value(workspaces)?)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| anyhow!("project not found: {project_id}"))?;
        project_from_row(row)
    }

    pub async fn delete_empty_project(&self, project_id: Uuid) -> Result<bool> {
        let result = sqlx::query(
            r#"
            delete from projects
            where id=$1
                and not exists(select 1 from sessions where project_id=$1)
            "#,
        )
        .bind(project_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }
}

pub(super) fn project_from_row(row: sqlx::postgres::PgRow) -> Result<Project> {
    Ok(Project {
        project_id: row.get("id"),
        name: row.get("name"),
        workspaces: serde_json::from_value(row.get::<Value, _>("workspaces"))?,
        metadata: row.get::<Value, _>("metadata"),
        created_at: row.get("created_at"),
        updated_at: row.get("updated_at"),
    })
}
