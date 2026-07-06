use std::sync::Arc;

use agent_session::AgentSession;
use agent_store::SessionConfig;
use agent_vocab::{ProviderConfig, ProviderKind, ReasoningEffort};
use serde_json::json;

use super::RuntimeSession;

fn config(system_prompt: &str, title: &str) -> SessionConfig {
    SessionConfig {
        project_id: None,
        outer_cwd: "/tmp".to_string(),
        workspaces: Vec::new(),
        system_prompt: system_prompt.to_string(),
        provider: ProviderConfig {
            kind: ProviderKind::Claude,
            model: "claude-opus-4-8".to_string(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::High,
            prompt_cache: None,
        },
        metadata: json!({ "title": title }),
    }
}

#[test]
fn config_replacement_atomically_replaces_prompt_and_preserves_in_flight_snapshot() {
    let mut runtime = RuntimeSession::new(AgentSession::new(), config("old prompt", "old"), None);
    let routine_clone = runtime.config.clone();
    assert!(Arc::ptr_eq(&routine_clone.config, &runtime.config.config));
    assert!(Arc::ptr_eq(routine_clone.prompt(), runtime.config.prompt()));
    let in_flight_prompt = Arc::clone(runtime.config.prompt());

    runtime.replace_config(config("new prompt", "new"));

    assert!(!Arc::ptr_eq(&in_flight_prompt, runtime.config.prompt()));
    assert_eq!(
        in_flight_prompt.stable_prefix.as_deref(),
        Some("old prompt")
    );
    assert_eq!(
        (
            runtime.config.system_prompt.as_str(),
            runtime.config.prompt().stable_prefix.as_deref(),
            runtime.config.metadata["title"].as_str(),
        ),
        ("new prompt", Some("new prompt"), Some("new"))
    );
}
