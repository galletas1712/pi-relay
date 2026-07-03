mod auth_retry;
mod compaction;
mod connections;
mod context_accounting;
mod prompt;
mod provider;
mod requests;
mod session_titles;
mod sidecar;
mod skills;
mod transcript;
mod web_tools;

pub(crate) use agent_prompt::PromptProfile;
#[cfg(test)]
pub(crate) use compaction::{
    append_delegation_ledger_to_output, local_summary_request, remote_compaction_request,
    CompactionOutput, CompactionSummaryKind,
};
pub(crate) use compaction::{
    compaction_auto_explicitly_disabled, compaction_auto_state,
    compaction_config_with_model_metadata, run_compaction, CompactionAutoState,
};
pub(crate) use connections::ProviderConnectionRegistry;
pub(crate) use context_accounting::model_input_tokens_for_gate;
pub(crate) use prompt::{
    current_pi_template, effective_prompt_profile, provider_tools_for_session, render_pi_prompt,
};
pub(crate) use provider::model_metadata_for_config;
#[cfg(test)]
pub(crate) use requests::injected_provider_start_count;
pub(crate) use requests::{build_model_request, run_model};
pub(crate) use session_titles::{
    schedule_session_title_refresh_for_model_turn, SessionTitleScheduler,
};
pub(crate) use sidecar::{run_model_sidecar, sidecar_session_id, ModelSidecarRequest};
pub(crate) use skills::{load_skill_result, resolve_skill_role, skill_identifier};
pub(crate) use web_tools::{is_web_tool_name, run_web_tool};
