use std::path::{Path, PathBuf};

use agent_prompt::{render_prompt, PromptContext, Skill, ToolSpec};
use agent_store::SessionConfig;
use agent_vocab::{ProviderKind, ReplayDisplayKind};

use crate::state::AppState;

pub(super) async fn assemble_agent_prompt(
    state: &AppState,
    config: &SessionConfig,
) -> anyhow::Result<agent_provider::PromptSections> {
    let ctx = prompt_context(state, config);
    Ok(agent_provider::PromptSections::stable(render_prompt(&ctx)))
}

pub(crate) fn rendered_pi_prompt(state: &AppState, config: &SessionConfig) -> String {
    render_prompt(&prompt_context(state, config))
}

pub(super) fn prompt_context(state: &AppState, config: &SessionConfig) -> PromptContext {
    PromptContext {
        cwd: PathBuf::from(&config.starting_cwd),
        tools: tool_specs(state, config.provider.kind),
        skills: load_prompt_skills(config),
    }
}

fn tool_specs(state: &AppState, provider: ProviderKind) -> Vec<ToolSpec> {
    state
        .tools
        .listings_for_provider(provider)
        .into_iter()
        .map(|listing| {
            ToolSpec::new(
                listing.name,
                listing.description,
                listing.input_schema,
                listing.kind == ReplayDisplayKind::HostedTool,
            )
        })
        .collect()
}

fn load_prompt_skills(config: &SessionConfig) -> Vec<Skill> {
    load_skills_for_cwd(&PathBuf::from(&config.starting_cwd))
}

pub(super) fn load_skills_for_cwd(cwd: &Path) -> Vec<Skill> {
    let mut skills = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    collect_skills_from_dir(&cwd.join(".agents/skills"), &mut skills, &mut seen);
    skills
}

fn collect_skills_from_dir(
    dir: &Path,
    skills: &mut Vec<Skill>,
    seen: &mut std::collections::BTreeSet<String>,
) {
    if !dir.exists() {
        return;
    }
    let skill_file = dir.join("SKILL.md");
    if skill_file.is_file() {
        if let Some(skill) = load_skill_file(&skill_file) {
            add_skill(skill, skills, seen);
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if name.to_string_lossy().starts_with('.') || name == "node_modules" {
            continue;
        }
        if path.is_dir() {
            collect_skills_from_dir(&path, skills, seen);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("md") {
            if let Some(skill) = load_skill_file(&path) {
                add_skill(skill, skills, seen);
            }
        }
    }
}

fn add_skill(skill: Skill, skills: &mut Vec<Skill>, seen: &mut std::collections::BTreeSet<String>) {
    if seen.insert(skill.name.clone()) {
        skills.push(skill);
    }
}

fn load_skill_file(path: &Path) -> Option<Skill> {
    let raw = std::fs::read_to_string(path).ok()?;
    let (frontmatter, _body) = split_frontmatter(&raw);
    let frontmatter = parse_simple_frontmatter(frontmatter.unwrap_or_default());
    let fallback_name = path.parent()?.file_name()?.to_string_lossy().to_string();
    let name = frontmatter
        .get("name")
        .cloned()
        .unwrap_or(fallback_name)
        .trim()
        .to_string();
    let description = frontmatter.get("description")?.trim().to_string();
    if name.is_empty() || description.is_empty() {
        return None;
    }
    Some(Skill::new(name, description, path.to_path_buf()))
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
