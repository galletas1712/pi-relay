//! The stage handoff directory writer.
//!
//! On the stage barrier, for every subagent (success or failure) the daemon
//! renders two files from the durable Postgres transcript — `final_message.md`
//! and an exhaustive, greppable `transcript.md` — plus a per-stage `index.json`
//! manifest, all under `<parent.outer_cwd>/.pi-handoff/<stage_id>/`. Everything
//! renders from `active_branch` (Ui body mode), so it survives an RO subagent's
//! snapshot being destroyed and a crashed subagent's partial tail.
//!
//! The writer is intentionally idempotent: stage completion may render/rewrite
//! the same durable transcript files before/around the DB terminal CAS. The CAS
//! single-flights the stage status and parent steer enqueue; file rendering
//! itself is safe to replay.

use std::path::{Path, PathBuf};

use agent_store::{HistoryTree, Stage, StageKind, StageStatus};
use agent_vocab::{
    AssistantMessage, ContentBlock, ToolResultStatus, TranscriptItem, TurnOutcome, UserMessage,
};
use serde_json::{json, Value};

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

struct SubagentHandoff {
    session_id: String,
    role: Option<String>,
    status: &'static str,
    suggested_next: Option<String>,
}

fn stage_dir(parent_outer_cwd: &str, stage_id: &str) -> PathBuf {
    Path::new(parent_outer_cwd).join(HANDOFF_DIR).join(stage_id)
}

/// Render and write the whole handoff directory for a completed stage. This is
/// a pure function of durable transcripts and stage metadata, so the barrier may
/// run it before/around the `finish_stage` CAS and safely replay it. The CAS,
/// not this writer, single-flights the terminal status and parent steer enqueue.
/// `stage_status` is the terminal status the caller is attempting to commit
/// (`done` vs `done_with_failures`).
pub(crate) async fn write_stage_handoff(
    state: &AppState,
    stage: &Stage,
    stage_status: StageStatus,
) -> std::result::Result<PathBuf, RpcError> {
    let parent_config = state
        .repo
        .load_session_config(&stage.parent_session_id)
        .await?;
    let dir = stage_dir(&parent_config.outer_cwd, &stage.id);

    let subagents = state.repo.list_stage_subagents(&stage.id).await?;

    let mut manifest = Vec::with_capacity(subagents.len());
    for subagent in &subagents {
        let history = state.repo.active_branch(&subagent.session_id).await?;
        let final_message = extract_final_message(&history);
        let transcript = render_transcript_markdown(&history);
        let status = subagent_status(subagent_outcome(&history));
        let suggested_next = extract_suggested_next(&final_message);

        let subagent_dir = dir.join(&subagent.session_id);
        tokio::fs::create_dir_all(&subagent_dir)
            .await
            .map_err(anyhow::Error::from)?;
        tokio::fs::write(
            subagent_dir.join("final_message.md"),
            final_message.as_bytes(),
        )
        .await
        .map_err(anyhow::Error::from)?;
        tokio::fs::write(subagent_dir.join("transcript.md"), transcript.as_bytes())
            .await
            .map_err(anyhow::Error::from)?;

        manifest.push(SubagentHandoff {
            session_id: subagent.session_id.clone(),
            role: subagent.role.clone(),
            status,
            suggested_next,
        });
    }

    let index = index_json(stage, stage_status, &manifest);
    tokio::fs::write(
        dir.join("index.json"),
        serde_json::to_vec_pretty(&index).map_err(anyhow::Error::from)?,
    )
    .await
    .map_err(anyhow::Error::from)?;

    Ok(dir)
}

/// The per-stage `index.json` manifest: the parent's entry point. Paths are
/// relative to the stage dir, so the parent navigates without scanning.
fn index_json(stage: &Stage, stage_status: StageStatus, subagents: &[SubagentHandoff]) -> Value {
    let subagents = subagents
        .iter()
        .map(|subagent| {
            json!({
                "id": subagent.session_id,
                "role": subagent.role,
                "status": subagent.status,
                "suggested_next": subagent.suggested_next,
                "final_message": format!("{}/final_message.md", subagent.session_id),
                "transcript": format!("{}/transcript.md", subagent.session_id),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "stage_id": stage.id,
        "kind": stage.kind.as_str(),
        "workflow": stage.workflow,
        "status": stage_status.as_str(),
        "subagents": subagents,
    })
}

/// The short completion steer delivered to the parent. It names the stage, the
/// ok/failed counts, and points at `index.json` — it never inlines messages.
pub(crate) fn steer_message(
    stage: &Stage,
    handoff_dir: &Path,
    ok: usize,
    failed: usize,
    failed_ids: &[String],
) -> String {
    let kind = match stage.kind {
        StageKind::Full => "full subagent",
        StageKind::ReadonlyFanout => "read-only fan-out",
    };
    let label = stage
        .label
        .as_deref()
        .map(|label| format!(" ({label})"))
        .unwrap_or_default();
    let index = handoff_dir.join("index.json");
    let mut message = format!(
        "Stage {} ({kind}){label} finished: {ok} ok, {failed} failed. \
         Read {}, then the per-subagent final_message.md files.",
        stage.id,
        index.display(),
    );
    if !failed_ids.is_empty() {
        message.push_str(&format!(" Failed: {}.", failed_ids.join(", ")));
    }
    message
}

#[cfg(test)]
#[path = "handoff_tests.rs"]
mod tests;
