use std::path::Path;

use agent_store::{Delegation, DelegationStatus, SessionActivity, SubagentType};

use crate::handoff::{delegation_dir, extract_suggested_next};
use crate::state::AppState;

const MAX_SUBAGENTS_PER_DELEGATION: usize = 8;
const MAX_SUGGESTED_NEXT_CHARS: usize = 120;

/// Build the compaction-only delegation ledger for a top-level parent session.
///
/// Normal parent model requests are transcript-driven and do not receive this
/// block. Compaction is the special case: older delegation start/completion
/// steers may be summarized away, so the daemon appends a bounded but complete
/// ledger of every delegation row owned by the parent session after the provider
/// returns its compacted summary.
///
/// Subagent sessions deliberately receive no parent/sibling delegation ledger:
/// subagents summarize only their own role contract, delegated task, transcript,
/// and tool results. Parent sessions own orchestration state, and nested
/// delegations are currently rejected.
///
/// This is intentionally much lighter than `inspect_delegation`: it uses
/// bounded DB reads plus existing artifact path conventions and never refreshes
/// or inlines transcript artifacts. The parent can call `inspect_delegation`
/// after compaction for fresh structured state and artifact publication.
pub(crate) async fn compaction_delegation_ledger(
    state: &AppState,
    session_id: &str,
) -> anyhow::Result<Option<String>> {
    if state
        .repo
        .session_subagent_type(session_id)
        .await?
        .is_some()
    {
        return Ok(None);
    }

    let parent_config = state.repo.load_session_config(session_id).await?;
    let delegations = state.repo.list_parent_delegations(session_id).await?;
    let Some(_) = delegations.first() else {
        return Ok(Some(empty_compaction_delegation_ledger()));
    };
    let mut out = ledger_header();

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
        append_compaction_status_note(&mut out, delegation.status);
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
        append_subagents(state, &mut out, delegation, progress.spawned, &handoff_dir).await?;
    }

    Ok(Some(out.trim_end().to_string()))
}

fn ledger_header() -> String {
    let mut out = String::from("## Delegation state at compaction time\n\n");
    out.push_str(
        "Point-in-time compaction ledger for this parent session. It lists every delegation row for the parent session, including running, done, done_with_failures, cancelled, and failed statuses. Per-subagent control-flow details and artifact file references are bounded. Full transcript and final-message contents are not inlined.\n\n",
    );
    out.push_str(
        "This section is appended after provider compaction so fresh delegation facts cross the compaction boundary. Distinguish delegations that completed, were cancelled, or failed before compaction from delegations that were still running at compaction time. A running delegation entry is only a point-in-time fact: do not assume it completed; wait for a later completion observation or call `inspect_delegation`.\n",
    );
    out
}

fn empty_compaction_delegation_ledger() -> String {
    let mut out = ledger_header();
    out.push_str("\nNo delegations existed for this parent session at compaction time.");
    out
}

fn append_compaction_status_note(out: &mut String, status: DelegationStatus) {
    let note = match status {
        DelegationStatus::Running => {
            "running at compaction time; point-in-time only; await later completion observation or inspect_delegation"
        }
        DelegationStatus::Done => "completed before compaction",
        DelegationStatus::DoneWithFailures => "completed with failures before compaction",
        DelegationStatus::Cancelled => "cancelled before compaction",
        DelegationStatus::Failed => "failed before compaction",
    };
    out.push_str(&format!("; compaction_note: {note}"));
}

async fn append_subagents(
    state: &AppState,
    out: &mut String,
    delegation: &Delegation,
    spawned_count: i32,
    handoff_dir: &Path,
) -> anyhow::Result<()> {
    let subagents = state
        .repo
        .list_delegation_subagents_for_context(&delegation.id, MAX_SUBAGENTS_PER_DELEGATION as i64)
        .await?;
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
        out.push_str(&format!(
            "  - subagent_id: `{}`; role: {}; type: {}; activity: {}; status: {}; steerable: {}; transcript_file: {}",
            inline_code(&subagent.session_id),
            optional_inline_code(subagent.role.as_deref()),
            optional_type(subagent.subagent_type),
            subagent.activity,
            status,
            steerable,
            optional_transcript_file(delegation.status, &subagent.session_id),
        ));
        if final_message_relevant(delegation.status) {
            let final_message_path = handoff_dir
                .join(&subagent.session_id)
                .join("final_message.md");
            out.push_str(&format!(
                "; final_message_file: `{}/final_message.md`",
                inline_code(&subagent.session_id)
            ));
            if let Some(suggested_next) = read_suggested_next(&final_message_path).await {
                out.push_str(&format!(
                    "; suggested_next: {}",
                    serde_json::to_string(&suggested_next)?
                ));
            }
        }
        out.push('\n');
    }
    let omitted = (spawned_count.max(0) as usize)
        .saturating_sub(shown)
        .max(subagents.len().saturating_sub(shown));
    if omitted > 0 {
        out.push_str(&format!(
            "  - ... {} more subagent(s) omitted from compaction ledger; call `inspect_delegation`.\n",
            omitted
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

fn optional_transcript_file(status: DelegationStatus, subagent_id: &str) -> String {
    transcript_file_for(status, subagent_id)
        .map(|file| format!("`{}`", inline_code(&file)))
        .unwrap_or_else(|| "null".to_string())
}

fn transcript_file_for(status: DelegationStatus, subagent_id: &str) -> Option<String> {
    match status {
        DelegationStatus::Cancelled => Some(format!("cancelled/{subagent_id}.transcript.md")),
        DelegationStatus::Failed => None,
        _ => Some(format!("{subagent_id}/transcript.md")),
    }
}

fn final_message_relevant(status: DelegationStatus) -> bool {
    matches!(
        status,
        DelegationStatus::Done | DelegationStatus::DoneWithFailures
    )
}

async fn read_suggested_next(path: &Path) -> Option<String> {
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }
    bounded_suggested_next(trimmed)
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

fn bounded_suggested_next(final_message: &str) -> Option<String> {
    extract_suggested_next(final_message)
        .map(|value| truncate_chars(&value, MAX_SUGGESTED_NEXT_CHARS))
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
pub(crate) fn test_empty_compaction_delegation_ledger() -> String {
    empty_compaction_delegation_ledger()
}

#[cfg(test)]
pub(crate) fn test_ledger_from_snapshots(
    delegations: Vec<(
        Delegation,
        agent_store::DelegationProgress,
        Vec<TestSubagent>,
    )>,
    parent_outer_cwd: &str,
) -> anyhow::Result<String> {
    let mut out = ledger_header();
    for (delegation, progress, subagents) in delegations {
        let handoff_dir = delegation_dir(parent_outer_cwd, &delegation.id);
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
        append_compaction_status_note(&mut out, delegation.status);
        out.push_str(&format!(
            "; handoff_dir: `{}`\n",
            inline_code(&handoff_dir.to_string_lossy())
        ));
        let shown = subagents.len().min(MAX_SUBAGENTS_PER_DELEGATION);
        for subagent in subagents.iter().take(shown) {
            out.push_str(&format!(
                "  - subagent_id: `{}`; role: {}; type: {}; activity: {}; status: {}; steerable: {}; transcript_file: {}",
                inline_code(&subagent.session_id),
                optional_inline_code(subagent.role.as_deref()),
                optional_type(subagent.subagent_type),
                subagent.activity,
                subagent.status,
                subagent.steerable,
                optional_transcript_file(delegation.status, &subagent.session_id),
            ));
            if final_message_relevant(delegation.status) {
                out.push_str(&format!(
                    "; final_message_file: `{}/final_message.md`",
                    inline_code(&subagent.session_id)
                ));
                if let Some(final_message) = subagent.final_message.as_deref() {
                    let trimmed_final_message = final_message.trim();
                    if !trimmed_final_message.is_empty() {
                        if let Some(suggested_next) = bounded_suggested_next(trimmed_final_message)
                        {
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
        let omitted = (progress.spawned.max(0) as usize)
            .saturating_sub(shown)
            .max(subagents.len().saturating_sub(shown));
        if omitted > 0 {
            out.push_str(&format!(
                "  - ... {} more subagent(s) omitted from compaction ledger; call `inspect_delegation`.\n",
                omitted
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
    fn empty_compaction_ledger_is_explicit_and_compact() {
        let text = test_empty_compaction_delegation_ledger();
        assert!(text.starts_with("## Delegation state at compaction time"));
        assert!(text.contains("No delegations existed for this parent session at compaction time."));
        assert!(!text.contains("## Current delegations"));
    }

    #[test]
    fn bounded_compaction_ledger_omits_transcript_and_final_message_bodies() {
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
        let text = test_ledger_from_snapshots(
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

        assert!(text.starts_with("## Delegation state at compaction time"));
        assert!(text.contains("completed before compaction"));
        assert!(text.contains("... 2 more subagent(s) omitted"));
        assert!(text.contains("final_message_file: `child_0/final_message.md`"));
        assert!(text.contains("suggested_next: \"approved\""));
        assert!(text.contains("Full transcript and final-message contents are not inlined"));
        assert!(!text.contains("final-message final-message"));
        assert!(!text.contains("## User"));
        assert!(!text.contains("## Assistant"));
        assert!(!text.contains("transcript body"));
        assert!(!text.contains("child_9/final_message.md"));
    }

    #[test]
    fn running_compaction_ledger_has_point_in_time_status_and_no_final_message_inline() {
        let text = test_ledger_from_snapshots(
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
        assert!(text.contains("running at compaction time; point-in-time only"));
        assert!(text.contains("steerable: true"));
        assert!(text.contains("transcript_file: `impl/transcript.md`"));
        assert!(!text.contains("final_message:"));
        assert!(!text.contains("not yet relevant"));
    }

    #[test]
    fn omitted_count_uses_spawned_progress_not_loaded_subagent_count() {
        let subagents = (0..9)
            .map(|index| TestSubagent {
                session_id: format!("child_{index}"),
                role: Some("reviewer".to_string()),
                subagent_type: Some(SubagentType::ReadOnly),
                activity: SessionActivity::Idle,
                status: "terminal".to_string(),
                steerable: false,
                final_message: None,
            })
            .collect::<Vec<_>>();
        let text = test_ledger_from_snapshots(
            vec![(
                delegation("delegation_large", DelegationStatus::Done),
                DelegationProgress {
                    expected: 20,
                    spawned: 20,
                    terminal: 20,
                    running: 0,
                    failed: 0,
                },
                subagents,
            )],
            "/tmp/session",
        )
        .expect("render context");

        assert!(text.contains("... 12 more subagent(s) omitted"));
        assert!(text.contains("subagent_id: `child_7`"));
        assert!(!text.contains("subagent_id: `child_8`"));
    }

    #[test]
    fn failed_delegation_ledger_does_not_point_at_normal_transcript_artifacts() {
        let text = test_ledger_from_snapshots(
            vec![(
                delegation("delegation_failed", DelegationStatus::Failed),
                DelegationProgress {
                    expected: 1,
                    spawned: 1,
                    terminal: 0,
                    running: 0,
                    failed: 0,
                },
                vec![TestSubagent {
                    session_id: "impl_failed".to_string(),
                    role: Some("implementer".to_string()),
                    subagent_type: Some(SubagentType::Full),
                    activity: SessionActivity::Idle,
                    status: "failed".to_string(),
                    steerable: false,
                    final_message: None,
                }],
            )],
            "/tmp/session",
        )
        .expect("render context");

        assert!(text.contains("transcript_file: null"));
        assert!(text.contains("failed before compaction"));
        assert!(!text.contains("impl_failed/transcript.md"));
        assert!(!text.contains("final_message_file:"));
    }
}
