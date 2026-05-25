use std::path::{Path, PathBuf};

use agent_prompt::{render_prompt, PromptContext, PromptWorkspace, Skill, ToolSpec};
use agent_store::{SessionConfig, SessionWorkspace};
use agent_vocab::ProviderKind;

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
        cwd: PathBuf::from(&config.outer_cwd),
        has_project: config.project_id.is_some(),
        workspaces: config
            .workspaces
            .iter()
            .map(|workspace| PromptWorkspace {
                workspace_dir: workspace.workspace_dir.clone(),
                remote_url: workspace.remote_url.clone(),
                remote_branch: workspace.remote_branch.clone(),
                base_sha: workspace.base_sha.clone(),
                local_branch: workspace.local_branch.clone(),
            })
            .collect(),
        tools: tool_specs(state, config.provider.kind),
        skills: load_prompt_skills(config),
    }
}

fn tool_specs(state: &AppState, provider: ProviderKind) -> Vec<ToolSpec> {
    state
        .tools
        .provider_tools_for_provider(provider)
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

fn load_prompt_skills(config: &SessionConfig) -> Vec<Skill> {
    load_skills_for_session_workspaces(&PathBuf::from(&config.outer_cwd), &config.workspaces)
}

#[cfg(test)]
#[allow(dead_code)]
pub(super) fn load_skills_for_workspace_roots(
    outer_cwd: &Path,
    workspace_dirs: &[String],
) -> Vec<Skill> {
    let workspaces = workspace_dirs
        .iter()
        .map(|workspace_dir| SessionWorkspace {
            workspace_dir: workspace_dir.clone(),
            remote_url: String::new(),
            remote_branch: String::new(),
            base_sha: String::new(),
            local_branch: String::new(),
        })
        .collect::<Vec<_>>();
    load_skills_for_session_workspaces_with_home(outer_cwd, &workspaces, home_dir().as_deref())
}

pub(super) fn load_skills_for_session_workspaces(
    outer_cwd: &Path,
    workspaces: &[SessionWorkspace],
) -> Vec<Skill> {
    load_skills_for_session_workspaces_with_home(outer_cwd, workspaces, home_dir().as_deref())
}

#[cfg(test)]
pub(super) fn load_skills_for_workspace_roots_with_home(
    outer_cwd: &Path,
    workspace_dirs: &[String],
    home: Option<&Path>,
) -> Vec<Skill> {
    let workspaces = workspace_dirs
        .iter()
        .map(|workspace_dir| SessionWorkspace {
            workspace_dir: workspace_dir.clone(),
            remote_url: String::new(),
            remote_branch: String::new(),
            base_sha: String::new(),
            local_branch: String::new(),
        })
        .collect::<Vec<_>>();
    load_skills_for_session_workspaces_with_home(outer_cwd, &workspaces, home)
}

pub(super) fn load_skills_for_session_workspaces_with_home(
    outer_cwd: &Path,
    workspaces: &[SessionWorkspace],
    home: Option<&Path>,
) -> Vec<Skill> {
    let outer_cwd = normalize_existing_dir(outer_cwd);
    let mut skills = Vec::new();

    if let Some(home) = home {
        let home_skills_dir = home.join(".agents/skills");
        add_skills_from_agents_dir(&home_skills_dir, None, &mut skills);
    }

    for workspace in workspaces {
        let workspace_root = outer_cwd.join(&workspace.workspace_dir);
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
    let raw = std::fs::read_to_string(path).ok()?;
    let (frontmatter, _body) = split_frontmatter(&raw);
    let frontmatter = parse_simple_frontmatter(frontmatter.unwrap_or_default());
    let name = frontmatter.get("name").cloned()?.trim().to_string();
    let description = frontmatter.get("description")?.trim().to_string();
    if name.is_empty() || description.is_empty() {
        return None;
    }
    let skill = match workspace {
        Some(workspace) => Skill::workspace(workspace.to_string(), name, description, path),
        None => Skill::global(name, description, path),
    };
    Some(skill)
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
