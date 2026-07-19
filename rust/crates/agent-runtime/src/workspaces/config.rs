use std::path::{Component, Path, PathBuf};

use agent_runtime_protocol::{ProjectWorkspace, WorkspaceKind};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

pub(super) const WORKSPACE_BASE_METADATA: &str = "metadata.json";
pub(super) const WORKSPACE_BASE_DIR: &str = "base";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct WorkspaceBaseConfig {
    pub(super) kind: WorkspaceKind,
    pub(super) workspace_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) remote_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) source_path: Option<String>,
}

pub(super) fn workspace_base_config(workspace: &ProjectWorkspace) -> Result<WorkspaceBaseConfig> {
    validate_workspace_dir(&workspace.workspace_dir)?;
    let workspace_dir = workspace.workspace_dir.trim().to_string();
    match workspace.kind {
        WorkspaceKind::Git => {
            let remote_url =
                required_git_field(workspace.remote_url.as_deref(), "remote_url")?.to_string();
            let remote_branch =
                required_git_field(workspace.remote_branch.as_deref(), "remote_branch")?
                    .to_string();
            Ok(WorkspaceBaseConfig {
                kind: WorkspaceKind::Git,
                workspace_dir,
                remote_url: Some(remote_url),
                remote_branch: Some(remote_branch),
                source_path: None,
            })
        }
        WorkspaceKind::Local => {
            let source_path =
                required_local_field(workspace.source_path.as_deref(), "source_path")?;
            let source = PathBuf::from(source_path);
            if !source.is_dir() {
                bail!(
                    "local workspace source_path is not a directory: {}",
                    source.display()
                );
            }
            Ok(WorkspaceBaseConfig {
                kind: WorkspaceKind::Local,
                workspace_dir,
                remote_url: None,
                remote_branch: None,
                source_path: Some(source.to_string_lossy().into_owned()),
            })
        }
    }
}

pub(super) async fn read_workspace_base_config(path: &Path) -> Result<Option<WorkspaceBaseConfig>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = tokio::fs::read(path)
        .await
        .with_context(|| format!("read workspace base metadata {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("decode workspace base metadata {}", path.display()))
        .map(Some)
}

pub(super) async fn write_workspace_base_config(
    path: &Path,
    config: &WorkspaceBaseConfig,
) -> Result<()> {
    let json = serde_json::to_vec_pretty(config).context("encode workspace base metadata")?;
    tokio::fs::write(path, json)
        .await
        .with_context(|| format!("write workspace base metadata {}", path.display()))?;
    Ok(())
}

pub fn validate_workspace_dir(workspace_dir: &str) -> Result<()> {
    let workspace_dir = workspace_dir.trim();
    if workspace_dir.is_empty() {
        bail!("workspace_dir is required");
    }
    if workspace_dir.starts_with('.') {
        bail!("workspace_dir must not start with '.': {workspace_dir}");
    }
    let path = Path::new(workspace_dir);
    let mut components = path.components();
    let valid_single_component =
        matches!(components.next(), Some(Component::Normal(_))) && components.next().is_none();
    if path.is_absolute() || !valid_single_component {
        bail!("workspace_dir must be a direct child name: {workspace_dir}");
    }
    if !workspace_dir
        .bytes()
        .all(|byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
    {
        bail!("workspace_dir may only contain ASCII letters, digits, '_' and '-': {workspace_dir}");
    }
    Ok(())
}

pub(super) fn required_git_field<'a>(value: Option<&'a str>, field: &str) -> Result<&'a str> {
    let value = value.unwrap_or("").trim();
    if value.is_empty() {
        bail!("git workspace {field} is required");
    }
    Ok(value)
}

pub(super) fn required_local_field<'a>(value: Option<&'a str>, field: &str) -> Result<&'a str> {
    let value = value.unwrap_or("").trim();
    if value.is_empty() {
        bail!("local workspace {field} is required");
    }
    Ok(value)
}

pub(super) fn path_component(value: &str) -> String {
    let encoded: String = value
        .bytes()
        .flat_map(|byte| match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => {
                vec![byte as char]
            }
            _ => format!("%{byte:02x}").chars().collect(),
        })
        .collect();
    if encoded.is_empty() {
        "%00".to_string()
    } else {
        encoded
    }
}

pub(super) fn branch_component(value: &str) -> String {
    let component = value
        .chars()
        .map(|ch| match ch {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '_' | '-' => ch,
            _ => '-',
        })
        .collect::<String>()
        .trim_matches(['-', '/'])
        .to_string();
    if component.is_empty() {
        "id".to_string()
    } else {
        component
    }
}
