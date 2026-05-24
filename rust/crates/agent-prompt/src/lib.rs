#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use minijinja::Environment;
use serde_json::{json, Value};

const PI_MD: &str = include_str!("../../../../PI.md");
const PI_COMPACTION_MD: &str = include_str!("../../../../PI.compaction.md");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub canonical_name: String,
    pub prompt_alias: String,
}

impl ToolSpec {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        input_schema: Value,
        canonical_name: impl Into<String>,
        prompt_alias: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema,
            canonical_name: canonical_name.into(),
            prompt_alias: prompt_alias.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub workspace: Option<String>,
    pub name: String,
    pub description: String,
    pub file_path: PathBuf,
}

impl Skill {
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        file_path: impl Into<PathBuf>,
    ) -> Self {
        Self::global(name, description, file_path)
    }

    pub fn global(
        name: impl Into<String>,
        description: impl Into<String>,
        file_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            workspace: None,
            name: name.into(),
            description: description.into(),
            file_path: file_path.into(),
        }
    }

    pub fn workspace(
        workspace: impl Into<String>,
        name: impl Into<String>,
        description: impl Into<String>,
        file_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            workspace: Some(workspace.into()),
            name: name.into(),
            description: description.into(),
            file_path: file_path.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct PromptContext {
    pub cwd: PathBuf,
    pub has_project: bool,
    pub workspace_dirs: Vec<String>,
    pub tools: Vec<ToolSpec>,
    pub skills: Vec<Skill>,
}

pub fn pi_md() -> &'static str {
    PI_MD
}

pub fn render_prompt(ctx: &PromptContext) -> String {
    render(PI_MD, ctx)
}

pub fn render_compaction_prompt(ctx: &PromptContext) -> String {
    render(PI_COMPACTION_MD, ctx)
}

fn render(template: &str, ctx: &PromptContext) -> String {
    let mut env = Environment::new();
    env.add_template("prompt", template)
        .expect("PI prompt template must parse");
    compact_blank_lines(
        &env.get_template("prompt")
            .expect("PI prompt template must exist")
            .render(template_context(ctx))
            .expect("PI prompt template must render"),
    )
}

fn template_context(ctx: &PromptContext) -> Value {
    let agents_md = if ctx.has_project {
        find_upward(&ctx.cwd, "AGENTS.md")
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default()
    } else {
        String::new()
    };
    json!({
        "session": {
            "cwd": path_display(&ctx.cwd),
            "has_project": ctx.has_project,
            "workspace_dirs": ctx.workspace_dirs,
            "workspace_dirs_markdown": workspace_dirs_markdown(&ctx.workspace_dirs),
        },
        "project": {
            "agents_md": agents_md,
        },
        "tools": {
            "specs": tools_specs_markdown(&ctx.tools),
            "aliases": tools_aliases_json(&ctx.tools),
        },
        "skills": {
            "index": skills_index_xml(&ctx.skills),
        },
    })
}

fn tools_specs_markdown(tools: &[ToolSpec]) -> String {
    if tools.is_empty() {
        return "No tools are currently available.".to_string();
    }
    let mut tools = tools.iter().collect::<Vec<_>>();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    tools
        .into_iter()
        .map(|tool| {
            let schema = serde_json::to_string_pretty(&tool.input_schema)
                .unwrap_or_else(|_| "{}".to_string());
            format!(
                "### {}\n\n{}\n\nParameters:\n\n```json\n{}\n```",
                tool.name,
                tool.description.trim(),
                schema
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn workspace_dirs_markdown(workspace_dirs: &[String]) -> String {
    if workspace_dirs.is_empty() {
        return "No workspace directories are configured for this session.".to_string();
    }
    workspace_dirs
        .iter()
        .map(|dir| format!("- {dir}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn tools_aliases_json(tools: &[ToolSpec]) -> Value {
    let mut map = serde_json::Map::new();
    for tool in tools {
        map.insert(tool.prompt_alias.clone(), Value::String(tool.name.clone()));
    }
    Value::Object(map)
}

fn skills_index_xml(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut skills = skills.iter().collect::<Vec<_>>();
    skills.sort_by(|left, right| {
        left.workspace
            .cmp(&right.workspace)
            .then_with(|| left.name.cmp(&right.name))
    });
    let mut lines = vec!["<available_skills>".to_string()];
    for skill in skills {
        lines.push("  <skill>".to_string());
        if let Some(workspace) = &skill.workspace {
            lines.push(format!(
                "    <workspace>{}</workspace>",
                escape_xml(workspace)
            ));
        }
        lines.push(format!("    <name>{}</name>", escape_xml(&skill.name)));
        lines.push(format!(
            "    <description>{}</description>",
            escape_xml(&skill.description)
        ));
        lines.push("  </skill>".to_string());
    }
    lines.push("</available_skills>".to_string());
    lines.join("\n")
}

fn find_upward(start: &Path, filename: &str) -> Option<PathBuf> {
    let mut current = if start.is_dir() {
        start
    } else {
        start.parent()?
    };
    loop {
        let candidate = current.join(filename);
        if candidate.is_file() {
            return Some(candidate);
        }
        current = current.parent()?;
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn path_display(path: &Path) -> String {
    path.display().to_string().replace('\\', "/")
}

fn compact_blank_lines(input: &str) -> String {
    let mut output = String::new();
    let mut blank_count = 0;
    for line in input.lines() {
        if line.trim().is_empty() {
            blank_count += 1;
            if blank_count <= 2 {
                output.push('\n');
            }
        } else {
            blank_count = 0;
            output.push_str(line.trim_end());
            output.push('\n');
        }
    }
    output.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx(tools: Vec<&str>, skills: Vec<Skill>) -> PromptContext {
        PromptContext {
            cwd: PathBuf::from("/tmp/project"),
            has_project: true,
            workspace_dirs: Vec::new(),
            tools: tools
                .into_iter()
                .map(|name| {
                    ToolSpec::new(
                        name,
                        format!("{name} description"),
                        json!({"type":"object"}),
                        name,
                        name.to_ascii_lowercase(),
                    )
                })
                .collect(),
            skills,
        }
    }

    #[test]
    fn renders_repo_pi_as_static_prompt() {
        let rendered = render_prompt(&ctx(vec!["Bash", "Grep", "Edit"], Vec::new()));
        assert!(rendered.contains("You are a helpful assitant"));
        assert!(rendered.contains("### Bash"));
        assert!(rendered.contains("### Edit"));
        assert!(!rendered.contains("Current date"));
        assert!(!rendered.contains("Starting working directory"));
    }

    #[test]
    fn skills_are_available_to_pi_template() {
        let global_skill = Skill::new(
            "rust-refactor",
            "Use for Rust refactors.",
            "/tmp/project/.agents/skills/rust-refactor/SKILL.md",
        );
        let workspace_skill = Skill::workspace(
            "repo",
            "rust-refactor",
            "Use for repo Rust refactors.",
            "/tmp/project/repo/.agents/skills/rust-refactor/SKILL.md",
        );
        let rendered = render_prompt(&ctx(vec!["Bash"], vec![global_skill, workspace_skill]));
        assert!(rendered.contains("<available_skills>"));
        assert!(rendered.contains("rust-refactor"));
        assert!(rendered.contains("<workspace>repo</workspace>"));
        assert!(!rendered.contains("<base_dir>"));
        assert!(!rendered.contains("<location>"));
    }

    #[test]
    fn custom_template_data_can_choose_to_include_cwd() {
        let rendered = render(
            "cwd={{ session.cwd }}\nworkspaces={{ session.workspace_dirs_markdown }}\n\n{{ tools.specs }}",
            &ctx(vec!["Bash"], Vec::new()),
        );
        assert!(rendered.contains("cwd=/tmp/project"));
        assert!(rendered.contains("No workspace directories"));
        assert!(rendered.contains("Parameters:"));
    }

    #[test]
    fn pi_template_gates_ephemeral_workspace_copy() {
        let mut ctx = ctx(vec!["Bash"], Vec::new());
        ctx.has_project = false;
        ctx.cwd = PathBuf::from("/home/tester");
        let rendered = render_prompt(&ctx);
        assert!(rendered.contains("Current working directory: /home/tester"));
        assert!(!rendered.contains("Workspace subdirectories"));
    }

    #[test]
    fn renders_compaction_prompt() {
        let rendered = render_compaction_prompt(&ctx(vec!["Bash"], Vec::new()));
        assert!(rendered.starts_with("Produce a concise continuation summary"));
        assert!(!rendered.contains("You are an expert coding assistant"));
    }
}
