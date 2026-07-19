use std::path::{Path, PathBuf};

use agent_prompt::{
    load_pi_compaction_md, load_pi_md, render_prompt, PromptContext, PromptMcpServer,
    PromptProfile, PromptWorkspace, PromptWorkspaceKind, Skill, SubagentRole, ToolSpec,
};
use agent_store::{SessionConfig, SessionWorkspace, WorkspaceKind};
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

/// Add daemon-owned fallback skills only when an explicit home/workspace skill
/// does not already expose the same name.
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

/// The first directory wins by skill name. This is only for daemon-owned
/// fallback catalogs; workspace/home skills retain their existing behavior.
pub(super) fn load_global_skills_from_dirs(preferred: &Path, fallback: &Path) -> Vec<Skill> {
    let mut skills = load_global_skills_from_dir(preferred);
    let mut names = skills
        .iter()
        .map(|skill| skill.name.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for skill in load_global_skills_from_dir(fallback) {
        if names.insert(skill.name.clone()) {
            skills.push(skill);
        }
    }
    skills
}

pub(crate) fn render_pi_prompt(state: &AppState, config: &SessionConfig) -> anyhow::Result<String> {
    let template = load_pi_md(&state.prompt_root)?;
    Ok(render_prompt(&template, &prompt_context(state, config)?))
}

pub(crate) fn current_pi_template(state: &AppState) -> anyhow::Result<String> {
    Ok(load_pi_md(&state.prompt_root)?)
}

pub(super) fn render_pi_compaction_prompt(
    state: &AppState,
    config: &SessionConfig,
) -> anyhow::Result<String> {
    let template = load_pi_compaction_md(&state.prompt_root)?;
    Ok(render_prompt(&template, &prompt_context(state, config)?))
}

pub(super) fn prompt_context(
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
        skills: load_prompt_skills(&state.config_root, &state.prompt_root, config, profile),
        subagent_roles: load_packaged_subagent_role_catalog(&state.config_root, &state.prompt_root),
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
    prompt_root: &Path,
    config: &SessionConfig,
    profile: PromptProfile,
) -> Vec<Skill> {
    let mut skills = load_skills_for_session_workspaces(
        &PathBuf::from(&config.workspace_id),
        &config.workspaces,
    );
    if profile == PromptProfile::Parent {
        extend_with_fallback_skills(
            &mut skills,
            load_global_skills_from_dirs(
                &config_root.join("workflows"),
                &prompt_root.join("workflows"),
            ),
        );
    }
    skills
}

fn load_packaged_subagent_role_catalog(
    config_root: &Path,
    prompt_root: &Path,
) -> Vec<SubagentRole> {
    load_global_skills_from_dirs(
        &config_root.join("subagent-roles"),
        &prompt_root.join("subagent-roles"),
    )
    .into_iter()
    .map(|skill| SubagentRole::new(skill.name, skill.description))
    .collect()
}

#[derive(Debug)]
pub(super) struct ParsedSkillFile {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) body: String,
}

#[cfg(test)]
#[allow(dead_code)]
pub(super) fn load_skills_for_workspace_roots(
    workspace_id: &Path,
    workspace_dirs: &[String],
) -> Vec<Skill> {
    let workspaces = workspace_dirs
        .iter()
        .map(|workspace_dir| SessionWorkspace::local(workspace_dir.clone(), String::new()))
        .collect::<Vec<_>>();
    load_skills_for_session_workspaces_with_home(workspace_id, &workspaces, home_dir().as_deref())
}

pub(super) fn load_skills_for_session_workspaces(
    workspace_id: &Path,
    workspaces: &[SessionWorkspace],
) -> Vec<Skill> {
    load_skills_for_session_workspaces_with_home(workspace_id, workspaces, home_dir().as_deref())
}

pub(super) fn load_global_skills_from_dir(dir: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    add_skills_from_agents_dir(dir, None, &mut skills);
    skills
}

#[cfg(test)]
pub(super) fn load_skills_for_workspace_roots_with_home(
    workspace_id: &Path,
    workspace_dirs: &[String],
    home: Option<&Path>,
) -> Vec<Skill> {
    let workspaces = workspace_dirs
        .iter()
        .map(|workspace_dir| SessionWorkspace::local(workspace_dir.clone(), String::new()))
        .collect::<Vec<_>>();
    load_skills_for_session_workspaces_with_home(workspace_id, &workspaces, home)
}

pub(super) fn load_skills_for_session_workspaces_with_home(
    workspace_id: &Path,
    workspaces: &[SessionWorkspace],
    home: Option<&Path>,
) -> Vec<Skill> {
    let workspace_id = normalize_existing_dir(workspace_id);
    let mut skills = Vec::new();

    if let Some(home) = home {
        let home_skills_dir = home.join(".agents/skills");
        add_skills_from_agents_dir(&home_skills_dir, None, &mut skills);
    }

    for workspace in workspaces {
        let workspace_root = workspace_id.join(&workspace.workspace_dir);
        let skills_dir = workspace_root.join(".agents/skills");
        add_skills_from_agents_dir(
            &skills_dir,
            Some(workspace.workspace_dir.as_str()),
            &mut skills,
        );
    }

    skills
}

fn home_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home))
}

fn normalize_existing_dir(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
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
    let (frontmatter, body) = split_frontmatter(&raw);
    let frontmatter = parse_simple_frontmatter(frontmatter.unwrap_or_default());
    let name = frontmatter
        .get("name")
        .ok_or_else(|| anyhow!("skill {} missing frontmatter name", path.display()))?
        .trim()
        .to_string();
    let description = frontmatter
        .get("description")
        .ok_or_else(|| anyhow!("skill {} missing frontmatter description", path.display()))?
        .trim()
        .to_string();
    Ok(ParsedSkillFile {
        name,
        description,
        body: body.trim().to_string(),
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
    fn discovers_home_and_workspace_agents_skills_without_recursive_workspace_scan() {
        let root = make_temp_dir("skills-discovery");
        let home = root.join("home");
        let outer = root.join("outer");
        let dynamo = outer.join("dynamo");
        let nccl = outer.join("NCCL");
        std::fs::create_dir_all(&dynamo).expect("dynamo dir");
        std::fs::create_dir_all(&nccl).expect("NCCL dir");

        write_skill(
            &home.join(".agents/skills/global-only/SKILL.md"),
            "global-only",
            "from home",
        );
        write_skill(
            &outer.join(".agents/skills/outer-only/SKILL.md"),
            "outer-only",
            "not discovered because outer cwd is not a workspace",
        );
        write_skill(
            &dynamo.join(".agents/skills/shared/SKILL.md"),
            "shared",
            "from dynamo",
        );
        write_skill(
            &nccl.join(".agents/skills/shared/SKILL.md"),
            "shared",
            "from NCCL",
        );
        write_skill(
            &dynamo.join("child/.agents/skills/child-only/SKILL.md"),
            "child-only",
            "not discovered without recursive workspace scanning",
        );
        write_skill(
            &dynamo.join(".agents/skills/group/deep/SKILL.md"),
            "deep",
            "not discovered because skill packages are immediate children only",
        );

        let skills = load_skills_for_workspace_roots_with_home(
            &outer,
            &["dynamo".to_string(), "NCCL".to_string()],
            Some(&home),
        );
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
        assert!(!skills.iter().any(|skill| skill.name == "outer-only"));
        assert!(!skills.iter().any(|skill| skill.name == "child-only"));
        assert!(!skills.iter().any(|skill| skill.name == "deep"));

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn only_loads_skill_directories_under_agents_skills() {
        let root = make_temp_dir("skills-root-files");
        let outer = root.join("outer");
        let workspace = outer.join("repo");
        std::fs::create_dir_all(workspace.join(".agents/skills")).expect("skills dir");
        std::fs::write(
            workspace.join(".agents/skills/root-file.md"),
            "---\nname: root-file\ndescription: ignored\n---\n",
        )
        .expect("root skill file");
        write_skill(
            &workspace.join(".agents/skills/nested/SKILL.md"),
            "nested",
            "loaded",
        );

        let skills = load_skills_for_workspace_roots_with_home(&outer, &["repo".to_string()], None);
        assert!(skills
            .iter()
            .any(|skill| skill.workspace.as_deref() == Some("repo") && skill.name == "nested"));
        assert!(!skills.iter().any(|skill| skill.name == "root-file"));

        std::fs::remove_dir_all(root).ok();
    }

    #[test]
    fn subagent_role_defaults_are_not_prompt_skills() {
        let root = make_temp_dir("subagent-roles-not-skills");
        let outer = root.join("outer");
        let workspace = outer.join("repo");
        write_skill(
            &outer.join("subagent-roles/tester/SKILL.md"),
            "tester",
            "default subagent role",
        );
        write_skill(
            &workspace.join("subagent-roles/reviewer/SKILL.md"),
            "reviewer",
            "workspace-local default-like role",
        );
        write_skill(
            &workspace.join(".agents/skills/visible/SKILL.md"),
            "visible",
            "normal skill",
        );

        let skills = load_skills_for_workspace_roots_with_home(&outer, &["repo".to_string()], None);
        assert!(skills.iter().any(|skill| skill.name == "visible"));
        assert!(!skills.iter().any(|skill| skill.name == "tester"));
        assert!(!skills.iter().any(|skill| skill.name == "reviewer"));

        std::fs::remove_dir_all(root).ok();
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
        write_skill(
            &workspace.join(".agents/skills/visible/SKILL.md"),
            "visible",
            "normal skill",
        );

        let config = SessionConfig {
            project_id: None,
            runtime_id: "runtime-test".to_string(),
            workspace_id: outer.to_string_lossy().to_string(),
            workspaces: vec![SessionWorkspace::local("repo", "")],
            system_prompt: String::new(),
            provider: agent_vocab::ProviderConfig {
                kind: ProviderKind::Claude,
                model: "claude-opus-4-8".to_string(),
                reasoning_effort: agent_vocab::ReasoningEffort::Medium,
                max_tokens: None,
                prompt_cache: None,
            },
            metadata: serde_json::Value::Null,
            mcp_manifest: None,
        };

        let skills = load_prompt_skills(&prompt_root, &prompt_root, &config, PromptProfile::Parent);
        assert!(skills
            .iter()
            .any(|skill| skill.workspace.is_none() && skill.name == "workflow-explore"));
        assert!(skills.iter().any(|skill| skill.name == "visible"));
        assert!(!skills.iter().any(|skill| skill.name == "explore"));

        let subagent_skills =
            load_prompt_skills(&prompt_root, &prompt_root, &config, PromptProfile::Subagent);
        assert!(!subagent_skills
            .iter()
            .any(|skill| skill.name == "workflow-explore"));
        assert!(subagent_skills.iter().any(|skill| skill.name == "visible"));

        let roles = load_packaged_subagent_role_catalog(&prompt_root, &prompt_root);
        assert!(roles
            .iter()
            .any(|role| role.name == "explore" && role.description == "default subagent role"));

        std::fs::remove_dir_all(prompt_root).ok();
    }

    #[test]
    fn config_catalog_precedes_packaged_catalog_in_parent_prompt() {
        let prompt_root = make_temp_dir("packaged-prompt-catalog");
        let config_root = make_temp_dir("config-prompt-catalog");
        let outer = prompt_root.join("outer");
        std::fs::create_dir_all(&outer).expect("outer");
        write_skill(
            &prompt_root.join("workflows/review/SKILL.md"),
            "review",
            "bundled workflow",
        );
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
            &prompt_root.join("subagent-roles/reviewer/SKILL.md"),
            "reviewer",
            "bundled reviewer",
        );
        write_skill(
            &config_root.join("subagent-roles/reviewer/SKILL.md"),
            "reviewer",
            "configured reviewer",
        );
        let config = SessionConfig {
            project_id: None,
            runtime_id: "runtime-test".to_string(),
            workspace_id: outer.to_string_lossy().to_string(),
            workspaces: Vec::new(),
            system_prompt: String::new(),
            provider: agent_vocab::ProviderConfig {
                kind: ProviderKind::OpenAi,
                model: "test".to_string(),
                reasoning_effort: agent_vocab::ReasoningEffort::Medium,
                max_tokens: None,
                prompt_cache: None,
            },
            metadata: serde_json::Value::Null,
            mcp_manifest: None,
        };

        let skills = load_prompt_skills(&config_root, &prompt_root, &config, PromptProfile::Parent);
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
        assert!(skills.iter().any(|skill| skill.name == "fallback"));
        let roles = load_packaged_subagent_role_catalog(&config_root, &prompt_root);
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
