use std::path::Path;

use agent_store::SessionWorkspace;
use agent_vocab::{ToolCall, ToolResultMessage};
use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::prompt::{
    load_skills_for_session_workspaces, load_skills_for_session_workspaces_with_home,
};

pub(crate) fn load_skill_result(
    outer_cwd: &Path,
    workspaces: &[SessionWorkspace],
    loaded_skills: &std::collections::BTreeSet<String>,
    call: &ToolCall,
) -> ToolResultMessage {
    match load_skill_output(outer_cwd, workspaces, loaded_skills, call) {
        Ok(output) => ToolResultMessage::success(call.id.clone(), "LoadSkill", output),
        Err(error) => ToolResultMessage::error(call.id.clone(), "LoadSkill", error.to_string()),
    }
}

fn load_skill_output(
    outer_cwd: &Path,
    workspaces: &[SessionWorkspace],
    loaded_skills: &std::collections::BTreeSet<String>,
    call: &ToolCall,
) -> Result<String> {
    load_skill_output_with_home(outer_cwd, workspaces, loaded_skills, call, None)
}

fn load_skill_output_with_home(
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
    let skills = match home {
        Some(home) => {
            load_skills_for_session_workspaces_with_home(outer_cwd, workspaces, Some(home))
        }
        None => load_skills_for_session_workspaces(outer_cwd, workspaces),
    };
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

        let first = load_skill_result(&outer_cwd, &workspaces, &loaded, &call);
        assert_eq!(first.status, agent_vocab::ToolResultStatus::Success);
        assert!(first.output.contains("<name>rust-refactor</name>"));
        assert!(first.output.contains("<workspace>repo</workspace>"));
        assert!(!first.output.contains("<base_dir>"));
        assert!(first.output.contains("Prefer small, tested changes."));

        loaded.insert(skill_identifier(Some("repo"), "rust-refactor"));
        let second = load_skill_result(&outer_cwd, &workspaces, &loaded, &call);
        assert_eq!(second.status, agent_vocab::ToolResultStatus::Success);
        assert_eq!(second.output, "skill already loaded");

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

        let result = load_skill_output_with_home(&outer_cwd, &[], &loaded, &call, Some(&home))
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
}
