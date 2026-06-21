use std::path::Path;

use agent_store::{Delegation, DelegationStatus, SessionActivity, SubagentType};

use crate::handoff::delegation_dir;
use crate::state::AppState;

const RECENT_TERMINAL_DELEGATION_LIMIT: i64 = 3;
const MAX_SUBAGENTS_PER_DELEGATION: usize = 8;
const MAX_FINAL_MESSAGE_CHARS: usize = 700;

/// Build the compact model-context block injected after the rendered PI.md.
///
/// This is intentionally much lighter than `inspect_delegation`: it uses
/// bounded DB reads plus existing artifact path conventions and never refreshes
/// or inlines transcript artifacts. The parent can call `inspect_delegation`
/// for fresh structured state and artifact publication.
pub(crate) async fn current_delegations_context(
    state: &AppState,
    session_id: &str,
) -> anyhow::Result<String> {
    if state
        .repo
        .session_subagent_type(session_id)
        .await?
        .is_some()
    {
        return Ok(String::new());
    }

    let delegations = state
        .repo
        .list_parent_current_delegations(session_id, RECENT_TERMINAL_DELEGATION_LIMIT)
        .await?;
    let Some(first) = delegations.first() else {
        return Ok(empty_current_delegations_context());
    };

    let parent_config = state
        .repo
        .load_session_config(&first.parent_session_id)
        .await?;
    let mut out = String::from("## Current delegations\n\n");
    out.push_str(
        "Compact daemon snapshot for context recovery. Running delegations are always listed; terminal delegations are the most recent 3 because there is no parent-acknowledgement state. Call `inspect_delegation` for fresh full structured state and artifact paths. Full transcript contents are not inlined.\n",
    );

    for delegation in &delegations {
        let progress = state.repo.delegation_progress(delegation).await?;
        let handoff_dir = delegation_dir(&parent_config.outer_cwd, &delegation.id);
        out.push_str(&format!(
            "\n- delegation_id: `{}`; kind: {}; status: {}; progress: expected {}, spawned {}, terminal {}, running {}, failed {}",
            inline_code(&delegation.id),
            delegation.kind,
            delegation.status,
            progress.expected,
            progress.spawned,
            progress.terminal,
            progress.running,
            progress.failed,
        ));
        if let Some(workflow) = delegation
            .workflow
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            out.push_str(&format!("; workflow: `{}`", inline_code(workflow)));
        }
        if let Some(label) = delegation
            .label
            .as_deref()
            .filter(|value| !value.is_empty())
        {
            out.push_str(&format!("; label: `{}`", inline_code(label)));
        }
        out.push_str(&format!(
            "; handoff_dir: `{}`",
            inline_code(&handoff_dir.to_string_lossy())
        ));
        out.push('\n');
        append_subagents(state, &mut out, delegation, &handoff_dir).await?;
    }

    Ok(out.trim_end().to_string())
}

fn empty_current_delegations_context() -> String {
    "## Current delegations\n\nNo running delegations and no recent terminal delegations."
        .to_string()
}

async fn append_subagents(
    state: &AppState,
    out: &mut String,
    delegation: &Delegation,
    handoff_dir: &Path,
) -> anyhow::Result<()> {
    let subagents = state.repo.list_delegation_subagents(&delegation.id).await?;
    let shown = subagents.len().min(MAX_SUBAGENTS_PER_DELEGATION);
    for subagent in subagents.iter().take(shown) {
        let terminal = state
            .repo
            .active_leaf_is_turn_boundary(&subagent.session_id)
            .await
            .unwrap_or(false);
        let status = subagent_status(delegation.status, terminal, subagent.activity);
        let has_active_work = matches!(delegation.status, DelegationStatus::Running)
            && subagent.subagent_type == Some(SubagentType::Full)
            && !terminal
            && (subagent.activity != SessionActivity::Idle
                || subagent_has_active_runtime(state, &subagent.session_id).await);
        let steerable = matches!(delegation.status, DelegationStatus::Running)
            && subagent.subagent_type == Some(SubagentType::Full)
            && !terminal
            && has_active_work;
        let transcript_file = transcript_file_for(delegation.status, &subagent.session_id);
        out.push_str(&format!(
            "  - subagent_id: `{}`; role: {}; type: {}; activity: {}; status: {}; steerable: {}; transcript_file: `{}`",
            inline_code(&subagent.session_id),
            optional_inline_code(subagent.role.as_deref()),
            optional_type(subagent.subagent_type),
            subagent.activity,
            status,
            steerable,
            inline_code(&transcript_file),
        ));
        if final_message_relevant(delegation.status) {
            let final_message_path = handoff_dir
                .join(&subagent.session_id)
                .join("final_message.md");
            out.push_str(&format!(
                "; final_message_file: `{}/final_message.md`",
                inline_code(&subagent.session_id)
            ));
            if let Some((final_message, suggested_next)) =
                read_bounded_final_message(&final_message_path).await
            {
                out.push_str(&format!(
                    "; final_message: {}",
                    serde_json::to_string(&final_message)?
                ));
                if let Some(suggested_next) = suggested_next {
                    out.push_str(&format!(
                        "; suggested_next: {}",
                        serde_json::to_string(&suggested_next)?
                    ));
                }
            }
        }
        out.push('\n');
    }
    if subagents.len() > shown {
        out.push_str(&format!(
            "  - ... {} more subagent(s) omitted from compact context; call `inspect_delegation`.\n",
            subagents.len() - shown
        ));
    }
    Ok(())
}

async fn subagent_has_active_runtime(state: &AppState, subagent_id: &str) -> bool {
    state.active.lock().await.contains_key(subagent_id)
}

fn subagent_status(
    delegation_status: DelegationStatus,
    terminal: bool,
    activity: SessionActivity,
) -> String {
    match delegation_status {
        DelegationStatus::Running if terminal => "terminal".to_string(),
        DelegationStatus::Running => activity.to_string(),
        DelegationStatus::Done | DelegationStatus::DoneWithFailures if terminal => {
            "terminal".to_string()
        }
        DelegationStatus::Done | DelegationStatus::DoneWithFailures => {
            delegation_status.as_str().to_string()
        }
        DelegationStatus::Cancelled | DelegationStatus::Failed => delegation_status.to_string(),
    }
}

fn transcript_file_for(status: DelegationStatus, subagent_id: &str) -> String {
    match status {
        DelegationStatus::Cancelled => format!("cancelled/{subagent_id}.transcript.md"),
        _ => format!("{subagent_id}/transcript.md"),
    }
}

fn final_message_relevant(status: DelegationStatus) -> bool {
    matches!(
        status,
        DelegationStatus::Done | DelegationStatus::DoneWithFailures
    )
}

async fn read_bounded_final_message(path: &Path) -> Option<(String, Option<String>)> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    let suggested_next = extract_suggested_next(trimmed);
    Some((
        truncate_chars(trimmed, MAX_FINAL_MESSAGE_CHARS),
        suggested_next,
    ))
}

fn truncate_chars(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated = chars.by_ref().take(max_chars).collect::<String>();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

fn extract_suggested_next(final_message: &str) -> Option<String> {
    final_message
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .and_then(|line| line.strip_prefix("suggested_next:"))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(truncate_suggested_next)
}

fn truncate_suggested_next(value: &str) -> String {
    truncate_chars(value, 120)
}

fn optional_inline_code(value: Option<&str>) -> String {
    value
        .filter(|value| !value.is_empty())
        .map(|value| format!("`{}`", inline_code(value)))
        .unwrap_or_else(|| "null".to_string())
}

fn optional_type(value: Option<SubagentType>) -> String {
    value
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

fn inline_code(value: &str) -> String {
    value
        .chars()
        .map(|ch| match ch {
            '`' => "\\`".to_string(),
            '\n' => "\\n".to_string(),
            '\r' => "\\r".to_string(),
            other => other.to_string(),
        })
        .collect::<String>()
}

#[cfg(test)]
pub(crate) fn test_empty_current_delegations_context() -> String {
    empty_current_delegations_context()
}

#[cfg(test)]
pub(crate) fn test_context_from_snapshots(
    delegations: Vec<(
        Delegation,
        agent_store::DelegationProgress,
        Vec<TestSubagent>,
    )>,
    parent_outer_cwd: &str,
) -> anyhow::Result<String> {
    let mut out = String::from("## Current delegations\n\n");
    out.push_str(
        "Compact daemon snapshot for context recovery. Running delegations are always listed; terminal delegations are the most recent 3 because there is no parent-acknowledgement state. Call `inspect_delegation` for fresh full structured state and artifact paths. Full transcript contents are not inlined.\n",
    );
    for (delegation, progress, subagents) in delegations {
        let handoff_dir = delegation_dir(parent_outer_cwd, &delegation.id);
        out.push_str(&format!(
            "\n- delegation_id: `{}`; kind: {}; status: {}; progress: expected {}, spawned {}, terminal {}, running {}, failed {}; handoff_dir: `{}`\n",
            inline_code(&delegation.id),
            delegation.kind,
            delegation.status,
            progress.expected,
            progress.spawned,
            progress.terminal,
            progress.running,
            progress.failed,
            inline_code(&handoff_dir.to_string_lossy())
        ));
        let shown = subagents.len().min(MAX_SUBAGENTS_PER_DELEGATION);
        for subagent in subagents.iter().take(shown) {
            out.push_str(&format!(
                "  - subagent_id: `{}`; role: {}; type: {}; activity: {}; status: {}; steerable: {}; transcript_file: `{}`",
                inline_code(&subagent.session_id),
                optional_inline_code(subagent.role.as_deref()),
                optional_type(subagent.subagent_type),
                subagent.activity,
                subagent.status,
                subagent.steerable,
                inline_code(&transcript_file_for(
                    delegation.status,
                    &subagent.session_id
                )),
            ));
            if final_message_relevant(delegation.status) {
                out.push_str(&format!(
                    "; final_message_file: `{}/final_message.md`",
                    inline_code(&subagent.session_id)
                ));
                if let Some(final_message) = subagent.final_message.as_deref() {
                    let trimmed_final_message = final_message.trim();
                    if !trimmed_final_message.is_empty() {
                        let suggested_next = extract_suggested_next(trimmed_final_message);
                        let final_message =
                            truncate_chars(trimmed_final_message, MAX_FINAL_MESSAGE_CHARS);
                        out.push_str(&format!(
                            "; final_message: {}",
                            serde_json::to_string(&final_message)?
                        ));
                        if let Some(suggested_next) = suggested_next {
                            out.push_str(&format!(
                                "; suggested_next: {}",
                                serde_json::to_string(&suggested_next)?
                            ));
                        }
                    }
                }
            }
            out.push('\n');
        }
        if subagents.len() > shown {
            out.push_str(&format!(
                "  - ... {} more subagent(s) omitted from compact context; call `inspect_delegation`.\n",
                subagents.len() - shown
            ));
        }
    }
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub(crate) struct TestSubagent {
    pub(crate) session_id: String,
    pub(crate) role: Option<String>,
    pub(crate) subagent_type: Option<SubagentType>,
    pub(crate) activity: SessionActivity,
    pub(crate) status: String,
    pub(crate) steerable: bool,
    pub(crate) final_message: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_store::{DelegationKind, DelegationProgress};

    fn delegation(id: &str, status: DelegationStatus) -> Delegation {
        Delegation {
            id: id.to_string(),
            parent_session_id: "parent".to_string(),
            workflow: None,
            label: None,
            kind: DelegationKind::ReadonlyFanout,
            status,
            attempt_id: "attempt".to_string(),
            expected_subagents: 1,
        }
    }

    #[test]
    fn empty_context_is_explicit_and_compact() {
        assert_eq!(
            test_empty_current_delegations_context(),
            "## Current delegations\n\nNo running delegations and no recent terminal delegations."
        );
    }

    #[test]
    fn bounded_context_omits_transcript_bodies_and_truncates_final_message() {
        let long_final = format!(
            "{}\n\nsuggested_next: approved",
            "final-message ".repeat(100)
        );
        let subagents = (0..10)
            .map(|index| TestSubagent {
                session_id: format!("child_{index}"),
                role: Some("reviewer".to_string()),
                subagent_type: Some(SubagentType::ReadOnly),
                activity: SessionActivity::Idle,
                status: "terminal".to_string(),
                steerable: false,
                final_message: Some(long_final.clone()),
            })
            .collect::<Vec<_>>();
        let text = test_context_from_snapshots(
            vec![(
                delegation("delegation_1", DelegationStatus::Done),
                DelegationProgress {
                    expected: 10,
                    spawned: 10,
                    terminal: 10,
                    running: 0,
                    failed: 0,
                },
                subagents,
            )],
            "/tmp/session",
        )
        .expect("render context");

        assert!(text.starts_with("## Current delegations"));
        assert!(text.contains("... 2 more subagent(s) omitted"));
        assert!(text.contains("final_message_file: `child_0/final_message.md`"));
        assert!(text.contains("suggested_next"));
        assert!(text.contains('…'));
        assert!(text.contains("Full transcript contents are not inlined"));
        assert!(!text.contains("## User"));
        assert!(!text.contains("## Assistant"));
        assert!(!text.contains("transcript body"));
        assert!(!text.contains("child_9/final_message.md"));
    }

    #[test]
    fn running_context_has_steerability_and_no_final_message_inline() {
        let text = test_context_from_snapshots(
            vec![(
                delegation("delegation_running", DelegationStatus::Running),
                DelegationProgress {
                    expected: 1,
                    spawned: 1,
                    terminal: 0,
                    running: 1,
                    failed: 0,
                },
                vec![TestSubagent {
                    session_id: "impl".to_string(),
                    role: Some("implementer".to_string()),
                    subagent_type: Some(SubagentType::Full),
                    activity: SessionActivity::Running,
                    status: "running".to_string(),
                    steerable: true,
                    final_message: Some("not yet relevant".to_string()),
                }],
            )],
            "/tmp/session",
        )
        .expect("render context");

        assert!(text.contains("status: running"));
        assert!(text.contains("steerable: true"));
        assert!(text.contains("transcript_file: `impl/transcript.md`"));
        assert!(!text.contains("final_message:"));
        assert!(!text.contains("not yet relevant"));
    }
}
