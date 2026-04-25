use agent_core::TranscriptRecord;

use crate::action::OneShotModelRequestId;
use crate::context::summary::estimate_records_tokens;
use crate::context::{CompactionPlan, CompactionSettings, Context};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoCompactionSettings {
    pub max_context_tokens: usize,
    pub keep_recent_tokens: usize,
}

impl AutoCompactionSettings {
    pub fn new(max_context_tokens: usize, keep_recent_tokens: usize) -> Self {
        Self {
            max_context_tokens,
            keep_recent_tokens,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OneShotModelRequest {
    pub instructions: String,
    pub input: Vec<ModelContentBlock>,
    pub output: OneShotModelOutputSpec,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelContentBlock {
    Text { text: String },
    Image { image: ImageInput },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageInput {
    Url(String),
    Base64 { media_type: String, data: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OneShotModelOutputSpec {
    Text,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OneShotModelOutput {
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingOneShot {
    pub(crate) request_id: OneShotModelRequestId,
    pub(crate) kind: PendingOneShotKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PendingOneShotKind {
    Compaction {
        plan: CompactionPlan,
        held_model_action: agent_core::AgentAction,
    },
}

pub(crate) fn prepare_auto_compaction(
    context: &Context,
    settings: AutoCompactionSettings,
) -> Option<CompactionPlan> {
    let tokens = estimate_records_tokens(context.transcript().records());
    if tokens <= settings.max_context_tokens {
        return None;
    }
    context.prepare_compaction(CompactionSettings {
        keep_recent_tokens: settings.keep_recent_tokens,
    })
}

pub(crate) fn compaction_request(plan: &CompactionPlan) -> OneShotModelRequest {
    let mut input = Vec::new();
    if let Some(previous_summary) = &plan.previous_summary {
        input.push(ModelContentBlock::Text {
            text: format!("Previous summary:\n{previous_summary}"),
        });
    }
    input.push(ModelContentBlock::Text {
        text: format_records("Context to summarize", &plan.records_to_summarize),
    });
    input.push(ModelContentBlock::Text {
        text: format_records("Recent context that will be kept", &plan.records_to_keep),
    });

    OneShotModelRequest {
        instructions: concat!(
            "Summarize the context that will be replaced. Preserve durable facts, ",
            "decisions, constraints, open tasks, tool results, and user intent. ",
            "Do not summarize the recent context except as needed to connect the ",
            "older context to it."
        )
        .to_string(),
        input,
        output: OneShotModelOutputSpec::Text,
    }
}

fn format_records(label: &str, records: &[TranscriptRecord]) -> String {
    let mut out = String::new();
    out.push_str(label);
    out.push_str(":\n");
    for record in records {
        out.push_str(&format!("{record:?}\n"));
    }
    out
}
