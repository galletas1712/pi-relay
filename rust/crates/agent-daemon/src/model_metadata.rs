use agent_vocab::ProviderKind;

/// Canonical Rust-side model metadata used for safety-critical context-window
/// decisions. Web/session metadata can override these values, but the runtime
/// must not depend on the UI supplying them.
pub(crate) fn context_window(provider: ProviderKind, model: &str) -> Option<usize> {
    match provider {
        ProviderKind::OpenAi => match model {
            "gpt-5.5" | "gpt-5.1" | "gpt-5.1-codex-max" | "gpt-5.1-codex-mini" | "gpt-5.2"
            | "gpt-5.2-codex" | "gpt-5.3-codex" => Some(272_000),
            _ => None,
        },
        ProviderKind::Claude => match model {
            "claude-opus-4-7" => Some(1_000_000),
            "claude-sonnet-4-5" => Some(200_000),
            _ => None,
        },
    }
}

pub(crate) fn default_auto_limit(provider: ProviderKind, model: &str) -> Option<usize> {
    context_window(provider, model).map(|window| window * 85 / 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_openai_models_have_defaults() {
        assert_eq!(
            context_window(ProviderKind::OpenAi, "gpt-5.1-codex-max"),
            Some(272_000)
        );
        assert_eq!(
            default_auto_limit(ProviderKind::OpenAi, "gpt-5.1-codex-max"),
            Some(231_200)
        );
    }

    #[test]
    fn known_claude_models_have_defaults() {
        assert_eq!(
            context_window(ProviderKind::Claude, "claude-opus-4-7"),
            Some(1_000_000)
        );
        assert_eq!(
            default_auto_limit(ProviderKind::Claude, "claude-sonnet-4-5"),
            Some(170_000)
        );
    }

    #[test]
    fn unknown_models_have_no_automatic_limit() {
        assert_eq!(context_window(ProviderKind::OpenAi, "mystery"), None);
        assert_eq!(default_auto_limit(ProviderKind::Claude, "mystery"), None);
    }
}
