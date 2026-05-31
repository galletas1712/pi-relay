#![forbid(unsafe_code)]

mod postgres;

use std::fmt;
use std::str::FromStr;

use agent_session::{ModelContext, SessionAction, SessionEvent, TranscriptStorageNode};
use agent_vocab::{
    ActionId, ProviderConfig, ProviderKind, ProviderReplayItem, TranscriptItem, TurnId,
    TurnOutcome, UserMessage,
};
pub use postgres::PostgresAgentStore;
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;
use uuid::Uuid;

macro_rules! text_enum {
    ($(
        $(#[$meta:meta])*
        pub enum $name:ident {
            $($variant:ident => $wire:literal),+ $(,)?
        }
    )+) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
            pub enum $name {
                $($variant),+
            }

            impl $name {
                pub fn as_str(self) -> &'static str {
                    match self {
                        $(Self::$variant => $wire),+
                    }
                }
            }

            impl fmt::Display for $name {
                fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                    f.write_str(self.as_str())
                }
            }

            impl FromStr for $name {
                type Err = String;

                fn from_str(value: &str) -> Result<Self, Self::Err> {
                    match value {
                        $($wire => Ok(Self::$variant),)+
                        other => Err(format!(
                            "unknown {}: {other}",
                            stringify!($name),
                        )),
                    }
                }
            }

            impl Serialize for $name {
                fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
                where
                    S: Serializer,
                {
                    serializer.serialize_str(self.as_str())
                }
            }

            impl<'de> Deserialize<'de> for $name {
                fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
                where
                    D: Deserializer<'de>,
                {
                    let value = String::deserialize(deserializer)?;
                    Self::from_str(&value).map_err(D::Error::custom)
                }
            }
        )+
    };
}

text_enum! {
    pub enum InputPriority {
        FollowUp => "follow_up",
        Steer => "steer",
    }

    pub enum QueuedInputStatus {
        Queued => "queued",
        Consuming => "consuming",
        Consumed => "consumed",
        Cancelled => "cancelled",
    }

    pub enum ActionKind {
        Model => "model",
        Tool => "tool",
        Compaction => "compaction",
    }

    pub enum ActionStatus {
        Pending => "pending",
        Blocked => "blocked",
        Running => "running",
        Completed => "completed",
        Error => "error",
        Interrupted => "interrupted",
        Stale => "stale",
    }

    pub enum SessionActivity {
        Idle => "idle",
        Queued => "queued",
        Running => "running",
    }

    pub enum ActiveBranchSyncStatus {
        Unchanged => "unchanged",
        Extended => "extended",
        BranchChanged => "branch_changed",
    }

    pub enum EventType {
        SessionCreated => "session.created",
        SessionConfigured => "session.configured",
        SessionRecovered => "session.recovered",
        SessionIdle => "session.idle",
        SessionWorkCancelled => "session.work_cancelled",
        InputQueued => "input.queued",
        InputPromoted => "input.promoted",
        InputUpdated => "input.updated",
        InputCancelled => "input.cancelled",
        InputReordered => "input.reordered",
        InputConsumed => "input.consumed",
        InputAccepted => "input.accepted",
        InputIgnored => "input.ignored",
        HistorySwitched => "history.switched",
        HistoryCompacted => "history.compacted",
        ActionRequested => "action.requested",
        ModelRequested => "model.requested",
        ModelCompleted => "model.completed",
        ModelError => "model.error",
        ToolRequested => "tool.requested",
        ToolStarted => "tool.started",
        ToolCompleted => "tool.completed",
        ToolError => "tool.error",
        CompactionRequested => "compaction.requested",
        CompactionCompleted => "compaction.completed",
        CompactionError => "compaction.error",
        TranscriptAppended => "transcript.appended",
        TurnStarted => "turn.started",
        TurnFinished => "turn.finished",
        AssistantMessage => "assistant.message",
    }
}

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub project_id: Option<Uuid>,
    pub outer_cwd: String,
    pub workspaces: Vec<SessionWorkspace>,
    pub system_prompt: String,
    pub provider: ProviderConfig,
    pub metadata: Value,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceKind {
    #[default]
    Git,
    Local,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProjectWorkspace {
    #[serde(default)]
    pub kind: WorkspaceKind,
    pub workspace_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
}

impl ProjectWorkspace {
    pub fn git(
        workspace_dir: impl Into<String>,
        remote_url: impl Into<String>,
        remote_branch: impl Into<String>,
    ) -> Self {
        Self {
            kind: WorkspaceKind::Git,
            workspace_dir: workspace_dir.into(),
            remote_url: Some(remote_url.into()),
            remote_branch: Some(remote_branch.into()),
            source_path: None,
        }
    }

    pub fn local(workspace_dir: impl Into<String>, source_path: impl Into<String>) -> Self {
        Self {
            kind: WorkspaceKind::Local,
            workspace_dir: workspace_dir.into(),
            remote_url: None,
            remote_branch: None,
            source_path: Some(source_path.into()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionWorkspace {
    #[serde(default)]
    pub kind: WorkspaceKind,
    pub workspace_dir: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remote_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local_branch: Option<String>,
}

impl SessionWorkspace {
    pub fn git(
        workspace_dir: impl Into<String>,
        remote_url: impl Into<String>,
        remote_branch: impl Into<String>,
        base_sha: impl Into<String>,
        local_branch: impl Into<String>,
    ) -> Self {
        Self {
            kind: WorkspaceKind::Git,
            workspace_dir: workspace_dir.into(),
            remote_url: Some(remote_url.into()),
            remote_branch: Some(remote_branch.into()),
            source_path: None,
            base_sha: Some(base_sha.into()),
            local_branch: Some(local_branch.into()),
        }
    }

    pub fn local(workspace_dir: impl Into<String>, source_path: impl Into<String>) -> Self {
        Self {
            kind: WorkspaceKind::Local,
            workspace_dir: workspace_dir.into(),
            remote_url: None,
            remote_branch: None,
            source_path: Some(source_path.into()),
            base_sha: None,
            local_branch: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Project {
    pub project_id: Uuid,
    pub name: String,
    pub workspaces: Vec<ProjectWorkspace>,
    pub metadata: Value,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventFrame {
    pub event_id: i64,
    pub event: EventType,
    pub session_id: String,
    pub data: Value,
}

#[derive(Debug, Clone)]
pub struct ActionUpdate {
    pub row_id: String,
    pub attempt_id: String,
    pub status: ActionStatus,
    pub result: Value,
}

#[derive(Debug, Clone)]
pub struct PersistedAction {
    pub row_id: String,
    pub attempt_id: String,
    pub action: SessionAction,
}

#[derive(Debug, Clone)]
pub struct PendingDispatchAction {
    pub row_id: String,
    pub attempt_id: String,
    pub action: SessionAction,
}

pub struct EnqueueUserInputResult {
    pub input_id: String,
    pub event: Option<EventFrame>,
    pub queue: Option<QueueState>,
}

pub struct PromoteQueuedInputResult {
    pub input_id: String,
    pub priority: InputPriority,
    pub status: QueuedInputStatus,
    pub promoted: bool,
    pub event: Option<EventFrame>,
    pub queue: QueueState,
}

#[derive(Debug, Clone)]
pub struct UpdateQueuedInputResult {
    pub input_id: String,
    pub updated: bool,
    pub reason: Option<String>,
    pub priority: InputPriority,
    pub status: QueuedInputStatus,
    pub event: Option<EventFrame>,
    pub queue: QueueState,
}

#[derive(Debug, Clone)]
pub struct CancelQueuedInputResult {
    pub input_id: String,
    pub cancelled: bool,
    pub reason: Option<String>,
    pub priority: InputPriority,
    pub status: QueuedInputStatus,
    pub event: Option<EventFrame>,
    pub queue: QueueState,
}

#[derive(Debug, Clone)]
pub struct ReorderQueuedFollowUpsResult {
    pub reordered: bool,
    pub reason: Option<String>,
    pub input_ids: Vec<String>,
    pub event: Option<EventFrame>,
    pub queue: QueueState,
}

#[derive(Debug, Clone)]
pub struct SessionSummary {
    pub session_id: String,
    pub project_id: Option<Uuid>,
    pub outer_cwd: String,
    pub workspaces: Vec<SessionWorkspace>,
    pub activity: SessionActivity,
    pub active_leaf_id: Option<String>,
    pub provider: ProviderConfig,
    pub metadata: Value,
    pub created_at: String,
    pub updated_at: String,
    pub has_transcript_entries: bool,
}

#[derive(Debug, Clone)]
pub struct PendingActionRecord {
    pub action_row_id: String,
    pub kind: ActionKind,
    pub status: ActionStatus,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct CompactionJob {
    pub action_row_id: String,
    pub attempt_id: String,
    pub source_session_id: String,
    pub source_leaf_id: String,
    pub model_context: ModelContext,
    pub compaction_context: ModelContext,
    pub tokens_before: Option<usize>,
    pub last_turn_id: TurnId,
    pub turn_started_at_ms: Option<u64>,
    pub trigger: CompactionTrigger,
    pub reason: Option<String>,
    pub scope: CompactionScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum CompactionScope {
    Boundary {
        source_leaf_id: String,
    },
    MidTurn {
        source_leaf_id: String,
        turn_id: TurnId,
        blocked_model_action_id: ActionId,
        blocked_model_action_row_id: String,
        blocked_model_attempt_id: String,
    },
}

impl CompactionScope {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Boundary { .. } => "boundary",
            Self::MidTurn { .. } => "mid_turn",
        }
    }

    pub fn source_leaf_id(&self) -> &str {
        match self {
            Self::Boundary { source_leaf_id } | Self::MidTurn { source_leaf_id, .. } => {
                source_leaf_id
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompactionTrigger {
    Manual,
    Auto { reason: String },
}

impl CompactionTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto { .. } => "auto",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Self::Manual => None,
            Self::Auto { reason } => Some(reason.as_str()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CompactionCompletion {
    pub summary: String,
    pub summary_kind: String,
    pub provider_replay: Vec<ProviderReplayItem>,
    pub remote: bool,
    pub provider: ProviderKind,
    pub usage: Option<Value>,
    pub continuation_suffix: Vec<TranscriptStorageNode>,
}

pub struct CreateCompactionResult {
    pub job: CompactionJob,
    pub events: Vec<EventFrame>,
}

pub struct CompleteCompactionResult {
    pub new_root_id: Option<String>,
    pub active_leaf_id: Option<String>,
    pub resumed_model_action: Option<PersistedAction>,
    pub events: Vec<EventFrame>,
}

#[derive(Debug, Clone)]
pub struct QueuedInputRecord {
    pub input_id: String,
    pub priority: InputPriority,
    pub status: QueuedInputStatus,
    pub content: UserMessage,
    pub client_input_id: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub promoted_at: Option<String>,
    pub follow_up_position: Option<i32>,
}

#[derive(Debug, Clone)]
pub struct QueueState {
    pub session_revision: i64,
    pub queue_revision: i64,
    pub transcript_revision: i64,
    pub activity: SessionActivity,
    pub queued_inputs: Vec<QueuedInputRecord>,
}

#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub session_id: String,
    pub project_id: Option<Uuid>,
    pub outer_cwd: String,
    pub workspaces: Vec<SessionWorkspace>,
    pub activity: SessionActivity,
    pub active_leaf_id: Option<String>,
    pub provider: ProviderConfig,
    pub metadata: Value,
    pub pending_actions: Vec<PendingActionRecord>,
    pub queued_inputs: Vec<QueuedInputRecord>,
    pub session_revision: i64,
    pub queue_revision: i64,
    pub transcript_revision: i64,
    pub last_event_id: i64,
    pub has_transcript_entries: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptEntryScope {
    FullTree,
    ActiveBranch,
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptEntryRecord {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp_ms: u64,
    pub sequence: i64,
    pub item: TranscriptItem,
    pub provider_replay: Vec<ProviderReplayItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TranscriptTreeNodeRecord {
    pub id: String,
    pub parent_id: Option<String>,
    pub source_leaf_id: Option<String>,
    pub timestamp_ms: u64,
    pub sequence: i64,
    pub item_type: String,
    pub turn_id: Option<TurnId>,
    pub outcome: Option<TurnOutcome>,
    pub can_switch_to: bool,
    pub edit_target_leaf_id: Option<String>,
    pub display_hint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TranscriptTreeIndex {
    pub session_id: String,
    pub active_leaf_id: Option<String>,
    pub session_revision: i64,
    pub transcript_revision: i64,
    pub after_sequence: i64,
    pub max_sequence: i64,
    pub complete: bool,
    pub nodes: Vec<TranscriptTreeNodeRecord>,
}

#[derive(Debug, Clone)]
pub struct TranscriptEntriesResult {
    pub session_id: String,
    pub session_revision: i64,
    pub transcript_revision: i64,
    pub entries: Vec<TranscriptEntryRecord>,
}

#[derive(Debug, Clone)]
pub struct HistoryTree {
    pub session_id: String,
    pub active_leaf_id: Option<String>,
    pub entries: Vec<TranscriptEntryRecord>,
}

#[derive(Debug, Clone)]
pub struct ActiveBranchSync {
    pub session_id: String,
    pub base_leaf_id: Option<String>,
    pub active_leaf_id: Option<String>,
    pub status: ActiveBranchSyncStatus,
    pub entries: Vec<TranscriptEntryRecord>,
}

#[derive(Debug, Clone)]
pub struct SwitchActiveLeafResult {
    pub session_id: String,
    pub active_leaf_id: Option<String>,
    pub activity: SessionActivity,
    pub session_revision: i64,
    pub queue_revision: i64,
    pub transcript_revision: i64,
    pub last_event_id: i64,
    pub active_branch_entries: Option<Vec<TranscriptEntryRecord>>,
    pub events: Vec<EventFrame>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueMutationError {
    input_id: String,
}

impl QueueMutationError {
    pub fn not_found(input_id: impl Into<String>) -> Self {
        Self {
            input_id: input_id.into(),
        }
    }

    pub fn input_id(&self) -> &str {
        &self.input_id
    }
}

impl fmt::Display for QueueMutationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "queued input not found: {}", self.input_id)
    }
}

impl std::error::Error for QueueMutationError {}

#[derive(Debug, Clone)]
pub struct InputRecord {
    pub input_id: String,
    pub status: QueuedInputStatus,
}

#[derive(Debug, Clone)]
pub struct AcceptedInput {
    pub priority: InputPriority,
    pub content: UserMessage,
    pub client_input_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QueuedInput {
    pub id: String,
    pub priority: InputPriority,
    pub content: UserMessage,
    pub client_input_id: Option<String>,
    pub claim_id: String,
    pub row_version: String,
}

#[derive(Debug, Clone)]
pub struct QueuedInputPreview {
    pub id: String,
    pub priority: InputPriority,
    pub content: UserMessage,
    pub client_input_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct StoredAction {
    pub kind: ActionKind,
    pub action_id: i64,
    pub turn_id: Option<i64>,
    pub attempt_id: String,
}

#[derive(Debug, Clone)]
pub struct ResumableModelAction {
    pub action_id: ActionId,
    pub turn_id: TurnId,
    pub status: ActionStatus,
    pub context_leaf_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TokenUsageEstimate {
    pub total_tokens: usize,
    pub base_tokens: usize,
    pub estimated_suffix_tokens: usize,
    pub suffix_start_leaf_id: Option<String>,
    pub suffix_entries: Vec<TranscriptStorageNode>,
}

impl TokenUsageEstimate {
    pub fn from_full_estimate(total_tokens: usize) -> Self {
        Self {
            total_tokens,
            base_tokens: 0,
            estimated_suffix_tokens: total_tokens,
            suffix_start_leaf_id: None,
            suffix_entries: Vec::new(),
        }
    }

    pub fn with_estimated_suffix_tokens(mut self, estimated_suffix_tokens: usize) -> Self {
        self.estimated_suffix_tokens = estimated_suffix_tokens;
        self.total_tokens = self.base_tokens.saturating_add(estimated_suffix_tokens);
        self
    }

    pub fn with_suffix_entries(mut self, suffix_entries: Vec<TranscriptStorageNode>) -> Self {
        self.suffix_entries = suffix_entries;
        self
    }
}

#[derive(Debug, Clone)]
pub struct ResumableToolAction {
    pub action_id: ActionId,
    pub turn_id: TurnId,
    pub status: ActionStatus,
    pub tool_call: agent_vocab::ToolCall,
}

pub struct OutputBatch<'a> {
    pub(crate) entries: &'a [TranscriptStorageNode],
    pub(crate) active_leaf_id: Option<&'a str>,
    pub(crate) session_events: &'a [SessionEvent],
    pub(crate) actions: &'a [SessionAction],
    pub(crate) action_update: Option<ActionUpdate>,
    pub(crate) consumed_input: Option<QueuedInput>,
    pub(crate) accepted_input: Option<AcceptedInput>,
}

impl<'a> OutputBatch<'a> {
    pub fn new(
        entries: &'a [TranscriptStorageNode],
        active_leaf_id: Option<&'a str>,
        session_events: &'a [SessionEvent],
        actions: &'a [SessionAction],
    ) -> Self {
        Self {
            entries,
            active_leaf_id,
            session_events,
            actions,
            action_update: None,
            consumed_input: None,
            accepted_input: None,
        }
    }

    pub fn with_action_update(mut self, action_update: Option<ActionUpdate>) -> Self {
        self.action_update = action_update;
        self
    }

    pub fn with_consumed_input(mut self, consumed_input: Option<QueuedInput>) -> Self {
        self.consumed_input = consumed_input;
        self
    }

    pub fn with_accepted_input(mut self, accepted_input: Option<AcceptedInput>) -> Self {
        self.accepted_input = accepted_input;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn input_priority_round_trips_as_wire_string() {
        assert_eq!(
            serde_json::to_value(InputPriority::FollowUp).unwrap(),
            json!("follow_up")
        );
        assert_eq!(
            serde_json::from_value::<InputPriority>(json!("steer")).unwrap(),
            InputPriority::Steer
        );
    }

    #[test]
    fn invalid_storage_vocab_is_rejected() {
        let error = serde_json::from_value::<ActionStatus>(json!("done"))
            .expect_err("invalid action status should fail");

        assert!(error.to_string().contains("unknown ActionStatus"));
    }
}
