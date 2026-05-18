use std::path::Path;

use agent_vocab::{ToolCall, ToolResultMessage};
use anyhow::{anyhow, Result};
use serde::Deserialize;

use super::prompt::load_skills_for_cwd;

pub(crate) fn load_skill_result(
    cwd: &Path,
    loaded_skills: &std::collections::BTreeSet<String>,
    call: &ToolCall,
) -> ToolResultMessage {
    match load_skill_output(cwd, loaded_skills, call) {
        Ok(output) => ToolResultMessage::success(call.id.clone(), "LoadSkill", output),
        Err(error) => ToolResultMessage::error(call.id.clone(), "LoadSkill", error.to_string()),
    }
}

fn load_skill_output(
    cwd: &Path,
    loaded_skills: &std::collections::BTreeSet<String>,
    call: &ToolCall,
) -> Result<String> {
    let args: LoadSkillArgs = serde_json::from_str(&call.args_json)?;
    let name = args.name.trim();
    if name.is_empty() {
        return Err(anyhow!("skill name cannot be empty"));
    }
    if loaded_skills.contains(name) {
        return Ok("skill already loaded".to_string());
    }
    let skills = load_skills_for_cwd(cwd);
    let Some(skill) = skills.into_iter().find(|skill| skill.name == name) else {
        return Err(anyhow!("skill not found: {name}"));
    };
    let content = std::fs::read_to_string(&skill.file_path)?;
    Ok(format!(
        "<loaded_skill>\n<name>{}</name>\n<content>\n{}\n</content>\n</loaded_skill>",
        xml_escape(&skill.name),
        content.trim()
    ))
}

#[derive(Debug, Deserialize)]
struct LoadSkillArgs {
    name: String,
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
        let cwd = make_temp_dir("load-skill");
        let skill_dir = cwd.join(".agents/skills/rust-refactor");
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
        let mut loaded = std::collections::BTreeSet::new();

        let first = load_skill_result(&cwd, &loaded, &call);
        assert_eq!(first.status, agent_vocab::ToolResultStatus::Success);
        assert!(first.output.contains("<name>rust-refactor</name>"));
        assert!(first.output.contains("Prefer small, tested changes."));

        loaded.insert("rust-refactor".to_string());
        let second = load_skill_result(&cwd, &loaded, &call);
        assert_eq!(second.status, agent_vocab::ToolResultStatus::Success);
        assert_eq!(second.output, "skill already loaded");

        std::fs::remove_dir_all(cwd).ok();
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
