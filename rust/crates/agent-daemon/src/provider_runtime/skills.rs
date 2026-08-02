use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_prompt::Skill;
use agent_runtime_protocol::{RawSkillFile, SkillKind, SkillOrigin};
use agent_store::PostgresAgentStore;
use agent_vocab::{ProviderConfig, ProviderKind, ReasoningEffort, ToolCall, ToolResultMessage};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::prompt::{parse_runtime_skills, parse_skill_contents, ParsedSkillFile};

pub(crate) fn load_skill_result(
    runtime_raw: &[RawSkillFile],
    call: &ToolCall,
) -> ToolResultMessage {
    match load_skill_output(runtime_raw, call) {
        Ok(output) => ToolResultMessage::success(call.id.clone(), "LoadSkill", output),
        Err(error) => ToolResultMessage::error(call.id.clone(), "LoadSkill", error.to_string()),
    }
}

fn load_skill_output(runtime_raw: &[RawSkillFile], call: &ToolCall) -> Result<String> {
    let args: LoadSkillArgs = serde_json::from_str(&call.args_json)?;
    let name = args.name.trim();
    if name.is_empty() {
        return Err(anyhow!("skill name cannot be empty"));
    }
    let skills = parse_runtime_skills(runtime_raw);
    let skill = resolve_load_skill(&skills, name)?;
    let file = runtime_raw
        .iter()
        .find(|file| Path::new(&file.path) == skill.file_path)
        .ok_or_else(|| anyhow!("resolved skill is missing from the runtime catalog: {name}"))?;
    Ok(serde_json::to_string(&LoadSkillOutput {
        path: &file.path,
        content: &file.contents,
    })?)
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LoadSkillArgs {
    name: String,
}

#[derive(Debug, Serialize)]
struct LoadSkillOutput<'a> {
    path: &'a str,
    content: &'a str,
}

#[derive(Debug, Clone)]
pub(crate) struct ResolvedPreloadedSkill {
    pub(crate) name: String,
    pub(crate) file_path: PathBuf,
    pub(crate) content: String,
}

#[derive(Debug)]
pub(crate) struct ResolvedSkillRole {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) file_path: PathBuf,
    pub(crate) content: String,
    pub(crate) provider: Option<ProviderConfig>,
    pub(crate) skills: Vec<ResolvedPreloadedSkill>,
}

pub(crate) fn resolve_skill_role(
    runtime_raw: &[RawSkillFile],
    name: &str,
) -> Result<ResolvedSkillRole> {
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("role name cannot be empty"));
    }
    let matches = runtime_raw
        .iter()
        .filter(|file| file.kind == SkillKind::SubagentRole && file.package_name == name)
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [file] => resolve_role_file(runtime_raw, file),
        [] => Err(anyhow!("role skill not found: {name}")),
        _ => Err(anyhow!("role skill is ambiguous: {name}")),
    }
}

pub(crate) fn resolved_role_catalog(runtime_raw: &[RawSkillFile]) -> Vec<ResolvedSkillRole> {
    runtime_raw
        .iter()
        .filter(|file| file.kind == SkillKind::SubagentRole)
        .filter_map(|file| resolve_role_file(runtime_raw, file).ok())
        .collect()
}

fn resolve_role_file(
    runtime_raw: &[RawSkillFile],
    file: &RawSkillFile,
) -> Result<ResolvedSkillRole> {
    if file.origin != SkillOrigin::RuntimeRole || file.workspace.is_some() {
        return Err(anyhow!(
            "role skill is not a global runtime role: {}",
            file.package_name
        ));
    }
    let parsed = parse_skill_contents(&file.contents)
        .ok_or_else(|| anyhow!("role skill {} missing valid frontmatter", file.package_name))?;
    if parsed.name != file.package_name {
        return Err(anyhow!(
            "role skill directory {} must match SKILL.md name {}",
            file.package_name,
            parsed.name
        ));
    }
    let provider = role_provider_from_frontmatter(&parsed, Path::new(&file.path))?;
    let skills = resolve_role_skills(runtime_raw, &parsed)?;
    Ok(ResolvedSkillRole {
        name: parsed.name,
        description: parsed.description,
        file_path: PathBuf::from(&file.path),
        content: parsed.body,
        provider,
        skills,
    })
}

fn resolve_role_skills(
    runtime_raw: &[RawSkillFile],
    role: &ParsedSkillFile,
) -> Result<Vec<ResolvedPreloadedSkill>> {
    let mut seen = std::collections::BTreeSet::new();
    role.frontmatter
        .skills
        .iter()
        .map(|requested| {
            let requested = requested.trim();
            if requested.is_empty() || requested.contains('/') {
                return Err(anyhow!(
                    "role skill {} has invalid global skill dependency: {requested}",
                    role.name
                ));
            }
            if !seen.insert(requested.to_string()) {
                return Err(anyhow!(
                    "role skill {} repeats skill dependency: {requested}",
                    role.name
                ));
            }
            let matches = runtime_raw
                .iter()
                .filter(|file| {
                    file.kind == SkillKind::Skill
                        && file.origin == SkillOrigin::HomeGlobal
                        && file.workspace.is_none()
                        && file.package_name == requested
                })
                .collect::<Vec<_>>();
            let file = match matches.as_slice() {
                [file] => *file,
                [] => {
                    return Err(anyhow!(
                        "role skill {} requires unavailable global skill: {requested}",
                        role.name
                    ))
                }
                _ => return Err(anyhow!("global skill is ambiguous: {requested}")),
            };
            let parsed = parse_skill_contents(&file.contents)
                .ok_or_else(|| anyhow!("skill {requested} missing valid frontmatter"))?;
            if parsed.name != file.package_name {
                return Err(anyhow!(
                    "skill directory {} must match SKILL.md name {}",
                    file.package_name,
                    parsed.name
                ));
            }
            Ok(ResolvedPreloadedSkill {
                name: parsed.name,
                file_path: PathBuf::from(&file.path),
                content: parsed.body,
            })
        })
        .collect()
}

fn role_provider_from_frontmatter(
    parsed: &ParsedSkillFile,
    skill_path: &Path,
) -> Result<Option<ProviderConfig>> {
    let model = parsed.frontmatter.model.as_deref();
    let Some(model) = model else {
        if parsed.frontmatter.reasoning_effort.is_some() || parsed.frontmatter.max_tokens.is_some()
        {
            return Err(anyhow!(
                "role skill {} model policy requires model: provider:model",
                skill_path.display()
            ));
        }
        return Ok(None);
    };
    let (provider_name, native_model) = model.split_once(':').ok_or_else(|| {
        anyhow!(
            "role skill {} has malformed model `{model}`; expected provider:model",
            skill_path.display()
        )
    })?;
    if model.trim() != model
        || provider_name.trim() != provider_name
        || native_model.trim() != native_model
        || provider_name.is_empty()
        || native_model.is_empty()
        || provider_name.trim().is_empty()
        || native_model.trim().is_empty()
        || native_model.contains(':')
    {
        return Err(anyhow!(
            "role skill {} has malformed model `{model}`; expected provider:model",
            skill_path.display()
        ));
    }
    let kind = provider_name.parse::<ProviderKind>().map_err(|error| {
        anyhow!(
            "role skill {} has unsupported provider prefix `{provider_name}`: {error}",
            skill_path.display()
        )
    })?;
    let reasoning_effort = parsed
        .frontmatter
        .reasoning_effort
        .as_deref()
        .unwrap_or("medium")
        .parse::<ReasoningEffort>()
        .map_err(|error| {
            anyhow!(
                "role skill {} has invalid reasoning_effort: {error}",
                skill_path.display()
            )
        })?;
    Ok(Some(ProviderConfig {
        kind,
        model: native_model.to_string(),
        reasoning_effort,
        max_tokens: parsed.frontmatter.max_tokens,
        prompt_cache: None,
    }))
}

/// Map a project display name to the `$HOME/.agents/projects/<key>` directory.
pub(crate) fn project_agents_key(name: &str) -> String {
    let mut key = String::new();
    let mut pending_dash = false;
    for ch in name.trim().chars() {
        if ch.is_ascii_whitespace() {
            pending_dash = !key.is_empty();
            continue;
        }
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '-' || lower == '_' {
            if pending_dash {
                key.push('-');
                pending_dash = false;
            }
            key.push(lower);
        }
    }
    key
}

/// Resolve the home-project skill overlay key for a session, if any.
pub(crate) async fn home_project_key(
    repo: &Arc<PostgresAgentStore>,
    project_id: Option<Uuid>,
) -> Result<Option<String>> {
    let Some(project_id) = project_id else {
        return Ok(None);
    };
    let project = repo.get_project(project_id).await?;
    let key = project_agents_key(&project.name);
    if key.is_empty() {
        return Ok(None);
    }
    Ok(Some(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_vocab::{ToolCallId, ToolResultStatus};

    #[test]
    fn project_agents_key_sanitizes_display_names() {
        assert_eq!(project_agents_key("Dynamo"), "dynamo");
        assert_eq!(project_agents_key(" My Project "), "my-project");
        assert_eq!(project_agents_key("foo_bar-2"), "foo_bar-2");
        assert_eq!(project_agents_key("@@@"), "");
    }

    #[test]
    fn load_skill_returns_absolute_path_and_exact_contents() {
        let mut skill = raw_skill(
            SkillKind::Skill,
            SkillOrigin::WorkspaceProject,
            Some("repo"),
            "rust-refactor",
            "",
        );
        skill.contents.push_str("\nSee [examples](examples.md).\n");
        let call = ToolCall {
            id: ToolCallId::new("call_1"),
            tool_name: "LoadSkill".to_string(),
            args_json: serde_json::json!({"name":"repo/rust-refactor"}).to_string(),
        };

        let result = load_skill_result(std::slice::from_ref(&skill), &call);

        assert_eq!(result.status, ToolResultStatus::Success);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&result.output).expect("JSON result"),
            serde_json::json!({
                "path": skill.path,
                "content": skill.contents,
            })
        );
    }

    #[test]
    fn workflow_origin_is_still_an_ordinary_loadable_skill() {
        let workflow = raw_skill(
            SkillKind::Skill,
            SkillOrigin::RuntimeWorkflow,
            None,
            "workflow-review",
            "",
        );
        let call = ToolCall {
            id: ToolCallId::new("call_1"),
            tool_name: "LoadSkill".to_string(),
            args_json: serde_json::json!({"name":"workflow-review"}).to_string(),
        };

        let output = load_skill_result(std::slice::from_ref(&workflow), &call).output;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&output).expect("JSON result"),
            serde_json::json!({
                "path": workflow.path,
                "content": workflow.contents,
            })
        );
    }

    #[test]
    fn role_resolves_provider_and_home_global_preloads() {
        let global = raw_skill(SkillKind::Skill, SkillOrigin::HomeGlobal, None, "swe", "");
        let role = raw_skill(
            SkillKind::SubagentRole,
            SkillOrigin::RuntimeRole,
            None,
            "reviewer",
            "model: claude:claude-opus-4-8\nreasoning_effort: high\nmax_tokens: 4096\nskills:\n  - swe\n",
        );

        let resolved = resolve_skill_role(&[role, global.clone()], "reviewer").expect("role");

        let provider = resolved.provider.expect("provider");
        assert_eq!(provider.kind, ProviderKind::Claude);
        assert_eq!(provider.model, "claude-opus-4-8");
        assert_eq!(provider.reasoning_effort, ReasoningEffort::High);
        assert_eq!(provider.max_tokens, Some(4096));
        assert_eq!(resolved.skills.len(), 1);
        assert_eq!(resolved.skills[0].name, "swe");
        assert_eq!(resolved.skills[0].file_path, PathBuf::from(global.path));
    }

    #[test]
    fn role_rejects_project_or_workflow_preloads() {
        for origin in [
            SkillOrigin::HomeProject,
            SkillOrigin::WorkspaceProject,
            SkillOrigin::RuntimeWorkflow,
        ] {
            let workspace = matches!(
                origin,
                SkillOrigin::HomeProject | SkillOrigin::WorkspaceProject
            )
            .then_some("repo");
            let dependency = raw_skill(SkillKind::Skill, origin, workspace, "special", "");
            let role = raw_skill(
                SkillKind::SubagentRole,
                SkillOrigin::RuntimeRole,
                None,
                "reviewer",
                "skills:\n  - special\n",
            );

            let error =
                resolve_skill_role(&[role, dependency], "reviewer").expect_err("must reject");
            assert!(error.to_string().contains("unavailable global skill"));
        }
    }

    #[test]
    fn role_directory_must_match_frontmatter_name() {
        let mut role = raw_skill(
            SkillKind::SubagentRole,
            SkillOrigin::RuntimeRole,
            None,
            "reviewer",
            "",
        );
        role.contents = role.contents.replace("name: reviewer", "name: tester");

        let error = resolve_skill_role(&[role], "reviewer").expect_err("mismatch");
        assert!(error.to_string().contains("must match"));
    }

    #[test]
    fn role_catalog_omits_unusable_roles() {
        let invalid = raw_skill(
            SkillKind::SubagentRole,
            SkillOrigin::RuntimeRole,
            None,
            "reviewer",
            "model: claude-opus-4-8\n",
        );

        assert!(resolved_role_catalog(&[invalid]).is_empty());
    }

    #[test]
    fn role_rejects_malformed_or_unsupported_composite_model() {
        for (model, expected) in [
            ("claude-opus-4-8", "expected provider:model"),
            (":claude-opus-4-8", "expected provider:model"),
            ("claude:   ", "expected provider:model"),
            (" claude:claude-opus-4-8", "expected provider:model"),
            ("claude: claude-opus-4-8", "expected provider:model"),
            ("claude:claude-opus-4-8 ", "expected provider:model"),
            ("claude::claude-opus-4-8", "expected provider:model"),
            ("bogus:some-model", "unsupported provider prefix"),
        ] {
            let role = raw_skill(
                SkillKind::SubagentRole,
                SkillOrigin::RuntimeRole,
                None,
                "reviewer",
                &format!("model: \"{model}\"\n"),
            );
            let error = resolve_skill_role(&[role], "reviewer").expect_err("must reject");
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn role_ignores_unsupported_kind_frontmatter() {
        let role = raw_skill(
            SkillKind::SubagentRole,
            SkillOrigin::RuntimeRole,
            None,
            "reviewer",
            "kind: claude\nmodel: claude:claude-opus-4-8\n",
        );

        let resolved = resolve_skill_role(&[role], "reviewer").expect("role");
        let provider = resolved.provider.expect("provider");
        assert_eq!(provider.kind, ProviderKind::Claude);
        assert_eq!(provider.model, "claude-opus-4-8");
    }

    fn raw_skill(
        kind: SkillKind,
        origin: SkillOrigin,
        workspace: Option<&str>,
        name: &str,
        extra_frontmatter: &str,
    ) -> RawSkillFile {
        let base = match workspace {
            Some(workspace) => format!("/runtime/workspaces/session/cwd/{workspace}"),
            None if origin == SkillOrigin::HomeGlobal => "/home/test/.agents".to_string(),
            None => "/home/test/.config/pi-relay/runtime".to_string(),
        };
        RawSkillFile {
            kind,
            origin,
            workspace: workspace.map(str::to_string),
            package_name: name.to_string(),
            path: format!("{base}/skills/{name}/SKILL.md"),
            contents: format!(
                "---\nname: {name}\ndescription: {name} description\n{extra_frontmatter}---\n\n{name} body\n"
            ),
        }
    }
}
