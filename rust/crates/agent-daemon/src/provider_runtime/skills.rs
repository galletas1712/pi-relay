use std::path::{Path, PathBuf};

use agent_prompt::Skill;
use agent_store::SessionWorkspace;
use agent_vocab::{ToolCall, ToolResultMessage};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;

use super::prompt::{
    load_global_skills_from_dir, load_parsed_skill_file, load_skills_for_session_workspaces,
    load_skills_for_session_workspaces_with_home,
};

pub(crate) fn load_skill_result(
    prompt_root: &Path,
    outer_cwd: &Path,
    workspaces: &[SessionWorkspace],
    loaded_skills: &std::collections::BTreeSet<String>,
    call: &ToolCall,
) -> ToolResultMessage {
    match load_skill_output(prompt_root, outer_cwd, workspaces, loaded_skills, call) {
        Ok(output) => ToolResultMessage::success(call.id.clone(), "LoadSkill", output),
        Err(error) => ToolResultMessage::error(call.id.clone(), "LoadSkill", error.to_string()),
    }
}

fn load_skill_output(
    prompt_root: &Path,
    outer_cwd: &Path,
    workspaces: &[SessionWorkspace],
    loaded_skills: &std::collections::BTreeSet<String>,
    call: &ToolCall,
) -> Result<String> {
    load_skill_output_with_home(
        prompt_root,
        outer_cwd,
        workspaces,
        loaded_skills,
        call,
        None,
    )
}

fn load_skill_output_with_home(
    prompt_root: &Path,
    outer_cwd: &Path,
    workspaces: &[SessionWorkspace],
    loaded_skills: &std::collections::BTreeSet<String>,
    call: &ToolCall,
    home: Option<&Path>,
) -> Result<String> {
    let args: LoadSkillArgs = serde_json::from_str(&call.args_json)?;
    let name = args.name.trim();
    if name.is_empty() {
        return Err(anyhow!("skill name cannot be empty"));
    }
    let workspace = args
        .workspace
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let skill_id = skill_identifier(workspace, name);
    if loaded_skills.contains(&skill_id) {
        return Ok("skill already loaded".to_string());
    }
    let mut skills = match home {
        Some(home) => {
            load_skills_for_session_workspaces_with_home(outer_cwd, workspaces, Some(home))
        }
        None => load_skills_for_session_workspaces(outer_cwd, workspaces),
    };
    if workspace.is_none() {
        skills.extend(load_global_skills_from_dir(&prompt_root.join("workflows")));
    }
    let Some(skill) = skills
        .into_iter()
        .find(|skill| skill.name == name && skill.workspace.as_deref() == workspace)
    else {
        return Err(match workspace {
            Some(workspace) => anyhow!("skill not found: {workspace}/{name}"),
            None => anyhow!("skill not found: {name}"),
        });
    };
    let content = std::fs::read_to_string(&skill.file_path)?;
    let workspace_xml = skill
        .workspace
        .as_deref()
        .map(|workspace| format!("\n<workspace>{}</workspace>", xml_escape(workspace)))
        .unwrap_or_default();
    Ok(format!(
        "<loaded_skill>\n<name>{}</name>{}\n<content>\n{}\n</content>\n</loaded_skill>",
        xml_escape(&skill.name),
        workspace_xml,
        content.trim()
    ))
}

#[derive(Debug, Deserialize)]
struct LoadSkillArgs {
    name: String,
    workspace: Option<String>,
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
    outer_cwd: &Path,
    workspaces: &[SessionWorkspace],
    name: &str,
    workspace: Option<&str>,
) -> Result<ResolvedSkillRole> {
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("role name cannot be empty"));
    }
    let workspace = workspace.map(str::trim).filter(|value| !value.is_empty());
    let skills = load_skills_for_session_workspaces(outer_cwd, workspaces);
    if let Some(workspace) = workspace {
        let Some(skill) = skills
            .into_iter()
            .find(|skill| skill.name == name && skill.workspace.as_deref() == Some(workspace))
        else {
            return Err(anyhow!("role skill not found: {workspace}/{name}"));
        };
        return role_from_skill(skill);
    }

    let mut matches = skills
        .into_iter()
        .filter(|skill| skill.name == name)
        .collect::<Vec<_>>();
    match matches.len() {
        1 => role_from_skill(matches.remove(0)),
        0 => packaged_role(prompt_root, name)
            .ok_or_else(|| anyhow!("role skill not found: {name}"))
            .and_then(role_from_skill),
        _ => {
            let mut scopes = matches
                .iter()
                .map(|skill| skill.workspace.as_deref().unwrap_or("global"))
                .collect::<Vec<_>>();
            scopes.sort_unstable();
            Err(anyhow!(
                "role skill is ambiguous: {name} exists in {}; pass role_workspace",
                scopes.join(", ")
            ))
        }
    }
}

fn load_packaged_role_skills(prompt_root: &Path) -> Vec<Skill> {
    load_global_skills_from_dir(&prompt_root.join("subagent-roles"))
}

fn packaged_role(prompt_root: &Path, name: &str) -> Option<Skill> {
    load_packaged_role_skills(prompt_root)
        .into_iter()
        .find(|skill| skill.name == name)
}

fn role_from_skill(skill: Skill) -> Result<ResolvedSkillRole> {
    let parsed = load_parsed_skill_file(&skill.file_path)
        .with_context(|| format!("read role skill {}", skill.file_path.display()))?;
    Ok(ResolvedSkillRole {
        name: skill.name,
        workspace: skill.workspace,
        description: skill.description,
        file_path: skill.file_path,
        content: parsed.body,
    })
}

fn xml_escape(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_vocab::{ToolCall, ToolCallId};
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn load_skill_result_loads_content_once() {
        let outer_cwd = make_temp_dir("load-skill");
        let workspace = outer_cwd.join("repo");
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
        let mut loaded = std::collections::BTreeSet::new();
        let workspaces = vec![SessionWorkspace::local("repo", "")];

        let first = load_skill_result(&outer_cwd, &outer_cwd, &workspaces, &loaded, &call);
        assert_eq!(first.status, agent_vocab::ToolResultStatus::Success);
        assert!(first.output.contains("<name>rust-refactor</name>"));
        assert!(first.output.contains("<workspace>repo</workspace>"));
        assert!(!first.output.contains("<base_dir>"));
        assert!(first.output.contains("Prefer small, tested changes."));

        loaded.insert(skill_identifier(Some("repo"), "rust-refactor"));
        let second = load_skill_result(&outer_cwd, &outer_cwd, &workspaces, &loaded, &call);
        assert_eq!(second.status, agent_vocab::ToolResultStatus::Success);
        assert_eq!(second.output, "skill already loaded");

        std::fs::remove_dir_all(outer_cwd).ok();
    }

    #[test]
    fn workflow_skills_load_but_do_not_resolve_as_packaged_roles() {
        let prompt_root = make_temp_dir("load-workflow-skill");
        let outer_cwd = prompt_root.join("outer");
        std::fs::create_dir_all(&outer_cwd).expect("outer cwd");
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

        let result = load_skill_result(&prompt_root, &outer_cwd, &[], &loaded, &call);
        assert_eq!(result.status, agent_vocab::ToolResultStatus::Success);
        assert!(result
            .output
            .contains("<name>workflow-only-test-role</name>"));
        assert!(!result.output.contains("<workspace>"));
        assert!(result.output.contains("delegate_readonly_tasks"));

        let error = resolve_skill_role(
            &prompt_root,
            &outer_cwd,
            &[],
            "workflow-only-test-role",
            None,
        )
        .expect_err("workflow skills are not packaged subagent roles");
        assert!(error
            .to_string()
            .contains("role skill not found: workflow-only-test-role"));

        std::fs::remove_dir_all(prompt_root).ok();
    }

    #[test]
    fn resolves_skill_role_content_without_tool_xml() {
        let outer_cwd = make_temp_dir("resolve-skill-role");
        let workspace = outer_cwd.join("repo");
        let skill_dir = workspace.join(".agents/skills/reviewer");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: reviewer\ndescription: Review code.\n---\n\nReview carefully.\n",
        )
        .expect("skill file");

        let role = resolve_skill_role(
            &outer_cwd,
            &outer_cwd,
            &[SessionWorkspace::local("repo", "")],
            "reviewer",
            Some("repo"),
        )
        .expect("role resolves");
        assert_eq!(role.name, "reviewer");
        assert_eq!(role.workspace.as_deref(), Some("repo"));
        assert_eq!(role.description, "Review code.");
        assert_eq!(role.content, "Review carefully.");

        std::fs::remove_dir_all(outer_cwd).ok();
    }

    #[test]
    fn resolves_unambiguous_workspace_role_without_workspace_arg() {
        let outer_cwd = make_temp_dir("resolve-unambiguous-workspace-role");
        let workspace = outer_cwd.join("repo");
        let skill_dir = workspace.join(".agents/skills/context-inspector");
        std::fs::create_dir_all(&skill_dir).expect("skill dir");
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: context-inspector\ndescription: Inspect context.\n---\n\nInspect carefully.\n",
        )
        .expect("skill file");

        let role = resolve_skill_role(
            &outer_cwd,
            &outer_cwd,
            &[SessionWorkspace::local("repo", "")],
            "context-inspector",
            None,
        )
        .expect("role resolves");
        assert_eq!(role.name, "context-inspector");
        assert_eq!(role.workspace.as_deref(), Some("repo"));

        std::fs::remove_dir_all(outer_cwd).ok();
    }

    #[test]
    fn ambiguous_workspace_role_requires_workspace_arg() {
        let outer_cwd = make_temp_dir("resolve-ambiguous-workspace-role");
        for workspace in ["repo-a", "repo-b"] {
            let skill_dir = outer_cwd
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
            &outer_cwd,
            &outer_cwd,
            &[
                SessionWorkspace::local("repo-a", ""),
                SessionWorkspace::local("repo-b", ""),
            ],
            "context-inspector",
            None,
        )
        .expect_err("ambiguous role rejected");
        assert!(error.to_string().contains("pass role_workspace"));

        std::fs::remove_dir_all(outer_cwd).ok();
    }

    #[test]
    fn resolves_packaged_worker_role_when_skill_is_absent() {
        let outer_cwd = make_temp_dir("resolve-packaged-role");
        write_role(
            &outer_cwd.join("subagent-roles/worker/SKILL.md"),
            "worker",
            "Perform delegated implementation, research, or artifact work.",
            "You are a delegated worker subagent.\n\nReport clearly.",
        );
        let role = resolve_skill_role(
            &outer_cwd,
            &outer_cwd,
            &[SessionWorkspace::local("repo", "")],
            "worker",
            None,
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
            outer_cwd.join("subagent-roles/worker/SKILL.md")
        );

        std::fs::remove_dir_all(outer_cwd).ok();
    }

    #[test]
    fn workspace_role_still_requires_a_skill_file() {
        let outer_cwd = make_temp_dir("resolve-missing-workspace-role");
        let error = resolve_skill_role(
            &outer_cwd,
            &outer_cwd,
            &[SessionWorkspace::local("repo", "")],
            "worker",
            Some("repo"),
        )
        .expect_err("workspace-scoped role should not fall back to builtin");
        assert!(error
            .to_string()
            .contains("role skill not found: repo/worker"));

        std::fs::remove_dir_all(outer_cwd).ok();
    }

    #[test]
    fn global_skill_omits_workspace() {
        let outer_cwd = make_temp_dir("load-global-skill");
        let home = outer_cwd.join("home");
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

        let result =
            load_skill_output_with_home(&outer_cwd, &outer_cwd, &[], &loaded, &call, Some(&home))
                .expect("loads global skill");
        let result = ToolResultMessage::success(call.id.clone(), "LoadSkill", result);
        assert_eq!(result.status, agent_vocab::ToolResultStatus::Success);
        assert!(result.output.contains("<name>global</name>"));
        assert!(!result.output.contains("<workspace>"));
        assert!(!result.output.contains("<base_dir>"));

        std::fs::remove_dir_all(outer_cwd).ok();
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
