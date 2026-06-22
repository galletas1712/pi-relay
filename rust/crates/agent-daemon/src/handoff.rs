//! The delegation handoff directory writer.
//!
//! On the delegation barrier, for every subagent (success or failure) the daemon
//! renders two files from the durable Postgres transcript — `final_message.md`
//! and an exhaustive, greppable `transcript.md` — under
//! `<parent.outer_cwd>/.pi-handoff/<delegation_id>/`. `inspect_delegation` is the
//! structured manifest/control-flow snapshot; the files remain transcript/detail
//! artifacts only. Everything renders from `active_branch` (Ui body mode), so it
//! survives an RO subagent's snapshot being destroyed and a crashed subagent's
//! partial tail.
//!
//! The writer is intentionally idempotent, but normal completion only publishes
//! these files after it wins the DB terminal-status CAS. That ordering prevents
//! a concurrent cancellation from receiving a normal completed handoff. Boot
//! repair may re-render the same files for already-completed delegations if the
//! daemon crashed after the status CAS but before publication.

use std::path::{Path, PathBuf};

use agent_store::{Delegation, DelegationKind, DelegationStatus, HistoryTree};
use agent_vocab::{
    AssistantMessage, ContentBlock, ToolResultStatus, TranscriptItem, TurnOutcome, UserMessage,
};

use crate::state::AppState;
use crate::types::RpcError;

const HANDOFF_DIR: &str = ".pi-handoff";

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

/// Extract a typed `suggested_next` edge label from a final message: if the
/// message's last non-empty line is `suggested_next: <value>`, return the raw
/// `<value>`. It is recorded verbatim and never validated against a workflow's
/// outcome set, so an out-of-set value is preserved (the parent branches on it
/// with judgment) rather than crashing the handoff.
pub(crate) fn extract_suggested_next(final_message: &str) -> Option<String> {
    let last_line = final_message
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())?;
    let value = last_line.strip_prefix("suggested_next:")?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

#[derive(Debug, Clone)]
pub(crate) struct SubagentArtifact {
    pub(crate) session_id: String,
    pub(crate) terminal_status: Option<&'static str>,
    pub(crate) final_message: Option<String>,
    pub(crate) suggested_next: Option<String>,
    pub(crate) final_message_path: Option<PathBuf>,
    pub(crate) transcript_path: PathBuf,
}

impl SubagentArtifact {
    pub(crate) fn final_message_rel(&self) -> Option<String> {
        self.final_message_path
            .as_ref()
            .map(|_| format!("{}/final_message.md", self.session_id))
    }

    pub(crate) fn transcript_rel(&self) -> String {
        format!("{}/transcript.md", self.session_id)
    }
}

pub(crate) fn delegation_dir(parent_outer_cwd: &str, delegation_id: &str) -> PathBuf {
    Path::new(parent_outer_cwd)
        .join(HANDOFF_DIR)
        .join(delegation_id)
}

/// Refresh per-subagent handoff artifacts from durable Postgres transcripts.
///
/// This writes each subagent's exhaustive `transcript.md` on every call. When
/// `include_final_messages` is true, it also writes `final_message.md`. Returned
/// metadata may still include final-message/suggested-next text for terminal
/// subagents in a running delegation, but normal final-message files are only
/// published for completed delegations. Running inspections pass `false`: their
/// transcript files are kept current, but no normal final-message artifact is
/// published before the completion CAS wins. Cancelled and failed delegations do
/// not publish normal per-subagent handoff artifacts; cancellation has its own
/// transcript-only artifact path.
pub(crate) async fn refresh_delegation_handoff_artifacts(
    state: &AppState,
    delegation: &Delegation,
    include_final_messages: bool,
) -> std::result::Result<(PathBuf, Vec<SubagentArtifact>), RpcError> {
    let parent_config = state
        .repo
        .load_session_config(&delegation.parent_session_id)
        .await?;
    let dir = delegation_dir(&parent_config.outer_cwd, &delegation.id);

    if matches!(
        delegation.status,
        DelegationStatus::Cancelled | DelegationStatus::Failed
    ) {
        return Ok((dir, Vec::new()));
    }

    let subagents = state.repo.list_delegation_subagents(&delegation.id).await?;

    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(anyhow::Error::from)?;

    let mut artifacts = Vec::with_capacity(subagents.len());
    for subagent in &subagents {
        let history = state.repo.active_branch(&subagent.session_id).await?;
        let is_terminal = active_branch_is_terminal(&history);
        let final_message = extract_final_message(&history);
        let transcript = render_transcript_markdown(&history);
        let status = subagent_status(subagent_outcome(&history));
        let suggested_next = extract_suggested_next(&final_message);
        let include_final_content = match delegation.status {
            DelegationStatus::Running => is_terminal,
            DelegationStatus::Done | DelegationStatus::DoneWithFailures => true,
            DelegationStatus::Cancelled | DelegationStatus::Failed => false,
        };

        let subagent_dir = dir.join(&subagent.session_id);
        tokio::fs::create_dir_all(&subagent_dir)
            .await
            .map_err(anyhow::Error::from)?;
        let final_message_path = if include_final_messages {
            let path = subagent_dir.join("final_message.md");
            tokio::fs::write(&path, final_message.as_bytes())
                .await
                .map_err(anyhow::Error::from)?;
            Some(path)
        } else {
            None
        };
        let transcript_path = subagent_dir.join("transcript.md");
        tokio::fs::write(&transcript_path, transcript.as_bytes())
            .await
            .map_err(anyhow::Error::from)?;

        artifacts.push(SubagentArtifact {
            session_id: subagent.session_id.clone(),
            terminal_status: is_terminal.then_some(status),
            final_message: include_final_content.then_some(final_message),
            suggested_next: include_final_content.then_some(suggested_next).flatten(),
            final_message_path,
            transcript_path,
        });
    }

    Ok((dir, artifacts))
}

/// Render and write the completed delegation's per-subagent handoff files.
/// This is a pure function of durable transcripts and delegation metadata and
/// is safe to replay, but it must not be used as the single-flight. The normal
/// barrier calls it only after winning the `finish_delegation` CAS; otherwise a
/// cancellation that wins the same race could be left with completed handoff
/// artifacts.
pub(crate) async fn write_delegation_handoff(
    state: &AppState,
    delegation: &Delegation,
) -> std::result::Result<PathBuf, RpcError> {
    let (dir, _) = refresh_delegation_handoff_artifacts(state, delegation, true).await?;
    Ok(dir)
}

/// The short completion steer delivered to the parent. It names the delegation,
/// the ok/failed counts, and points at the handoff directory — it never inlines
/// full transcripts.
pub(crate) fn steer_message(
    delegation: &Delegation,
    handoff_dir: &Path,
    ok: usize,
    failed: usize,
    failed_ids: &[String],
) -> String {
    let kind = match delegation.kind {
        DelegationKind::Full => "full subagent",
        DelegationKind::ReadonlyFanout => "read-only fan-out",
    };
    let label = delegation
        .label
        .as_deref()
        .map(|label| format!(" ({label})"))
        .unwrap_or_default();
    let mut message = format!(
        "Delegation {} ({kind}){label} finished: {ok} ok, {failed} failed. \
         Use inspect_delegation for the structured snapshot; transcript details \
         are in {}.",
        delegation.id,
        handoff_dir.display(),
    );
    if !failed_ids.is_empty() {
        message.push_str(&format!(" Failed: {}.", failed_ids.join(", ")));
    }
    message
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;
