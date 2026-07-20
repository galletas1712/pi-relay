use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use agent_prompt::{PromptProfile, Skill};
use agent_runtime_protocol::RawSkillFile;
use agent_vocab::{ToolCall, ToolResultMessage};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use super::prompt::{
    extend_with_fallback_skills, load_global_skills_from_dirs, load_parsed_skill_file,
    parse_runtime_skills, parse_skill_contents,
};

pub(crate) fn load_skill_result(
    prompt_root: &Path,
    config_root: &Path,
    runtime_raw: &[RawSkillFile],
    loaded_skills: &std::collections::BTreeSet<String>,
    call: &ToolCall,
    profile: PromptProfile,
) -> ToolResultMessage {
    match load_skill_output(
        prompt_root,
        config_root,
        runtime_raw,
        loaded_skills,
        call,
        profile,
    ) {
        Ok(output) => ToolResultMessage::success(call.id.clone(), "LoadSkill", output),
        Err(error) => ToolResultMessage::error(call.id.clone(), "LoadSkill", error.to_string()),
    }
}

fn is_packaged_workflow_skill(
    config_root: &Path,
    prompt_root: &Path,
    requested_name: &str,
) -> bool {
    load_global_skills_from_dirs(
        &config_root.join("workflows"),
        &prompt_root.join("workflows"),
    )
    .into_iter()
    .any(|skill| skill.exposed_name() == requested_name)
}

fn load_skill_output(
    prompt_root: &Path,
    config_root: &Path,
    runtime_raw: &[RawSkillFile],
    loaded_skills: &std::collections::BTreeSet<String>,
    call: &ToolCall,
    profile: PromptProfile,
) -> Result<String> {
    let args: LoadSkillArgs = serde_json::from_str(&call.args_json)?;
    let name = args.name.trim();
    if name.is_empty() {
        return Err(anyhow!("skill name cannot be empty"));
    }
    let mut skills = parse_runtime_skills(runtime_raw);
    if profile == PromptProfile::Parent {
        extend_with_fallback_skills(
            &mut skills,
            load_global_skills_from_dirs(
                &config_root.join("workflows"),
                &prompt_root.join("workflows"),
            ),
        );
    }
    let skill = match resolve_load_skill(&skills, name) {
        Ok(skill) => skill,
        Err(_error)
            if profile == PromptProfile::Subagent
                && is_packaged_workflow_skill(config_root, prompt_root, name) =>
        {
            return Err(anyhow!(
                "workflow skills are not available to subagent sessions"
            ))
        }
        Err(error) => return Err(error),
    };
    let skill_id = skill_identifier(skill.workspace.as_deref(), &skill.name);
    if loaded_skills.contains(&skill_id) {
        return loaded_skill_json("already_loaded", skill, None);
    }
    let content = skill_body(skill, runtime_raw)?;
    loaded_skill_json("loaded", skill, Some(content.trim()))
}

/// Return a skill's full `SKILL.md` body. Runtime skills (home + workspace)
/// reuse the already-fetched contents; packaged workflow skills read from disk.
fn skill_body(skill: &Skill, runtime_raw: &[RawSkillFile]) -> Result<String> {
    match runtime_raw
        .iter()
        .find(|file| skill.file_path.to_str() == Some(file.rel_path.as_str()))
    {
        Some(file) => Ok(file.contents.clone()),
        None => Ok(std::fs::read_to_string(&skill.file_path)?),
    }
}

fn resolve_load_skill<'a>(skills: &'a [Skill], requested_name: &str) -> Result<&'a Skill> {
    let mut matches = skills
        .iter()
        .filter(|skill| skill.exposed_name() == requested_name)
        .collect::<Vec<_>>();
    match matches.len() {
        1 => Ok(matches.remove(0)),
        0 => Err(anyhow!(
            "skill not found: {requested_name}. Use the exact name from the available skills JSON; workspace skills are prefixed as workspace/name."
        )),
        _ => Err(anyhow!(
            "skill name is ambiguous: {requested_name}. Use the exact name from the available skills JSON."
        )),
    }
}

fn loaded_skill_json(status: &str, skill: &Skill, content: Option<&str>) -> Result<String> {
    let mut object = serde_json::Map::new();
    object.insert("status".to_string(), json!(status));
    object.insert("name".to_string(), json!(skill.exposed_name()));
    object.insert("skill_name".to_string(), json!(skill.name));
    if let Some(workspace) = &skill.workspace {
        object.insert("workspace".to_string(), json!(workspace));
    }
    if let Some(content) = content {
        object.insert("content".to_string(), json!(content));
    }
    Ok(serde_json::to_string_pretty(&Value::Object(object))?)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadSkillArgs {
    name: String,
}

pub(crate) fn skill_identifier(workspace: Option<&str>, name: &str) -> String {
    match workspace {
        Some(workspace) => format!("{workspace}\0{name}"),
        None => format!("\0{name}"),
    }
}

#[derive(Debug)]
pub(crate) struct ResolvedSkillRole {
    pub(crate) name: String,
    pub(crate) workspace: Option<String>,
    pub(crate) description: String,
    pub(crate) file_path: PathBuf,
    pub(crate) content: String,
}

pub(crate) fn resolve_skill_role(
    prompt_root: &Path,
    config_root: &Path,
    runtime_raw: &[RawSkillFile],
    name: &str,
) -> Result<ResolvedSkillRole> {
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("role name cannot be empty"));
    }
    let skills = parse_runtime_skills(runtime_raw);

    let mut exposed_matches = skills
        .iter()
        .filter(|skill| skill.exposed_name() == name)
        .cloned()
        .collect::<Vec<_>>();
    match exposed_matches.len() {
        1 => return role_from_skill(exposed_matches.remove(0), runtime_raw),
        0 => {}
        _ => {
            return Err(anyhow!(
                "role skill is ambiguous: {name}; use a unique prefixed role name"
            ))
        }
    }

    packaged_role(config_root, prompt_root, name)
        .ok_or_else(|| anyhow!("role skill not found: {name}"))
        .and_then(|skill| role_from_skill(skill, runtime_raw))
}

pub(crate) fn validate_subagent_model_roles<'a>(
    prompt_root: &Path,
    config_root: &Path,
    configured_roles: impl IntoIterator<Item = &'a str>,
) -> Result<()> {
    let available = load_packaged_role_skills(config_root, prompt_root)
        .into_iter()
        .map(|skill| skill.name)
        .collect::<BTreeSet<_>>();
    let missing = configured_roles
        .into_iter()
        .filter(|role| !available.contains(*role))
        .collect::<Vec<_>>();
    if missing.is_empty() {
        return Ok(());
    }
    Err(anyhow!(
        "subagent_models entries require matching global role skills: {}",
        missing.join(", ")
    ))
}

fn load_packaged_role_skills(config_root: &Path, prompt_root: &Path) -> Vec<Skill> {
    load_global_skills_from_dirs(
        &config_root.join("subagent-roles"),
        &prompt_root.join("subagent-roles"),
    )
}

fn packaged_role(config_root: &Path, prompt_root: &Path, name: &str) -> Option<Skill> {
    load_packaged_role_skills(config_root, prompt_root)
        .into_iter()
        .find(|skill| skill.name == name)
}

fn role_from_skill(skill: Skill, runtime_raw: &[RawSkillFile]) -> Result<ResolvedSkillRole> {
    let body = match runtime_raw
        .iter()
        .find(|file| skill.file_path.to_str() == Some(file.rel_path.as_str()))
    {
        Some(file) => parse_skill_contents(&file.contents)
            .map(|parsed| parsed.body)
            .ok_or_else(|| anyhow!("role skill {} missing frontmatter", skill.exposed_name()))?,
        None => {
            load_parsed_skill_file(&skill.file_path)
                .with_context(|| format!("read role skill {}", skill.file_path.display()))?
                .body
        }
    };
    Ok(ResolvedSkillRole {
        name: skill.name,
        workspace: skill.workspace,
        description: skill.description,
        file_path: skill.file_path,
        content: body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_vocab::{ToolCall, ToolCallId};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn load_skill_result_loads_content_once() {
        let workspace_id = make_temp_dir("load-skill");
        let workspace = workspace_id.join("repo");
        let skill_dir = workspace.join(".agents/skills/rust-refactor");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: rust-refactor\ndescription: Use for Rust refactors.\n---\n\nPrefer small, tested changes.\n",
        )
        .expect("skill file");

        let call = ToolCall {
            id: ToolCallId::from_u64(1),
            tool_name: "LoadSkill".to_string(),
            args_json: r#"{"name":"repo/rust-refactor"}"#.to_string(),
        };
        let mut loaded = std::collections::BTreeSet::new();
        let runtime_raw = discover_raw(&workspace_id, &["repo"]);

        let first = load_skill_result(
            &workspace_id,
            &workspace_id,
            &runtime_raw,
            &loaded,
            &call,
            PromptProfile::Parent,
        );
        assert_eq!(first.status, agent_vocab::ToolResultStatus::Success);
        let first_json: Value = serde_json::from_str(&first.output).expect("json output");
        assert_eq!(first_json["status"], "loaded");
        assert_eq!(first_json["name"], "repo/rust-refactor");
        assert_eq!(first_json["skill_name"], "rust-refactor");
        assert_eq!(first_json["workspace"], "repo");
        assert!(!first.output.contains("<base_dir>"));
        assert!(first.output.contains("Prefer small, tested changes."));

        loaded.insert(skill_identifier(Some("repo"), "rust-refactor"));
        let second = load_skill_result(
            &workspace_id,
            &workspace_id,
            &runtime_raw,
            &loaded,
            &call,
            PromptProfile::Parent,
        );
        assert_eq!(second.status, agent_vocab::ToolResultStatus::Success);
        let second_json: Value = serde_json::from_str(&second.output).expect("json output");
        assert_eq!(second_json["status"], "already_loaded");
        assert_eq!(second_json["name"], "repo/rust-refactor");
        assert!(second_json.get("content").is_none());

        std::fs::remove_dir_all(workspace_id).ok();
    }

    #[test]
    fn load_skill_result_rejects_workspace_argument() {
        let workspace_id = make_temp_dir("load-skill-workspace-arg-rejected");
        let workspace = workspace_id.join("repo");
        let skill_dir = workspace.join(".agents/skills/rust-refactor");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: rust-refactor\ndescription: Use for Rust refactors.\n---\n\nPrefer small, tested changes.\n",
        )
        .expect("skill file");

        let call = ToolCall {
            id: ToolCallId::from_u64(1),
            tool_name: "LoadSkill".to_string(),
            args_json: r#"{"workspace":"repo","name":"rust-refactor"}"#.to_string(),
        };
        let loaded = std::collections::BTreeSet::new();
        let runtime_raw = discover_raw(&workspace_id, &["repo"]);

        let result = load_skill_result(
            &workspace_id,
            &workspace_id,
            &runtime_raw,
            &loaded,
            &call,
            PromptProfile::Parent,
        );
        assert_eq!(result.status, agent_vocab::ToolResultStatus::Error);
        assert!(result.output.contains("unknown field `workspace`"));

        std::fs::remove_dir_all(workspace_id).ok();
    }

    #[test]
    fn load_skill_result_rejects_unprefixed_workspace_skill() {
        let workspace_id = make_temp_dir("load-skill-unprefixed-workspace-rejected");
        let workspace = workspace_id.join("repo");
        let skill_dir = workspace.join(".agents/skills/rust-refactor");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: rust-refactor\ndescription: Use for Rust refactors.\n---\n\nPrefer small, tested changes.\n",
        )
        .expect("skill file");

        let call = ToolCall {
            id: ToolCallId::from_u64(1),
            tool_name: "LoadSkill".to_string(),
            args_json: r#"{"name":"rust-refactor"}"#.to_string(),
        };
        let loaded = std::collections::BTreeSet::new();
        let runtime_raw = discover_raw(&workspace_id, &["repo"]);

        let result = load_skill_result(
            &workspace_id,
            &workspace_id,
            &runtime_raw,
            &loaded,
            &call,
            PromptProfile::Parent,
        );
        assert_eq!(result.status, agent_vocab::ToolResultStatus::Error);
        assert!(result.output.contains("skill not found: rust-refactor"));

        std::fs::remove_dir_all(workspace_id).ok();
    }

    #[test]
    fn workflow_skills_load_but_do_not_resolve_as_packaged_roles() {
        let prompt_root = make_temp_dir("load-workflow-skill");
        let workspace_id = prompt_root.join("outer");
        std::fs::create_dir_all(&workspace_id).expect("outer cwd");
        let skill_dir = prompt_root.join("workflows/workflow-only-test-role");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: workflow-only-test-role\ndescription: Test workflow skill.\n---\n\nUse delegate_readonly_tasks to fan out.\n",
        )
        .expect("skill file");

        let call = ToolCall {
            id: ToolCallId::from_u64(1),
            tool_name: "LoadSkill".to_string(),
            args_json: r#"{"name":"workflow-only-test-role"}"#.to_string(),
        };
        let loaded = std::collections::BTreeSet::new();

        let result = load_skill_result(
            &prompt_root,
            &prompt_root,
            &[],
            &loaded,
            &call,
            PromptProfile::Parent,
        );
        assert_eq!(result.status, agent_vocab::ToolResultStatus::Success);
        let output: Value = serde_json::from_str(&result.output).expect("json output");
        assert_eq!(output["name"], "workflow-only-test-role");
        assert_eq!(output["skill_name"], "workflow-only-test-role");
        assert!(output.get("workspace").is_none());
        assert!(output["content"]
            .as_str()
            .expect("content string")
            .contains("delegate_readonly_tasks"));

        let error = resolve_skill_role(&prompt_root, &prompt_root, &[], "workflow-only-test-role")
            .expect_err("workflow skills are not packaged subagent roles");
        assert!(error
            .to_string()
            .contains("role skill not found: workflow-only-test-role"));

        std::fs::remove_dir_all(prompt_root).ok();
    }

    #[test]
    fn config_catalog_overrides_bundled_workflows_and_roles_without_duplicates() {
        let prompt_root = make_temp_dir("bundled-catalog");
        let config_root = make_temp_dir("config-catalog");
        let workspace_id = prompt_root.join("outer");
        std::fs::create_dir_all(&workspace_id).expect("outer cwd");
        write_role(
            &prompt_root.join("workflows/review/SKILL.md"),
            "review",
            "Bundled workflow",
            "Bundled workflow body.",
        );
        write_role(
            &prompt_root.join("workflows/fallback/SKILL.md"),
            "fallback",
            "Fallback workflow",
            "Fallback workflow body.",
        );
        write_role(
            &config_root.join("workflows/review/SKILL.md"),
            "review",
            "Configured workflow",
            "Configured workflow body.",
        );
        write_role(
            &prompt_root.join("subagent-roles/reviewer/SKILL.md"),
            "reviewer",
            "Bundled reviewer",
            "Bundled reviewer body.",
        );
        write_role(
            &config_root.join("subagent-roles/reviewer/SKILL.md"),
            "reviewer",
            "Configured reviewer",
            "Configured reviewer body.",
        );

        let call = ToolCall {
            id: ToolCallId::from_u64(1),
            tool_name: "LoadSkill".to_string(),
            args_json: r#"{"name":"review"}"#.to_string(),
        };
        let result = load_skill_result(
            &prompt_root,
            &config_root,
            &[],
            &std::collections::BTreeSet::new(),
            &call,
            PromptProfile::Parent,
        );
        assert_eq!(result.status, agent_vocab::ToolResultStatus::Success);
        assert!(result.output.contains("Configured workflow body."));
        let fallback_call = ToolCall {
            id: ToolCallId::from_u64(2),
            tool_name: "LoadSkill".to_string(),
            args_json: r#"{"name":"fallback"}"#.to_string(),
        };
        let fallback = load_skill_result(
            &prompt_root,
            &config_root,
            &[],
            &std::collections::BTreeSet::new(),
            &fallback_call,
            PromptProfile::Parent,
        );
        assert_eq!(fallback.status, agent_vocab::ToolResultStatus::Success);
        assert!(fallback.output.contains("Fallback workflow body."));

        let role = resolve_skill_role(&prompt_root, &config_root, &[], "reviewer")
            .expect("configured role resolves");
        assert_eq!(role.description, "Configured reviewer");
        assert!(role.content.contains("Configured reviewer body."));

        let workspace_role = workspace_id.join("repo/.agents/skills/reviewer/SKILL.md");
        write_role(
            &workspace_role,
            "reviewer",
            "Workspace reviewer",
            "Workspace reviewer body.",
        );
        let workspace_raw = discover_raw(&workspace_id, &["repo"]);
        let role = resolve_skill_role(&prompt_root, &config_root, &workspace_raw, "repo/reviewer")
            .expect("workspace role resolves first");
        assert_eq!(role.description, "Workspace reviewer");

        std::fs::remove_dir_all(prompt_root).ok();
        std::fs::remove_dir_all(config_root).ok();
    }

    #[test]
    fn subagent_model_roles_must_match_global_role_skills() {
        let prompt_root = make_temp_dir("model-role-source");
        let config_root = make_temp_dir("model-role-config");
        write_role(
            &prompt_root.join("subagent-roles/reviewer/SKILL.md"),
            "reviewer",
            "Reviewer",
            "Review the work.",
        );

        validate_subagent_model_roles(&prompt_root, &config_root, ["reviewer"])
            .expect("global role is valid");

        let error =
            validate_subagent_model_roles(&prompt_root, &config_root, ["repo/reviewer", "missing"])
                .expect_err("runtime and missing roles are not global model-policy keys");
        assert_eq!(
            error.to_string(),
            "subagent_models entries require matching global role skills: repo/reviewer, missing"
        );

        std::fs::remove_dir_all(prompt_root).ok();
        std::fs::remove_dir_all(config_root).ok();
    }

    #[test]
    fn subagent_profile_rejects_workflow_skills() {
        let prompt_root = make_temp_dir("subagent-load-workflow-rejected");
        let workspace_id = prompt_root.join("outer");
        std::fs::create_dir_all(&workspace_id).expect("outer cwd");
        let skill_dir = prompt_root.join("workflows/workflow-implement-review");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: workflow-implement-review\ndescription: Orchestrate implementation and review.\n---\n\nCall delegate_writing_task.\n",
        )
        .expect("skill file");

        let call = ToolCall {
            id: ToolCallId::from_u64(1),
            tool_name: "LoadSkill".to_string(),
            args_json: r#"{"name":"workflow-implement-review"}"#.to_string(),
        };
        let loaded = std::collections::BTreeSet::new();

        let result = load_skill_result(
            &prompt_root,
            &prompt_root,
            &[],
            &loaded,
            &call,
            PromptProfile::Subagent,
        );
        assert_eq!(result.status, agent_vocab::ToolResultStatus::Error);
        assert!(result
            .output
            .contains("workflow skills are not available to subagent sessions"));

        std::fs::remove_dir_all(prompt_root).ok();
    }

    #[test]
    fn resolves_skill_role_content_without_tool_xml() {
        let workspace_id = make_temp_dir("resolve-skill-role");
        let workspace = workspace_id.join("repo");
        let skill_dir = workspace.join(".agents/skills/reviewer");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: reviewer\ndescription: Review code.\n---\n\nReview carefully.\n",
        )
        .expect("skill file");

        let role = resolve_skill_role(
            &workspace_id,
            &workspace_id,
            &discover_raw(&workspace_id, &["repo"]),
            "repo/reviewer",
        )
        .expect("role resolves");
        assert_eq!(role.name, "reviewer");
        assert_eq!(role.workspace.as_deref(), Some("repo"));
        assert_eq!(role.description, "Review code.");
        assert_eq!(role.content, "Review carefully.");

        std::fs::remove_dir_all(workspace_id).ok();
    }

    #[test]
    fn rejects_unprefixed_workspace_role_even_when_unique() {
        let workspace_id = make_temp_dir("resolve-unambiguous-workspace-role");
        let workspace = workspace_id.join("repo");
        let skill_dir = workspace.join(".agents/skills/context-inspector");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: context-inspector\ndescription: Inspect context.\n---\n\nInspect carefully.\n",
        )
        .expect("skill file");

        let error = resolve_skill_role(
            &workspace_id,
            &workspace_id,
            &discover_raw(&workspace_id, &["repo"]),
            "context-inspector",
        )
        .expect_err("unprefixed workspace role is rejected");
        assert!(error
            .to_string()
            .contains("role skill not found: context-inspector"));

        std::fs::remove_dir_all(workspace_id).ok();
    }

    #[test]
    fn unprefixed_workspace_role_does_not_suggest_legacy_workspace_arg() {
        let workspace_id = make_temp_dir("resolve-ambiguous-workspace-role");
        for workspace in ["repo-a", "repo-b"] {
            let skill_dir = workspace_id
                .join(workspace)
                .join(".agents/skills/context-inspector");
            std::fs::create_dir_all(&skill_dir).expect("skill dir");
            std::fs::write(
                skill_dir.join("SKILL.md"),
                "---\nname: context-inspector\ndescription: Inspect context.\n---\n\nInspect carefully.\n",
            )
            .expect("skill file");
        }

        let error = resolve_skill_role(
            &workspace_id,
            &workspace_id,
            &discover_raw(&workspace_id, &["repo-a", "repo-b"]),
            "context-inspector",
        )
        .expect_err("unprefixed workspace role is rejected");
        assert!(error
            .to_string()
            .contains("role skill not found: context-inspector"));
        assert!(!error.to_string().contains("role_workspace"));

        std::fs::remove_dir_all(workspace_id).ok();
    }

    #[test]
    fn resolves_workspace_role_with_prefixed_name() {
        let workspace_id = make_temp_dir("resolve-prefixed-workspace-role");
        let workspace = workspace_id.join("repo");
        let skill_dir = workspace.join(".agents/skills/context-inspector");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: context-inspector\ndescription: Inspect context.\n---\n\nInspect carefully.\n",
        )
        .expect("skill file");

        let role = resolve_skill_role(
            &workspace_id,
            &workspace_id,
            &discover_raw(&workspace_id, &["repo"]),
            "repo/context-inspector",
        )
        .expect("role resolves");
        assert_eq!(role.name, "context-inspector");
        assert_eq!(role.workspace.as_deref(), Some("repo"));

        std::fs::remove_dir_all(workspace_id).ok();
    }

    #[test]
    fn resolves_packaged_worker_role_when_skill_is_absent() {
        let workspace_id = make_temp_dir("resolve-packaged-role");
        write_role(
            &workspace_id.join("subagent-roles/worker/SKILL.md"),
            "worker",
            "Perform delegated implementation, research, or artifact work.",
            "You are a delegated worker subagent.\n\nReport clearly.",
        );
        let role = resolve_skill_role(
            &workspace_id,
            &workspace_id,
            &discover_raw(&workspace_id, &["repo"]),
            "worker",
        )
        .expect("role resolves");
        assert_eq!(role.name, "worker");
        assert_eq!(role.workspace, None);
        assert_eq!(
            role.description,
            "Perform delegated implementation, research, or artifact work."
        );
        assert_eq!(
            role.content,
            "You are a delegated worker subagent.\n\nReport clearly."
        );
        assert_eq!(
            role.file_path,
            workspace_id.join("subagent-roles/worker/SKILL.md")
        );

        std::fs::remove_dir_all(workspace_id).ok();
    }

    #[test]
    fn prefixed_workspace_role_still_requires_a_skill_file() {
        let workspace_id = make_temp_dir("resolve-missing-workspace-role");
        write_role(
            &workspace_id.join("subagent-roles/worker/SKILL.md"),
            "worker",
            "Perform delegated implementation, research, or artifact work.",
            "You are a delegated worker subagent.",
        );
        let error = resolve_skill_role(
            &workspace_id,
            &workspace_id,
            &discover_raw(&workspace_id, &["repo"]),
            "repo/worker",
        )
        .expect_err("workspace-scoped role should not fall back to builtin");
        assert!(error
            .to_string()
            .contains("role skill not found: repo/worker"));

        std::fs::remove_dir_all(workspace_id).ok();
    }

    #[test]
    fn global_skill_omits_workspace() {
        let workspace_id = make_temp_dir("load-global-skill");
        let home = workspace_id.join("home");
        let skill_dir = home.join(".agents/skills/global");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: global\ndescription: Use globally.\n---\n\nGlobal content.\n",
        )
        .expect("skill file");

        let call = ToolCall {
            id: ToolCallId::from_u64(1),
            tool_name: "LoadSkill".to_string(),
            args_json: r#"{"name":"global"}"#.to_string(),
        };
        let loaded = std::collections::BTreeSet::new();

        let runtime_raw = discover_raw_with_home(&workspace_id, &[], &home);
        let result = load_skill_result(
            &workspace_id,
            &workspace_id,
            &runtime_raw,
            &loaded,
            &call,
            PromptProfile::Parent,
        );
        assert_eq!(result.status, agent_vocab::ToolResultStatus::Success);
        let output: Value = serde_json::from_str(&result.output).expect("json output");
        assert_eq!(output["name"], "global");
        assert_eq!(output["skill_name"], "global");
        assert!(output.get("workspace").is_none());
        assert!(!result.output.contains("<base_dir>"));

        std::fs::remove_dir_all(workspace_id).ok();
    }

    /// Reproduce what the runtime returns for a temp workspace tree: every
    /// `<workspace_id>/<dir>/.agents/skills/<name>/SKILL.md`.
    fn discover_raw(workspace_id: &Path, workspace_dirs: &[&str]) -> Vec<RawSkillFile> {
        let mut files = Vec::new();
        for dir in workspace_dirs {
            collect_raw(
                &workspace_id.join(dir).join(".agents/skills"),
                Some(dir),
                &mut files,
            );
        }
        files.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
        files
    }

    /// Like `discover_raw` but also includes the runtime host's global
    /// `<home>/.agents/skills` as `None`-workspace (home) skills.
    fn discover_raw_with_home(
        workspace_id: &Path,
        workspace_dirs: &[&str],
        home: &Path,
    ) -> Vec<RawSkillFile> {
        let mut files = Vec::new();
        collect_raw(&home.join(".agents/skills"), None, &mut files);
        for dir in workspace_dirs {
            collect_raw(
                &workspace_id.join(dir).join(".agents/skills"),
                Some(dir),
                &mut files,
            );
        }
        files.sort_by(|left, right| left.rel_path.cmp(&right.rel_path));
        files
    }

    fn collect_raw(skills_dir: &Path, workspace: Option<&str>, files: &mut Vec<RawSkillFile>) {
        let Ok(entries) = std::fs::read_dir(skills_dir) else {
            return;
        };
        for entry in entries.flatten() {
            if !entry.path().is_dir() {
                continue;
            }
            let Ok(contents) = std::fs::read_to_string(entry.path().join("SKILL.md")) else {
                continue;
            };
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let rel_path = match workspace {
                Some(workspace) => format!("{workspace}/.agents/skills/{name}/SKILL.md"),
                None => format!("~/.agents/skills/{name}/SKILL.md"),
            };
            files.push(RawSkillFile {
                workspace: workspace.map(str::to_string),
                rel_path,
                contents,
            });
        }
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

    fn write_role(path: &Path, name: &str, description: &str, body: &str) {
        let parent = path.parent().expect("role path parent");
        std::fs::create_dir_all(parent).expect("role dir");
        std::fs::write(
            path,
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}\n"),
        )
        .expect("role file");
    }
}
