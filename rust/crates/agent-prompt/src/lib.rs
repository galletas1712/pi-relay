#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use minijinja::Environment;
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub canonical_name: String,
    pub prompt_alias: String,
}

fn mcp_servers_markdown(servers: &[PromptMcpServer]) -> String {
    servers
        .iter()
        .map(|server| {
            let tools = server
                .tools
                .iter()
                .map(|tool| format!("`{tool}`"))
                .collect::<Vec<_>>()
                .join(", ");
            format!("- {}: {tools}", server.server)
        })
        .collect::<Vec<_>>()
        .join("\n")
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

    pub fn exposed_name(&self) -> String {
        match self.workspace.as_deref() {
            Some(workspace) => format!("{workspace}/{}", self.name),
            None => self.name.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptProfile {
    Parent,
    Subagent,
}

impl PromptProfile {
    fn as_str(self) -> &'static str {
        match self {
            Self::Parent => "parent",
            Self::Subagent => "subagent",
        }
    }

    fn can_delegate(self) -> bool {
        matches!(self, Self::Parent)
    }

    fn can_load_workflows(self) -> bool {
        matches!(self, Self::Parent)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubagentRole {
    pub name: String,
    pub description: String,
}

impl SubagentRole {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptWorkspaceKind {
    Git,
    Local,
}

#[derive(Debug, Clone)]
pub struct PromptWorkspace {
    pub kind: PromptWorkspaceKind,
    pub workspace_dir: String,
    pub remote_url: Option<String>,
    pub remote_branch: Option<String>,
    pub source_path: Option<String>,
    pub base_sha: Option<String>,
    pub local_branch: Option<String>,
}

#[derive(Debug, Clone)]
pub struct PromptContext {
    pub profile: PromptProfile,
    pub cwd: PathBuf,
    pub has_project: bool,
    pub workspaces: Vec<PromptWorkspace>,
    pub tools: Vec<ToolSpec>,
    pub skills: Vec<Skill>,
    pub subagent_roles: Vec<SubagentRole>,
    pub mcp_servers: Vec<PromptMcpServer>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptMcpServer {
    pub server: String,
    pub tools: Vec<String>,
}

pub fn load_pi_md(repo_root: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(repo_root.join("PI.md"))
}

pub fn load_pi_compaction_md(repo_root: &Path) -> std::io::Result<String> {
    std::fs::read_to_string(repo_root.join("PI.compaction.md"))
}

pub fn render_prompt(template: &str, ctx: &PromptContext) -> String {
    render(template, ctx)
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
        agents_md_for_workspaces(&ctx.cwd, &ctx.workspaces)
    } else {
        String::new()
    };
    json!({
        "profile": {
            "name": ctx.profile.as_str(),
        },
        "capabilities": {
            "can_delegate": ctx.profile.can_delegate(),
            "can_load_workflows": ctx.profile.can_load_workflows(),
        },
        "session": {
            "cwd": path_display(&ctx.cwd),
            "has_project": ctx.has_project,
            "workspaces": workspaces_json(&ctx.workspaces),
            "workspaces_markdown": workspaces_markdown(&ctx.workspaces),
        },
        "project": {
            "agents_md": agents_md,
        },
        "tools": {
            "specs": tools_specs_markdown(&ctx.tools),
            "aliases": tools_aliases_json(&ctx.tools),
        },
        "skills": {
            "index": skills_index_json(&ctx.skills),
        },
        "subagent_roles": {
            "catalog": subagent_role_catalog_json(&ctx.subagent_roles),
        },
        "mcp": {
            "servers_markdown": mcp_servers_markdown(&ctx.mcp_servers),
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

fn workspaces_json(workspaces: &[PromptWorkspace]) -> Value {
    Value::Array(
        workspaces
            .iter()
            .map(|workspace| {
                json!({
                    "kind": workspace_kind(workspace.kind),
                    "workspace_dir": workspace.workspace_dir,
                    "remote_url": workspace.remote_url,
                    "remote_branch": workspace.remote_branch,
                    "source_path": workspace.source_path,
                    "base_sha": workspace.base_sha,
                    "local_branch": workspace.local_branch,
                })
            })
            .collect(),
    )
}

fn workspaces_markdown(workspaces: &[PromptWorkspace]) -> String {
    if workspaces.is_empty() {
        return "No project workspaces are configured for this session.".to_string();
    }
    workspaces
        .iter()
        .map(|workspace| match workspace.kind {
            PromptWorkspaceKind::Git => format!(
                "- {dir}\n  - type: Git\n  - remote: {remote}\n  - starting branch: {branch}",
                dir = workspace.workspace_dir,
                remote = workspace.remote_url.as_deref().unwrap_or(""),
                branch = workspace.remote_branch.as_deref().unwrap_or(""),
            ),
            PromptWorkspaceKind::Local => {
                format!(
                    "- {dir}\n  - type: local folder copy",
                    dir = workspace.workspace_dir
                )
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn workspace_kind(kind: PromptWorkspaceKind) -> &'static str {
    match kind {
        PromptWorkspaceKind::Git => "git",
        PromptWorkspaceKind::Local => "local",
    }
}

fn tools_aliases_json(tools: &[ToolSpec]) -> Value {
    let mut map = serde_json::Map::new();
    for tool in tools {
        map.insert(tool.prompt_alias.clone(), Value::String(tool.name.clone()));
    }
    Value::Object(map)
}

fn skills_index_json(skills: &[Skill]) -> String {
    if skills.is_empty() {
        return String::new();
    }
    let mut skills = skills.iter().collect::<Vec<_>>();
    skills.sort_by(|left, right| {
        left.exposed_name()
            .cmp(&right.exposed_name())
            .then_with(|| left.name.cmp(&right.name))
    });
    let skills = skills
        .into_iter()
        .map(|skill| {
            json!({
                "name": skill.exposed_name(),
                "description": skill.description,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&json!({
        "available_skills": skills,
    }))
    .expect("available skills JSON must serialize")
}

fn subagent_role_catalog_json(roles: &[SubagentRole]) -> String {
    if roles.is_empty() {
        return String::new();
    }
    let mut roles = roles.iter().collect::<Vec<_>>();
    roles.sort_by(|left, right| left.name.cmp(&right.name));
    let roles = roles
        .into_iter()
        .map(|role| {
            json!({
                "name": role.name,
                "description": role.description,
            })
        })
        .collect::<Vec<_>>();
    serde_json::to_string_pretty(&json!({
        "packaged_subagent_roles": roles,
    }))
    .expect("subagent role catalog JSON must serialize")
}

fn agents_md_for_workspaces(cwd: &Path, workspaces: &[PromptWorkspace]) -> String {
    workspaces
        .iter()
        .filter_map(|workspace| {
            let path = cwd.join(&workspace.workspace_dir).join("AGENTS.md");
            let content = std::fs::read_to_string(path).ok()?;
            if content.trim().is_empty() {
                return None;
            }
            Some(format!(
                "### {}\n\n{}",
                workspace.workspace_dir,
                content.trim()
            ))
        })
        .collect::<Vec<_>>()
        .join("\n\n")
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

    const TEST_PI_MD: &str = include_str!("../../../../PI.md");
    const TEST_PI_COMPACTION_MD: &str = include_str!("../../../../PI.compaction.md");

    fn ctx(profile: PromptProfile, tools: Vec<&str>, skills: Vec<Skill>) -> PromptContext {
        PromptContext {
            profile,
            cwd: PathBuf::from("/tmp/project"),
            has_project: true,
            workspaces: vec![PromptWorkspace {
                kind: PromptWorkspaceKind::Git,
                workspace_dir: "repo".to_string(),
                remote_url: Some("https://example.com/repo.git".to_string()),
                remote_branch: Some("main".to_string()),
                source_path: None,
                base_sha: Some("abc123".to_string()),
                local_branch: Some("pi/session/test/repo".to_string()),
            }],
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
            subagent_roles: vec![SubagentRole::new("reviewer", "Review artifacts.")],
            mcp_servers: Vec::new(),
        }
    }

    #[test]
    fn subagent_profile_omits_parent_orchestration_sections() {
        let rendered = render_prompt(
            TEST_PI_MD,
            &ctx(PromptProfile::Subagent, vec!["Bash", "Edit"], Vec::new()),
        );

        assert!(rendered.contains("### Bash"));
        assert!(!rendered.contains("## Subagent delegation"));
        assert!(!rendered.contains("Packaged subagent roles"));
        assert!(!rendered.contains("delegate_readonly_tasks"));
        assert!(!rendered.contains("delegate_writing_task"));
    }

    #[test]
    fn prompt_mcp_section_is_conditional_and_contains_names_only() {
        let empty = render_prompt(
            TEST_PI_MD,
            &ctx(PromptProfile::Parent, vec!["Bash"], Vec::new()),
        );
        assert!(!empty.contains("### MCP"));

        let mut selected = ctx(PromptProfile::Parent, vec!["Bash"], Vec::new());
        selected.mcp_servers = vec![PromptMcpServer {
            server: "workspace".to_string(),
            tools: vec![
                "mcp__workspace__read".to_string(),
                "mcp__workspace__search".to_string(),
            ],
        }];
        let rendered = render_prompt(TEST_PI_MD, &selected);
        assert!(rendered.contains("### MCP"));
        assert!(rendered.contains("- workspace: `mcp__workspace__read`, `mcp__workspace__search`"));
        let mcp_section = rendered
            .split_once("### MCP")
            .expect("selected MCP heading")
            .1
            .split_once("## Subagent delegation")
            .expect("following prompt section")
            .0;
        for forbidden in [
            "input_schema",
            "catalog_fingerprint",
            "healthy",
            "connection epoch",
        ] {
            assert!(!mcp_section.contains(forbidden));
        }
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
        let rendered = render_prompt(
            TEST_PI_MD,
            &ctx(
                PromptProfile::Parent,
                vec!["Bash"],
                vec![global_skill, workspace_skill],
            ),
        );
        assert!(rendered.contains("\"available_skills\""));
        assert!(rendered.contains("rust-refactor"));
        assert!(rendered.contains("repo/rust-refactor"));
        assert!(!rendered.contains("<workspace>repo</workspace>"));
        assert!(!rendered.contains("<base_dir>"));
        assert!(!rendered.contains("<location>"));
    }

    #[test]
    fn skills_index_uses_json_and_serde_escaping() {
        let skill = Skill::new(
            "quote-skill",
            "Use for <xml> & \"quotes\".",
            "/tmp/project/.agents/skills/quote-skill/SKILL.md",
        );
        let rendered = render_prompt(
            TEST_PI_MD,
            &ctx(PromptProfile::Parent, vec!["Bash"], vec![skill]),
        );
        assert!(rendered.contains("\"name\": \"quote-skill\""));
        assert!(rendered.contains("\"description\": \"Use for <xml> & \\\"quotes\\\".\""));
        assert!(!rendered.contains("&lt;xml&gt;"));
    }

    #[test]
    fn custom_template_data_can_choose_to_include_cwd() {
        let rendered = render(
            "cwd={{ session.cwd }}\nworkspaces={{ session.workspaces_markdown }}\n\n{{ tools.specs }}",
            &ctx(PromptProfile::Parent, vec!["Bash"], Vec::new()),
        );
        assert!(rendered.contains("cwd=/tmp/project"));
        assert!(rendered.contains("starting branch: main"));
        assert!(rendered.contains("Parameters:"));
    }

    #[test]
    fn pi_template_gates_ephemeral_workspace_copy() {
        let mut ctx = ctx(PromptProfile::Parent, vec!["Bash"], Vec::new());
        ctx.has_project = false;
        ctx.workspaces = Vec::new();
        ctx.cwd = PathBuf::from("/home/tester");
        let rendered = render_prompt(TEST_PI_MD, &ctx);
        assert!(rendered.contains("Current working directory: /home/tester"));
        assert!(!rendered.contains("Workspace subdirectories"));
    }

    #[test]
    fn renders_compaction_prompt() {
        let rendered = render_prompt(
            TEST_PI_COMPACTION_MD,
            &ctx(PromptProfile::Parent, vec!["Bash"], Vec::new()),
        );
        assert!(rendered.starts_with("Produce a concise continuation summary"));
        assert!(!rendered.contains("You are an expert coding assistant"));
    }
}
