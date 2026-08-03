#![forbid(unsafe_code)]

#[macro_use]
mod macros;
mod daemon_observation;
mod ids;
mod image;
mod message;
mod provider;
mod transcript_item;

pub use crate::daemon_observation::DaemonToolObservation;
pub use crate::ids::{ActionId, ToolCallId, TurnId};
pub use crate::image::{
    decode_base64_bounded, encode_base64, normalize_mime_type, sniff_mime,
    validate_durable_content, validate_inline_image, ImageValidationError,
    MAX_AGGREGATE_IMAGE_BYTES, MAX_IMAGES_PER_CONTENT, MAX_IMAGE_BYTES,
};
pub use crate::message::{
    content_display_text, inline_content_display_text, AssistantItem, AssistantMessage,
    ContentBlock, ImageArtifactId, InlineContentBlock, InlineToolResultMessage, ToolCall,
    ToolDefinition, ToolResultMessage, ToolResultStatus, UserMessage,
};
pub use crate::provider::{
    ProviderConfig, ProviderKind, ProviderReplayItem, ReasoningEffort, ReplayDisplay,
    ReplayDisplayKind,
};
pub use crate::transcript_item::{CompactionSummary, TranscriptItem, TurnOutcome};
