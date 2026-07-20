//! The delegation handoff directory writer.
//!
//! On the delegation barrier, for every subagent (success or failure) the daemon
//! renders two files from the durable Postgres transcript — `final_message.md`
//! and an exhaustive, greppable `transcript.md` — under
//! `<parent.workspace_id>/.pi-handoff/<delegation_id>/`. `inspect_delegation` is the
//! structured control-flow snapshot; the files remain transcript/detail
//! artifacts only. Postgres transcript history is the durable source of truth:
//! these artifact files are derived from `active_branch` (Ui body mode) and can
//! be re-rendered after an RO subagent's filesystem snapshot is destroyed or a
//! daemon crash leaves publication incomplete.
//!
//! The writer is intentionally idempotent, but normal completion only publishes
//! these files after it wins the DB terminal-status CAS. That ordering prevents
//! a concurrent cancellation from receiving a normal completed handoff. Boot
//! repair may re-render the same files for already-completed delegations if the
//! daemon crashed after the status CAS but before publication.

use std::path::{Component, Path};

use agent_store::{Delegation, DelegationStatus, HistoryTree};
use agent_vocab::{
    AssistantMessage, ContentBlock, ToolResultStatus, TranscriptItem, TurnOutcome, UserMessage,
};

use crate::state::AppState;
use crate::types::RpcError;

pub(crate) const HANDOFF_DIR: &str = ".pi-handoff";
pub(crate) const TASK_PROMPT_FILE: &str = "task_prompt.md";

/// The per-subagent handoff outcome, derived from the subagent's terminal
/// `TurnOutcome`. Graceful is `done`; an interrupted or crashed turn is
/// `failed`. Mirrors the lifecycle classification in `runtime/mod.rs`.
fn subagent_status(outcome: TurnOutcome) -> &'static str {
    match outcome {
        TurnOutcome::Graceful => "done",
        TurnOutcome::Interrupted | TurnOutcome::Crashed => "failed",
    }
}

/// The terminal `TurnOutcome` of a subagent, read from its durable transcript:
/// the most recent `TurnFinished` marker on its active branch. A subagent with
/// no finished turn (e.g. crashed before any boundary) is treated as crashed.
#[cfg(test)]
pub(crate) fn subagent_outcome(history: &HistoryTree) -> TurnOutcome {
    history
        .entries
        .iter()
        .rev()
        .find_map(|entry| match &entry.item {
            TranscriptItem::TurnFinished { outcome, .. } => Some(*outcome),
            _ => None,
        })
        .unwrap_or(TurnOutcome::Crashed)
}

/// The compact terminal status for a subagent active branch.
///
/// This mirrors the store's delegation progress convention: an empty active
/// branch (`active_leaf_id == None`) and a compaction-summary leaf are terminal
/// non-failures. Only a durable `TurnFinished` outcome can mark a terminal
/// subagent as failed.
pub(crate) fn terminal_subagent_status(history: &HistoryTree) -> Option<&'static str> {
    let Some(active_leaf_id) = history.active_leaf_id.as_deref() else {
        return Some("done");
    };
    let leaf = history
        .entries
        .iter()
        .rev()
        .find(|entry| entry.id == active_leaf_id)?;
    match &leaf.item {
        TranscriptItem::TurnFinished { outcome, .. } => Some(subagent_status(*outcome)),
        TranscriptItem::CompactionSummary(_) => Some("done"),
        _ => None,
    }
}

/// Whether the active branch is at a durable turn boundary. This mirrors the
/// store's terminality predicate for the active branch, but works from the
/// `HistoryTree` already loaded to render artifacts.
pub(crate) fn active_branch_is_terminal(history: &HistoryTree) -> bool {
    let Some(active_leaf_id) = history.active_leaf_id.as_deref() else {
        return true;
    };
    history
        .entries
        .iter()
        .rev()
        .find(|entry| entry.id == active_leaf_id)
        .is_some_and(|entry| {
            matches!(
                entry.item,
                TranscriptItem::TurnFinished { .. } | TranscriptItem::CompactionSummary(_)
            )
        })
}

fn user_message_text(message: &UserMessage) -> String {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.as_str(),
            ContentBlock::Image { .. } => "[image]",
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn fenced(language: &str, body: &str) -> String {
    format!("```{language}\n{}\n```", body.trim_end())
}

fn tool_status_label(status: &ToolResultStatus) -> &'static str {
    match status {
        ToolResultStatus::Success => "success",
        ToolResultStatus::Error => "error",
        ToolResultStatus::Interrupted => "interrupted",
        ToolResultStatus::Crashed => "crashed",
    }
}

/// Render a subagent's full active branch to exhaustive, greppable markdown.
///
/// Tool calls live in BOTH `AssistantMessage.items` and standalone
/// `ToolCallStarted` entries (see `agent-core/src/state.rs`); we render them
/// only from the `ToolCallStarted` entries so each call appears once and stays
/// adjacent to its result. The `AssistantMessage` arm renders text only.
/// Nothing is truncated — the handoff is the durable record.
pub(crate) fn render_transcript_markdown(history: &HistoryTree) -> String {
    let mut out = String::new();
    for entry in &history.entries {
        match &entry.item {
            TranscriptItem::UserMessage(message) => {
                let text = user_message_text(message);
                let text = text.trim();
                if !text.is_empty() {
                    out.push_str(&format!("## User\n\n{text}\n\n"));
                }
            }
            TranscriptItem::AssistantMessage(message) => {
                let text = message.text();
                let text = text.trim();
                if !text.is_empty() {
                    out.push_str(&format!("## Assistant\n\n{text}\n\n"));
                }
            }
            TranscriptItem::ToolCallStarted { tool_call, .. } => {
                let args = tool_call
                    .args_value()
                    .map(|value| {
                        serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string())
                    })
                    .unwrap_or_else(|_| tool_call.args_json.clone());
                out.push_str(&format!(
                    "### Tool call: {}\n\n{}\n\n",
                    tool_call.tool_name,
                    fenced("json", &args)
                ));
            }
            TranscriptItem::ToolResult(result) => {
                out.push_str(&format!(
                    "### Tool result: {} [{}]\n\n{}\n\n",
                    result.tool_name,
                    tool_status_label(&result.status),
                    fenced("", &result.output)
                ));
            }
            TranscriptItem::CompactionSummary(summary) => {
                out.push_str(&format!(
                    "## Compaction summary\n\n{}\n\n",
                    summary.summary.trim()
                ));
            }
            TranscriptItem::DaemonToolObservation(observation) => {
                let text = observation
                    .render_text()
                    .unwrap_or_else(|_| "Daemon observation could not be rendered.".to_string());
                out.push_str(&format!("## Daemon observation\n\n{}\n\n", text.trim()));
            }
            TranscriptItem::TurnStarted { .. } | TranscriptItem::TurnFinished { .. } => {}
        }
    }
    out
}

/// Extract a subagent's final message: the last non-empty assistant text on its
/// active branch.
pub(crate) fn extract_final_message(history: &HistoryTree) -> String {
    history
        .entries
        .iter()
        .rev()
        .find_map(|entry| match &entry.item {
            TranscriptItem::AssistantMessage(message) => non_empty_text(message),
            _ => None,
        })
        .unwrap_or_default()
}

fn non_empty_text(message: &AssistantMessage) -> Option<String> {
    let text = message.text();
    (!text.trim().is_empty()).then_some(text)
}

/// Extract a typed `outcome` edge label from a final message: if the message's
/// last non-empty line is `outcome: <value>`, return the raw `<value>`. It is
/// recorded verbatim and never validated against a workflow's outcome set, so an
/// out-of-set value is preserved (the parent branches on it with judgment)
/// rather than crashing the handoff.
///
/// Legacy fallback: historical `final_message.md` artifacts emitted the line as
/// `suggested_next: <value>` before this field was renamed to `outcome`. If no
/// `outcome:` line is found, a trailing `suggested_next:` line is still accepted
/// so those older artifacts continue to parse.
pub(crate) fn extract_outcome(final_message: &str) -> Option<String> {
    let last_line = final_message
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let value = last_line
        .strip_prefix("outcome:")
        // Legacy fallback for pre-rename artifacts.
        .or_else(|| last_line.strip_prefix("suggested_next:"))?
        .trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentArtifact {
    pub(crate) session_id: String,
    pub(crate) terminal_status: Option<&'static str>,
    pub(crate) outcome: Option<String>,
    pub(crate) has_final_message: bool,
    pub(crate) has_task_prompt: bool,
}

impl SubagentArtifact {
    pub(crate) fn final_message_rel(&self) -> Option<String> {
        self.has_final_message
            .then(|| format!("{}/final_message.md", self.session_id))
    }

    pub(crate) fn transcript_rel(&self) -> String {
        format!("{}/transcript.md", self.session_id)
    }

    pub(crate) fn task_prompt_rel(&self) -> Option<String> {
        self.has_task_prompt
            .then(|| task_prompt_rel(&self.session_id))
    }
}

pub(crate) fn task_prompt_rel(session_id: &str) -> String {
    format!("{session_id}/{TASK_PROMPT_FILE}")
}

/// Reject any dynamic path segment that is not a plain file/dir name. Every
/// handoff writer should validate public ids before joining them onto the
/// trusted handoff root.
pub(crate) fn safe_handoff_path_segment(
    segment: &str,
    field: &str,
) -> std::result::Result<String, RpcError> {
    let trimmed = segment.trim();
    let reject = || {
        RpcError::new(
            "invalid_params",
            format!("{field} is not a valid path segment"),
        )
    };
    if trimmed.is_empty() || trimmed.contains('\0') {
        return Err(reject());
    }
    let mut components = Path::new(trimmed).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) if name == std::ffi::OsStr::new(trimmed) => {
            Ok(trimmed.to_string())
        }
        _ => Err(reject()),
    }
}

pub(crate) async fn refresh_task_prompt_artifact(
    state: &AppState,
    runtime_id: &str,
    workspace_id: &str,
    delegation_rel_dir: &str,
    session_id: &str,
    task: &str,
) -> std::result::Result<(), RpcError> {
    let session_segment = safe_handoff_path_segment(session_id, "subagent_id")?;
    let rel_path = format!("{delegation_rel_dir}/{session_segment}/{TASK_PROMPT_FILE}");
    state
        .runtime_hosts
        .write_workspace_file(runtime_id, workspace_id, &rel_path, task)
        .await?;
    Ok(())
}

pub(crate) async fn refresh_task_prompt_artifact_if_present(
    state: &AppState,
    runtime_id: &str,
    workspace_id: &str,
    delegation_rel_dir: &str,
    session_id: &str,
    task: Option<&str>,
) -> std::result::Result<bool, RpcError> {
    safe_handoff_path_segment(session_id, "subagent_id")?;
    let Some(task) = task.filter(|task| !task.trim().is_empty()) else {
        return Ok(false);
    };
    refresh_task_prompt_artifact(
        state,
        runtime_id,
        workspace_id,
        delegation_rel_dir,
        session_id,
        task,
    )
    .await?;
    Ok(true)
}

/// The cwd-relative directory (on the session's runtime) that holds a
/// delegation's handoff artifacts: `.pi-handoff/<delegation_id>`. It is
/// relative to the session cwd so runtime-side file tools can read it.
pub(crate) fn delegation_dir(delegation_id: &str) -> String {
    format!("{HANDOFF_DIR}/{delegation_id}")
}

/// Refresh per-subagent handoff artifacts from durable Postgres transcripts.
///
/// This writes each subagent's exhaustive `transcript.md` on every call. When
/// `include_final_messages` is true, it also writes `final_message.md` for
/// subagents whose current delegation status permits final content: terminal
/// children while a delegation is still running, or every child after
/// done/done_with_failures. Returned metadata may include `outcome` for
/// subagents whose final-message content is publishable, but final-message prose
/// is not returned to snapshots. Cancelled and failed delegations do not publish
/// normal per-subagent handoff artifacts; cancellation has its own
/// transcript-only artifact path.
pub(crate) async fn refresh_delegation_handoff_artifacts(
    state: &AppState,
    delegation: &Delegation,
    include_final_messages: bool,
) -> std::result::Result<(String, Vec<SubagentArtifact>), RpcError> {
    let parent_config = state
        .repo
        .load_session_config(&delegation.parent_session_id)
        .await?;
    let runtime_id = parent_config.runtime_id.as_str();
    let workspace_id = parent_config.workspace_id.as_str();
    let dir = delegation_dir(&delegation.id);

    if matches!(
        delegation.status,
        DelegationStatus::Cancelled | DelegationStatus::Failed
    ) {
        return Ok((dir, Vec::new()));
    }

    let subagents = state.repo.list_delegation_subagents(&delegation.id).await?;

    let mut artifacts = Vec::with_capacity(subagents.len());
    for subagent in &subagents {
        let history = state.repo.active_branch(&subagent.session_id).await?;
        let is_terminal = active_branch_is_terminal(&history);
        let final_message = extract_final_message(&history);
        let transcript = render_transcript_markdown(&history);
        let status = terminal_subagent_status(&history);
        let outcome = extract_outcome(&final_message);
        let include_final_content = match delegation.status {
            DelegationStatus::Running => {
                crate::delegation_tools::load_subagent_work_state(state, &subagent.session_id)
                    .await?
                    .is_completion_terminal()
            }
            DelegationStatus::Done | DelegationStatus::DoneWithFailures => true,
            DelegationStatus::Cancelled | DelegationStatus::Failed => false,
        };

        let has_task_prompt = refresh_task_prompt_artifact_if_present(
            state,
            runtime_id,
            workspace_id,
            &dir,
            &subagent.session_id,
            subagent.task.as_deref(),
        )
        .await?;
        let should_write_final_message = include_final_messages && include_final_content;
        if should_write_final_message {
            state
                .runtime_hosts
                .write_workspace_file(
                    runtime_id,
                    workspace_id,
                    &format!("{dir}/{}/final_message.md", subagent.session_id),
                    &final_message,
                )
                .await?;
        }
        state
            .runtime_hosts
            .write_workspace_file(
                runtime_id,
                workspace_id,
                &format!("{dir}/{}/transcript.md", subagent.session_id),
                &transcript,
            )
            .await?;

        artifacts.push(SubagentArtifact {
            session_id: subagent.session_id.clone(),
            terminal_status: is_terminal.then_some(status).flatten(),
            outcome: include_final_content.then_some(outcome).flatten(),
            has_final_message: should_write_final_message,
            has_task_prompt,
        });
    }

    Ok((dir, artifacts))
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;
