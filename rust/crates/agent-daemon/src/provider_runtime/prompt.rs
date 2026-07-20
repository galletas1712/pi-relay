use std::path::{Path, PathBuf};

use agent_prompt::{
    load_pi_compaction_md, load_pi_md, render_prompt, PromptContext, PromptMcpServer,
    PromptProfile, PromptWorkspace, PromptWorkspaceKind, Skill, SubagentRole, ToolSpec,
};
use agent_runtime_protocol::RawSkillFile;
use agent_store::{SessionConfig, WorkspaceKind};
use agent_tools::ProviderTool;
use agent_vocab::ProviderKind;
use anyhow::{anyhow, Context};
use serde_json::Value;

use crate::state::AppState;

pub(super) async fn assemble_agent_prompt(
    _state: &AppState,
    config: &SessionConfig,
    _session_id: &str,
) -> anyhow::Result<agent_provider::PromptSections> {
    Ok(agent_provider::PromptSections::stable(
        config.system_prompt.clone(),
    ))
}

/// Add daemon-owned skills only when a runtime home/workspace skill does not
/// already expose the same name.
pub(super) fn extend_with_fallback_skills(skills: &mut Vec<Skill>, fallback: Vec<Skill>) {
    let mut names = skills
        .iter()
        .map(Skill::exposed_name)
        .collect::<std::collections::BTreeSet<_>>();
    for skill in fallback {
        if names.insert(skill.exposed_name()) {
            skills.push(skill);
        }
    }
}

pub(crate) async fn render_pi_prompt(
    state: &AppState,
    config: &SessionConfig,
) -> anyhow::Result<String> {
    let template = load_pi_md(&state.prompt_root)?;
    Ok(render_prompt(
        &template,
        &prompt_context(state, config).await?,
    ))
}

pub(crate) fn current_pi_template(state: &AppState) -> anyhow::Result<String> {
    Ok(load_pi_md(&state.prompt_root)?)
}

pub(super) async fn render_pi_compaction_prompt(
    state: &AppState,
    config: &SessionConfig,
) -> anyhow::Result<String> {
    let template = load_pi_compaction_md(&state.prompt_root)?;
    Ok(render_prompt(
        &template,
        &prompt_context(state, config).await?,
    ))
}

pub(super) async fn prompt_context(
    state: &AppState,
    config: &SessionConfig,
) -> anyhow::Result<PromptContext> {
    let profile = prompt_profile(config);
    let snapshot = crate::provider_runtime::mcp_snapshot_for_session(config)?;
    let mut servers = std::collections::BTreeMap::<String, Vec<String>>::new();
    for tool in &snapshot.manifest().tools {
        servers
            .entry(tool.server_id.clone())
            .or_default()
            .push(tool.exposed_name.clone());
    }
    let mcp_servers = servers
        .into_iter()
        .map(|(server, tools)| PromptMcpServer { server, tools })
        .collect();
    // Workspace skills live in the checked-out repo on the session's runtime, so
    // enumerate them over the protocol rather than the control plane's own disk.
    let workspace_dirs = config
        .workspaces
        .iter()
        .map(|workspace| workspace.workspace_dir.clone())
        .collect::<Vec<_>>();
    let runtime_raw = state
        .runtime_hosts
        .read_runtime_skills(&config.runtime_id, &config.workspace_id, &workspace_dirs)
        .await?;
    Ok(PromptContext {
        profile,
        cwd: PathBuf::from(&config.workspace_id),
        has_project: config.project_id.is_some(),
        workspaces: config
            .workspaces
            .iter()
            .map(|workspace| PromptWorkspace {
                kind: match workspace.kind {
                    WorkspaceKind::Git => PromptWorkspaceKind::Git,
                    WorkspaceKind::Local => PromptWorkspaceKind::Local,
                },
                workspace_dir: workspace.workspace_dir.clone(),
                remote_url: workspace.remote_url.clone(),
                remote_branch: workspace.remote_branch.clone(),
                source_path: workspace.source_path.clone(),
                base_sha: workspace.base_sha.clone(),
                local_branch: workspace.local_branch.clone(),
            })
            .collect(),
        tools: tool_specs(state, config.provider.kind, profile),
        skills: load_prompt_skills(&state.config_root, &runtime_raw, profile),
        subagent_roles: load_configured_subagent_role_catalog(&state.config_root),
        mcp_servers,
    })
}

pub(crate) fn prompt_profile(config: &SessionConfig) -> PromptProfile {
    if config
        .metadata
        .get("subagent")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return PromptProfile::Subagent;
    }
    if let Some(profile) = config
        .metadata
        .get("prompt_profile")
        .and_then(Value::as_str)
    {
        return match profile {
            "subagent" => PromptProfile::Subagent,
            _ => PromptProfile::Parent,
        };
    }
    PromptProfile::Parent
}

pub(crate) async fn effective_prompt_profile(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
) -> anyhow::Result<PromptProfile> {
    if state
        .repo
        .session_subagent_type(session_id)
        .await?
        .is_some()
    {
        return Ok(PromptProfile::Subagent);
    }
    Ok(prompt_profile(config))
}

pub(crate) fn provider_tools_for_session(
    state: &AppState,
    provider: ProviderKind,
    profile: PromptProfile,
) -> Vec<ProviderTool> {
    provider_tools_for_profile(state.tools.provider_tools_for_provider(provider), profile)
}

fn provider_tools_for_profile(
    tools: Vec<ProviderTool>,
    profile: PromptProfile,
) -> Vec<ProviderTool> {
    tools
        .into_iter()
        .filter(|tool| tool_allowed_for_profile(tool, profile))
        .collect()
}

fn tool_allowed_for_profile(tool: &ProviderTool, profile: PromptProfile) -> bool {
    if profile == PromptProfile::Parent {
        return true;
    }
    !matches!(
        tool.canonical_name.as_str(),
        "delegate_writing_task"
            | "delegate_readonly_tasks"
            | "inspect_delegation"
            | "cancel_delegation"
            | "steer_subagent"
            | "interrupt_subagent"
    )
}

fn tool_specs(state: &AppState, provider: ProviderKind, profile: PromptProfile) -> Vec<ToolSpec> {
    tool_specs_from_provider_tools(provider_tools_for_session(state, provider, profile))
}

fn tool_specs_from_provider_tools(tools: Vec<ProviderTool>) -> Vec<ToolSpec> {
    tools
        .into_iter()
        .map(|tool| {
            ToolSpec::new(
                tool.name,
                tool.description,
                tool.input_schema,
                tool.canonical_name,
                tool.prompt_alias.unwrap_or_else(|| "other".to_string()),
            )
        })
        .collect()
}

fn load_prompt_skills(
    config_root: &Path,
    runtime_raw: &[RawSkillFile],
    profile: PromptProfile,
) -> Vec<Skill> {
    let mut skills = parse_runtime_skills(runtime_raw);
    if profile == PromptProfile::Parent {
        extend_with_fallback_skills(
            &mut skills,
            load_global_skills_from_dir(&config_root.join("workflows")),
        );
    }
    skills
}

fn load_configured_subagent_role_catalog(config_root: &Path) -> Vec<SubagentRole> {
    load_global_skills_from_dir(&config_root.join("subagent-roles"))
        .into_iter()
        .map(|skill| SubagentRole::new(skill.name, skill.description))
        .collect()
}

#[derive(Debug)]
pub(super) struct ParsedSkillFile {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) body: String,
    pub(super) frontmatter: std::collections::BTreeMap<String, String>,
}

/// Build the skill catalog from the runtime's raw `SKILL.md` set. `None`
/// workspace entries become global (home) skills; `Some` entries become
/// workspace skills. The `rel_path` is stashed in `file_path` as an opaque
/// identifier so LoadSkill can match a body back to this same set.
pub(super) fn parse_runtime_skills(raw: &[RawSkillFile]) -> Vec<Skill> {
    raw.iter()
        .filter_map(|file| {
            let parsed = parse_skill_contents(&file.contents)?;
            let skill = match &file.workspace {
                Some(workspace) => Skill::workspace(
                    workspace.clone(),
                    parsed.name,
                    parsed.description,
                    &file.rel_path,
                ),
                None => Skill::global(parsed.name, parsed.description, &file.rel_path),
            };
            Some(skill)
        })
        .collect()
}

pub(super) fn load_global_skills_from_dir(dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    add_skills_from_agents_dir(dir, None, &mut skills);
    skills
}

fn add_skills_from_agents_dir(dir: &Path, workspace: Option<&str>, skills: &mut Vec<Skill>) {
    if !dir.is_dir() {
        return;
    }

    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries = entries.flatten().collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') || !path.is_dir() {
            continue;
        }
        let skill_file = path.join("SKILL.md");
        if !skill_file.is_file() {
            continue;
        }
        if let Some(skill) = load_skill_file(&skill_file, workspace) {
            skills.push(skill);
        }
    }
}

fn load_skill_file(path: &Path, workspace: Option<&str>) -> Option<Skill> {
    let parsed = load_parsed_skill_file(path).ok()?;
    if parsed.name.is_empty() || parsed.description.is_empty() {
        return None;
    }
    let skill = match workspace {
        Some(workspace) => {
            Skill::workspace(workspace.to_string(), parsed.name, parsed.description, path)
        }
        None => Skill::global(parsed.name, parsed.description, path),
    };
    Some(skill)
}

pub(super) fn load_parsed_skill_file(path: &Path) -> anyhow::Result<ParsedSkillFile> {
    let raw =
        std::fs::read_to_string(path).with_context(|| format!("read skill {}", path.display()))?;
    parse_skill_contents(&raw).ok_or_else(|| {
        anyhow!(
            "skill {} missing frontmatter name or description",
            path.display()
        )
    })
}

/// Parse `SKILL.md` contents into name/description/body. Returns `None` when the
/// frontmatter lacks a non-empty name or description.
pub(super) fn parse_skill_contents(raw: &str) -> Option<ParsedSkillFile> {
    let (frontmatter, body) = split_frontmatter(raw);
    let frontmatter = parse_simple_frontmatter(frontmatter.unwrap_or_default());
    let name = frontmatter.get("name")?.trim().to_string();
    let description = frontmatter.get("description")?.trim().to_string();
    if name.is_empty() || description.is_empty() {
        return None;
    }
    Some(ParsedSkillFile {
        name,
        description,
        body: body.trim().to_string(),
        frontmatter,
    })
}

fn split_frontmatter(raw: &str) -> (Option<&str>, &str) {
    let Some(rest) = raw.strip_prefix("---\n") else {
        return (None, raw);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, raw);
    };
    let mut body = &rest[end + "\n---".len()..];
    if let Some(stripped) = body.strip_prefix("\r\n") {
        body = stripped;
    } else if let Some(stripped) = body.strip_prefix('\n') {
        body = stripped;
    }
    (Some(&rest[..end]), body)
}

fn parse_simple_frontmatter(frontmatter: &str) -> std::collections::BTreeMap<String, String> {
    let mut map = std::collections::BTreeMap::new();
    for line in frontmatter.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        map.insert(
            key.trim().to_string(),
            value
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .to_string(),
        );
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_tools::ToolRegistry;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn parse_runtime_skills_classifies_home_and_workspace_sources() {
        // Discovery (which dirs are scanned, non-recursively) happens on the
        // runtime; the control plane only classifies what the runtime returns.
        let raw = vec![
            raw_skill(None, "global-only", "from home"),
            raw_skill(Some("dynamo"), "shared", "from dynamo"),
            raw_skill(Some("NCCL"), "shared", "from NCCL"),
        ];
        let skills = parse_runtime_skills(&raw);
        assert!(skills
            .iter()
            .any(|skill| skill.workspace.is_none() && skill.name == "global-only"));
        assert!(skills.iter().any(|skill| {
            skill.workspace.as_deref() == Some("dynamo")
                && skill.name == "shared"
                && skill.description == "from dynamo"
        }));
        assert!(skills.iter().any(|skill| {
            skill.workspace.as_deref() == Some("NCCL")
                && skill.name == "shared"
                && skill.description == "from NCCL"
        }));
        // Home and workspace skills never collide: their exposed names live in
        // different namespaces (`shared` vs `dynamo/shared`).
        assert!(skills
            .iter()
            .any(|skill| skill.exposed_name() == "dynamo/shared"));
        assert!(!skills.iter().any(|skill| skill.exposed_name() == "shared"));
    }

    #[test]
    fn parse_runtime_skills_skips_entries_missing_frontmatter() {
        let raw = vec![
            raw_skill(Some("repo"), "ok", "good"),
            RawSkillFile {
                workspace: Some("repo".to_string()),
                rel_path: "repo/.agents/skills/no-desc/SKILL.md".to_string(),
                contents: "---\nname: no-desc\n---\n\nbody".to_string(),
            },
            RawSkillFile {
                workspace: None,
                rel_path: "~/.agents/skills/blank/SKILL.md".to_string(),
                contents: "no frontmatter at all\n".to_string(),
            },
        ];
        let skills = parse_runtime_skills(&raw);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "ok");
    }

    #[test]
    fn workflow_skills_are_prompt_skills_but_roles_stay_hidden() {
        let prompt_root = make_temp_dir("workflow-skills-in-index");
        let outer = prompt_root.join("outer");
        let workspace = outer.join("repo");
        std::fs::create_dir_all(&workspace).expect("workspace dir");
        write_skill(
            &prompt_root.join("workflows/workflow-explore/SKILL.md"),
            "workflow-explore",
            "parallel read-only exploration",
        );
        write_skill(
            &prompt_root.join("subagent-roles/explore/SKILL.md"),
            "explore",
            "default subagent role",
        );
        let runtime_raw = vec![raw_skill(Some("repo"), "visible", "normal skill")];

        let skills = load_prompt_skills(&prompt_root, &runtime_raw, PromptProfile::Parent);
        assert!(skills
            .iter()
            .any(|skill| skill.workspace.is_none() && skill.name == "workflow-explore"));
        assert!(skills.iter().any(|skill| skill.name == "visible"));
        assert!(!skills.iter().any(|skill| skill.name == "explore"));

        let subagent_skills =
            load_prompt_skills(&prompt_root, &runtime_raw, PromptProfile::Subagent);
        assert!(!subagent_skills
            .iter()
            .any(|skill| skill.name == "workflow-explore"));
        assert!(subagent_skills.iter().any(|skill| skill.name == "visible"));

        let roles = load_configured_subagent_role_catalog(&prompt_root);
        assert!(roles
            .iter()
            .any(|role| role.name == "explore" && role.description == "default subagent role"));

        std::fs::remove_dir_all(prompt_root).ok();
    }

    #[test]
    fn parent_prompt_uses_only_configured_daemon_catalogs() {
        let prompt_root = make_temp_dir("non-config-prompt-catalog");
        let config_root = make_temp_dir("config-prompt-catalog");
        let outer = prompt_root.join("outer");
        std::fs::create_dir_all(&outer).expect("outer");
        write_skill(
            &prompt_root.join("workflows/fallback/SKILL.md"),
            "fallback",
            "bundled fallback",
        );
        write_skill(
            &config_root.join("workflows/review/SKILL.md"),
            "review",
            "configured workflow",
        );
        write_skill(
            &config_root.join("subagent-roles/reviewer/SKILL.md"),
            "reviewer",
            "configured reviewer",
        );
        let skills = load_prompt_skills(&config_root, &[], PromptProfile::Parent);
        assert_eq!(
            skills
                .iter()
                .filter(|skill| skill.workspace.is_none() && skill.name == "review")
                .count(),
            1
        );
        assert!(skills
            .iter()
            .any(|skill| { skill.name == "review" && skill.description == "configured workflow" }));
        assert!(!skills.iter().any(|skill| skill.name == "fallback"));
        let roles = load_configured_subagent_role_catalog(&config_root);
        assert_eq!(
            roles.iter().filter(|role| role.name == "reviewer").count(),
            1
        );
        assert!(roles
            .iter()
            .any(|role| role.name == "reviewer" && role.description == "configured reviewer"));

        std::fs::remove_dir_all(prompt_root).ok();
        std::fs::remove_dir_all(config_root).ok();
    }

    #[test]
    fn provider_tool_filter_matches_prompt_tool_specs_for_profiles() {
        let registry = ToolRegistry::with_builtin_tools();
        let all_tools = registry.provider_tools_for_provider(ProviderKind::OpenAi);

        let parent_provider_tools =
            provider_tools_for_profile(all_tools.clone(), PromptProfile::Parent);
        let parent_spec_names = tool_specs_from_provider_tools(parent_provider_tools.clone())
            .into_iter()
            .map(|tool| tool.canonical_name)
            .collect::<Vec<_>>();
        let parent_provider_names = parent_provider_tools
            .iter()
            .map(|tool| tool.canonical_name.clone())
            .collect::<Vec<_>>();
        assert_eq!(parent_spec_names, parent_provider_names);
        assert!(parent_spec_names.contains(&"delegate_writing_task".to_string()));
        assert!(parent_spec_names.contains(&"delegate_readonly_tasks".to_string()));
        assert!(parent_spec_names.contains(&"inspect_delegation".to_string()));
        assert!(parent_spec_names.contains(&"cancel_delegation".to_string()));
        assert!(parent_spec_names.contains(&"steer_subagent".to_string()));
        assert!(parent_spec_names.contains(&"interrupt_subagent".to_string()));

        let subagent_provider_tools =
            provider_tools_for_profile(all_tools, PromptProfile::Subagent);
        let subagent_spec_names = tool_specs_from_provider_tools(subagent_provider_tools.clone())
            .into_iter()
            .map(|tool| tool.canonical_name)
            .collect::<Vec<_>>();
        let subagent_provider_names = subagent_provider_tools
            .iter()
            .map(|tool| tool.canonical_name.clone())
            .collect::<Vec<_>>();
        assert_eq!(subagent_spec_names, subagent_provider_names);
        assert!(subagent_spec_names.contains(&"LoadSkill".to_string()));
        assert!(!subagent_spec_names.contains(&"delegate_writing_task".to_string()));
        assert!(!subagent_spec_names.contains(&"delegate_readonly_tasks".to_string()));
        assert!(!subagent_spec_names.contains(&"inspect_delegation".to_string()));
        assert!(!subagent_spec_names.contains(&"cancel_delegation".to_string()));
        assert!(!subagent_spec_names.contains(&"steer_subagent".to_string()));
        assert!(!subagent_spec_names.contains(&"interrupt_subagent".to_string()));
        assert_ne!(
            crate::provider_runtime::provider_toolset_fingerprint(&parent_provider_tools),
            crate::provider_runtime::provider_toolset_fingerprint(&subagent_provider_tools),
            "parent usage anchors must not be reused by a child with a different first-party profile"
        );
    }

    #[test]
    fn prompt_profile_subagent_flag_wins_over_parent_profile() {
        let mut config = SessionConfig {
            project_id: None,
            runtime_id: "runtime-test".to_string(),
            workspace_id: "/tmp".to_string(),
            workspaces: Vec::new(),
            system_prompt: String::new(),
            provider: agent_vocab::ProviderConfig {
                kind: ProviderKind::OpenAi,
                model: "gpt-5.2".to_string(),
                reasoning_effort: agent_vocab::ReasoningEffort::Medium,
                max_tokens: None,
                prompt_cache: None,
            },
            metadata: serde_json::json!({
                "prompt_profile": "parent",
                "subagent": true,
            }),
            mcp_manifest: None,
        };

        assert_eq!(prompt_profile(&config), PromptProfile::Subagent);
        config.metadata = serde_json::json!({ "prompt_profile": "subagent" });
        assert_eq!(prompt_profile(&config), PromptProfile::Subagent);
        config.metadata = serde_json::json!({ "subagent": true });
        assert_eq!(prompt_profile(&config), PromptProfile::Subagent);
    }

    fn raw_skill(workspace: Option<&str>, name: &str, description: &str) -> RawSkillFile {
        let rel_path = match workspace {
            Some(workspace) => format!("{workspace}/.agents/skills/{name}/SKILL.md"),
            None => format!("~/.agents/skills/{name}/SKILL.md"),
        };
        RawSkillFile {
            workspace: workspace.map(str::to_string),
            rel_path,
            contents: format!(
                "---\nname: {name}\ndescription: {description}\nignored: true\n---\n\n# {name}\n"
            ),
        }
    }

    fn write_skill(path: &Path, name: &str, description: &str) {
        std::fs::create_dir_all(path.parent().expect("skill parent")).expect("skill dir");
        std::fs::write(
            path,
            format!(
                "---\nname: {name}\ndescription: {description}\nignored: true\n---\n\n# {name}\n"
            ),
        )
        .expect("write skill");
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("time")
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("pi-relay-{prefix}-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }
}
