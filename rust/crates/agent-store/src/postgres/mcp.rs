use anyhow::{bail, Result};

use crate::McpSessionManifestBinding;

pub(super) async fn install_session_manifest_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    binding: &McpSessionManifestBinding,
) -> Result<()> {
    sqlx::query(
        r#"
        insert into mcp_session_manifests (fingerprint, manifest)
        values ($1::text, $2)
        on conflict (fingerprint) do update
        set last_used_at=now()
        where mcp_session_manifests.manifest=excluded.manifest
        "#,
    )
    .bind(&binding.manifest_fingerprint)
    .bind(&binding.manifest)
    .execute(&mut **tx)
    .await?;
    let stored: Option<serde_json::Value> =
        sqlx::query_scalar("select manifest from mcp_session_manifests where fingerprint=$1::text")
            .bind(&binding.manifest_fingerprint)
            .fetch_optional(&mut **tx)
            .await?;
    if stored.as_ref() != Some(&binding.manifest) {
        bail!("MCP manifest fingerprint collision or corrupt stored manifest");
    }
    Ok(())
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
