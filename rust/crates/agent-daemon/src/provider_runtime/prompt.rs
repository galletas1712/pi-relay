use std::path::PathBuf;

use agent_prompt::{
    load_pi_compaction_md, load_pi_md, render_prompt, PromptContext, PromptMcpServer,
    PromptProfile, PromptWorkspace, PromptWorkspaceKind, Skill, SubagentRole, ToolSpec,
};
use agent_runtime_protocol::{RawInstructionFile, RawSkillFile, SkillKind};
use agent_store::{SessionConfig, WorkspaceKind};
use agent_tools::ProviderTool;
use agent_vocab::ProviderKind;
use serde::Deserialize;
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
    let runtime_context = state
        .runtime_hosts
        .read_runtime_context(&config.runtime_id, &config.workspace_id, &workspace_dirs)
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
        agents_md: render_runtime_instructions(&runtime_context.instructions),
        tools: tool_specs(state, config.provider.kind, profile),
        skills: parse_runtime_skills(&runtime_context.skills),
        subagent_roles: load_subagent_role_catalog(&runtime_context.skills),
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

fn load_subagent_role_catalog(raw: &[RawSkillFile]) -> Vec<SubagentRole> {
    raw.iter()
        .filter(|file| file.kind == SkillKind::SubagentRole)
        .filter_map(|file| {
            let parsed = parse_skill_contents(&file.contents)?;
            (parsed.name == file.package_name).then_some(parsed)
        })
        .map(|parsed| SubagentRole::new(parsed.name, parsed.description))
        .collect()
}

fn render_runtime_instructions(files: &[RawInstructionFile]) -> String {
    files
        .iter()
        .filter(|file| !file.contents.trim().is_empty())
        .map(|file| match &file.workspace {
            Some(workspace) => format!("### {workspace}\n\n{}", file.contents.trim()),
            None => file.contents.trim().to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

#[derive(Debug)]
pub(super) struct ParsedSkillFile {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) body: String,
    pub(super) frontmatter: SkillFrontmatter,
}

#[derive(Debug, Deserialize)]
pub(super) struct SkillFrontmatter {
    pub(super) name: String,
    pub(super) description: String,
    #[serde(default)]
    pub(super) kind: Option<String>,
    #[serde(default)]
    pub(super) model: Option<String>,
    #[serde(default)]
    pub(super) reasoning_effort: Option<String>,
    #[serde(default)]
    pub(super) max_tokens: Option<u32>,
    #[serde(default)]
    pub(super) skills: Vec<String>,
}

/// Build the skill catalog from the runtime's raw `SKILL.md` set. `None`
/// workspace entries become global (home) skills; `Some` entries become
/// workspace skills. The `rel_path` is stashed in `file_path` as an opaque
/// identifier so LoadSkill can match a body back to this same set.
pub(super) fn parse_runtime_skills(raw: &[RawSkillFile]) -> Vec<Skill> {
    raw.iter()
        .filter(|file| file.kind == SkillKind::Skill)
        .filter_map(|file| {
            let parsed = parse_skill_contents(&file.contents)?;
            if parsed.name != file.package_name {
                return None;
            }
            let skill = match &file.workspace {
                Some(workspace) => Skill::workspace(
                    workspace.clone(),
                    parsed.name,
                    parsed.description,
                    &file.path,
                ),
                None => Skill::global(parsed.name, parsed.description, &file.path),
            };
            Some(skill)
        })
        .collect()
}

/// Parse `SKILL.md` contents into name/description/body. Returns `None` when the
/// frontmatter lacks a non-empty name or description.
pub(super) fn parse_skill_contents(raw: &str) -> Option<ParsedSkillFile> {
    let (frontmatter, body) = split_frontmatter(raw);
    let frontmatter: SkillFrontmatter = serde_yaml::from_str(frontmatter?).ok()?;
    let name = frontmatter.name.trim().to_string();
    let description = frontmatter.description.trim().to_string();
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_tools::ToolRegistry;

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
                kind: SkillKind::Skill,
                origin: agent_runtime_protocol::SkillOrigin::WorkspaceProject,
                workspace: Some("repo".to_string()),
                package_name: "no-desc".to_string(),
                path: "/tmp/repo/.agents/skills/no-desc/SKILL.md".to_string(),
                contents: "---\nname: no-desc\n---\n\nbody".to_string(),
            },
            RawSkillFile {
                kind: SkillKind::Skill,
                origin: agent_runtime_protocol::SkillOrigin::HomeGlobal,
                workspace: None,
                package_name: "blank".to_string(),
                path: "/home/test/.agents/skills/blank/SKILL.md".to_string(),
                contents: "no frontmatter at all\n".to_string(),
            },
        ];
        let skills = parse_runtime_skills(&raw);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].name, "ok");
    }

    #[test]
    fn workflow_skills_are_ordinary_but_roles_stay_hidden() {
        let mut workflow = raw_skill(None, "workflow-explore", "parallel exploration");
        workflow.origin = agent_runtime_protocol::SkillOrigin::RuntimeWorkflow;
        let mut role = raw_skill(None, "explore", "default subagent role");
        role.kind = SkillKind::SubagentRole;
        role.origin = agent_runtime_protocol::SkillOrigin::RuntimeRole;
        let runtime_raw = vec![
            workflow,
            role,
            raw_skill(Some("repo"), "visible", "normal skill"),
        ];

        let skills = parse_runtime_skills(&runtime_raw);
        assert!(skills
            .iter()
            .any(|skill| skill.workspace.is_none() && skill.name == "workflow-explore"));
        assert!(skills.iter().any(|skill| skill.name == "visible"));
        assert!(!skills.iter().any(|skill| skill.name == "explore"));

        let roles = load_subagent_role_catalog(&runtime_raw);
        assert!(roles
            .iter()
            .any(|role| role.name == "explore" && role.description == "default subagent role"));
    }

    #[test]
    fn runtime_instructions_keep_global_then_workspace_order() {
        let files = vec![
            RawInstructionFile {
                workspace: None,
                path: "/config/runtime/AGENTS.md".to_string(),
                contents: "global rules\n".to_string(),
            },
            RawInstructionFile {
                workspace: Some("repo".to_string()),
                path: "/workspace/repo/AGENTS.md".to_string(),
                contents: "repo rules\n".to_string(),
            },
        ];

        assert_eq!(
            render_runtime_instructions(&files),
            "global rules\n\n### repo\n\nrepo rules"
        );
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
        let (origin, path) = match workspace {
            Some(workspace) => (
                agent_runtime_protocol::SkillOrigin::WorkspaceProject,
                format!("/tmp/{workspace}/.agents/skills/{name}/SKILL.md"),
            ),
            None => (
                agent_runtime_protocol::SkillOrigin::HomeGlobal,
                format!("/home/test/.agents/skills/{name}/SKILL.md"),
            ),
        };
        RawSkillFile {
            kind: SkillKind::Skill,
            origin,
            workspace: workspace.map(str::to_string),
            package_name: name.to_string(),
            path,
            contents: format!(
                "---\nname: {name}\ndescription: {description}\nignored: true\n---\n\n# {name}\n"
            ),
        }
    }
}
