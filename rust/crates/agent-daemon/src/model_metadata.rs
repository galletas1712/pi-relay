use agent_vocab::{ProviderKind, ReasoningEffort};

const HOSTED_GPT56_MODELS: &[&str] = &["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"];

fn is_hosted_gpt56(model: &str) -> bool {
    HOSTED_GPT56_MODELS.contains(&model)
}

/// Canonical Rust-side model metadata used for safety-critical context-window
/// decisions. Web/session metadata can override these values, but the runtime
/// must not depend on the UI supplying them.
pub(crate) fn context_window(provider: ProviderKind, model: &str) -> Option<usize> {
    match provider {
        ProviderKind::OpenAi if is_hosted_gpt56(model) => Some(372_000),
        ProviderKind::OpenAi => match model {
            "gpt-5.5" | "gpt-5.1" | "gpt-5.1-codex-max" | "gpt-5.1-codex-mini" | "gpt-5.2"
            | "gpt-5.2-codex" | "gpt-5.3-codex" => Some(272_000),
            _ => None,
        },
        ProviderKind::Claude => match model {
            "claude-sonnet-5" | "claude-fable-5" | "claude-opus-4-8" | "claude-opus-4-7" => {
                Some(1_000_000)
            }
            "claude-sonnet-4-5" => Some(200_000),
            _ => None,
        },
    }
}

pub(crate) fn default_auto_limit_for_window(
    provider: ProviderKind,
    model: &str,
    window: usize,
) -> usize {
    match provider {
        // Anthropic's current verified 1M input models should compact halfway
        // through the window. Key this to authoritative window metadata, not a
        // static id list, so newly discovered 1M Claude models get the same
        // policy while changed/non-1M models retain the generic safe default.
        ProviderKind::Claude if window == 1_000_000 => 500_000,
        // Codex derives this family-specific raw threshold as 90% of the live
        // 372k context window. Older OpenAI models retain the existing 85%.
        ProviderKind::OpenAi if is_hosted_gpt56(model) => window.saturating_mul(90) / 100,
        ProviderKind::OpenAi | ProviderKind::Claude => window.saturating_mul(85) / 100,
    }
}

// Per-model reasoning-effort support, encoded from a live provider probe on
// 2026-06-17 (see /tmp/probe-verify/effort_matrix.txt). Sending an unsupported
// value 400s, so the daemon normalizes the session's requested effort to the
// nearest supported value before building a provider request.
//
// OpenAI gpt-5.x: the provider's own 400 enumerates exactly these values for
//   gpt-5.5 ("Supported values are: 'none', 'low', 'medium', 'high', and
//   'xhigh'."). `minimal` and `max` are rejected. Only gpt-5.5 was authorized
//   on the probe account; the rest of the family is assumed to share this set.
//   Live GPT-5.6 metadata confirms Sol, Terra, and Luna add `max`.
// Claude opus-4-7/opus-4-8 (adaptive thinking): low/medium/high/xhigh/max all
//   accepted on the wire; `none`/`minimal` are refused. sonnet-4-5 is
//   non-adaptive, so the Anthropic adapter drops effort entirely; its table is
//   only used to keep normalization a no-op within the adaptive intensity band.
const OPENAI_GPT5_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::None,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
];

const OPENAI_GPT56_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::None,
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

const CLAUDE_ADAPTIVE_EFFORTS: &[ReasoningEffort] = &[
    ReasoningEffort::Low,
    ReasoningEffort::Medium,
    ReasoningEffort::High,
    ReasoningEffort::XHigh,
    ReasoningEffort::Max,
];

pub(crate) fn supported_reasoning_efforts(
    provider: ProviderKind,
    model: &str,
) -> &'static [ReasoningEffort] {
    match provider {
        ProviderKind::OpenAi if is_hosted_gpt56(model) => OPENAI_GPT56_EFFORTS,
        ProviderKind::OpenAi => match model {
            "gpt-5.5" | "gpt-5.1" | "gpt-5.1-codex-max" | "gpt-5.1-codex-mini" | "gpt-5.2"
            | "gpt-5.2-codex" | "gpt-5.3-codex" => OPENAI_GPT5_EFFORTS,
            // Unknown OpenAI model: assume the gpt-5.x family's common set.
            _ => OPENAI_GPT5_EFFORTS,
        },
        ProviderKind::Claude => match model {
            "claude-sonnet-5" | "claude-fable-5" | "claude-opus-4-8" | "claude-opus-4-7"
            | "claude-sonnet-4-5" => CLAUDE_ADAPTIVE_EFFORTS,
            // Unknown Claude model: assume the adaptive set.
            _ => CLAUDE_ADAPTIVE_EFFORTS,
        },
    }
}

// Effort intensity, used to pick the nearest supported value. Higher is more
// reasoning: none < minimal < low < medium < high < xhigh < max.
fn effort_intensity(effort: ReasoningEffort) -> u8 {
    match effort {
        ReasoningEffort::None => 0,
        ReasoningEffort::Minimal => 1,
        ReasoningEffort::Low => 2,
        ReasoningEffort::Medium => 3,
        ReasoningEffort::High => 4,
        ReasoningEffort::XHigh => 5,
        ReasoningEffort::Max => 6,
    }
}

/// Normalize a requested reasoning effort to a value the model actually
/// supports. If `requested` is already supported it is returned unchanged.
/// Otherwise the nearest supported value by intensity is returned, preferring
/// the HIGHER value on a tie (so `minimal`->`low` and `max`->`xhigh` for
/// gpt-5.x, and `xhigh`->`high` for a model that lacks xhigh).
pub(crate) fn normalize_reasoning_effort(
    provider: ProviderKind,
    model: &str,
    requested: ReasoningEffort,
) -> ReasoningEffort {
    let supported = supported_reasoning_efforts(provider, model);
    if supported.contains(&requested) {
        return requested;
    }
    let target = effort_intensity(requested);
    supported
        .iter()
        .copied()
        .min_by_key(|candidate| {
            let intensity = effort_intensity(*candidate);
            let distance = target.abs_diff(intensity);
            // Tie-break toward the higher-intensity value: a closer candidate
            // wins on distance; equal distance prefers the one above `target`.
            (distance, u8::from(intensity < target))
        })
        .unwrap_or(requested)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_openai_models_have_defaults() {
        for model in HOSTED_GPT56_MODELS {
            assert_eq!(context_window(ProviderKind::OpenAi, model), Some(372_000));
            assert_eq!(
                default_auto_limit_for_window(ProviderKind::OpenAi, model, 372_000),
                334_800
            );
        }
        assert_eq!(
            context_window(ProviderKind::OpenAi, "gpt-5.3-codex"),
            Some(272_000)
        );
        assert_eq!(
            default_auto_limit_for_window(ProviderKind::OpenAi, "gpt-5.3-codex", 272_000),
            231_200
        );
    }

    #[test]
    fn known_claude_models_have_defaults() {
        for model in ["claude-sonnet-5", "claude-fable-5", "claude-opus-4-8"] {
            assert_eq!(context_window(ProviderKind::Claude, model), Some(1_000_000));
            assert_eq!(
                default_auto_limit_for_window(ProviderKind::Claude, model, 1_000_000),
                500_000
            );
        }
    }

    #[test]
    fn discovered_window_policy_is_provider_and_model_aware() {
        assert_eq!(
            default_auto_limit_for_window(
                ProviderKind::Claude,
                "claude-newly-discovered",
                1_000_000
            ),
            500_000
        );
        assert_eq!(
            default_auto_limit_for_window(ProviderKind::Claude, "claude-newly-discovered", 500_000),
            425_000
        );
        assert_eq!(
            default_auto_limit_for_window(ProviderKind::OpenAi, "gpt-5.6-luna", 372_000),
            334_800
        );
        assert_eq!(
            default_auto_limit_for_window(ProviderKind::OpenAi, "gpt-5.3-codex", 272_000),
            231_200
        );
    }

    #[test]
    fn unknown_models_have_no_automatic_limit() {
        assert_eq!(context_window(ProviderKind::OpenAi, "mystery"), None);
        assert_eq!(context_window(ProviderKind::Claude, "mystery"), None);
    }

    #[test]
    fn gpt5_normalizes_to_probed_supported_set() {
        use ReasoningEffort::*;
        let normalize =
            |effort| normalize_reasoning_effort(ProviderKind::OpenAi, "gpt-5.5", effort);
        // Probed support: none, low, medium, high, xhigh.
        assert_eq!(normalize(None), None);
        assert_eq!(normalize(Low), Low);
        assert_eq!(normalize(Medium), Medium);
        assert_eq!(normalize(High), High);
        assert_eq!(normalize(XHigh), XHigh);
        // minimal is rejected by the provider: nearest is a tie between none and
        // low, and the tie-break prefers the higher value.
        assert_eq!(normalize(Minimal), Low);
        // max is rejected by the provider: nearest supported is xhigh.
        assert_eq!(normalize(Max), XHigh);
    }

    #[test]
    fn older_gpt5_family_shares_gpt55_support() {
        use ReasoningEffort::*;
        for model in [
            "gpt-5.1",
            "gpt-5.1-codex-max",
            "gpt-5.1-codex-mini",
            "gpt-5.2",
            "gpt-5.2-codex",
            "gpt-5.3-codex",
        ] {
            assert_eq!(
                normalize_reasoning_effort(ProviderKind::OpenAi, model, Minimal),
                Low
            );
            assert_eq!(
                normalize_reasoning_effort(ProviderKind::OpenAi, model, Max),
                XHigh
            );
        }
    }

    #[test]
    fn gpt56_family_accepts_max_reasoning() {
        use ReasoningEffort::*;
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert_eq!(
                normalize_reasoning_effort(ProviderKind::OpenAi, model, Max),
                Max
            );
            assert_eq!(
                normalize_reasoning_effort(ProviderKind::OpenAi, model, Minimal),
                Low
            );
        }
    }

    #[test]
    fn unknown_openai_model_uses_gpt5_default_set() {
        use ReasoningEffort::*;
        assert_eq!(
            normalize_reasoning_effort(ProviderKind::OpenAi, "mystery", Max),
            XHigh
        );
        assert_eq!(
            normalize_reasoning_effort(ProviderKind::OpenAi, "mystery", High),
            High
        );
    }

    #[test]
    fn claude_adaptive_normalizes_to_probed_supported_set() {
        use ReasoningEffort::*;
        for model in [
            "claude-sonnet-5",
            "claude-fable-5",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-sonnet-4-5",
        ] {
            let normalize =
                |effort| normalize_reasoning_effort(ProviderKind::Claude, model, effort);
            // Probed support: low, medium, high, xhigh, max.
            assert_eq!(normalize(Low), Low);
            assert_eq!(normalize(Medium), Medium);
            assert_eq!(normalize(High), High);
            assert_eq!(normalize(XHigh), XHigh);
            assert_eq!(normalize(Max), Max);
            // none/minimal are not supported; nearest supported is low.
            assert_eq!(normalize(None), Low);
            assert_eq!(normalize(Minimal), Low);
        }
    }

    #[test]
    fn supported_set_membership_matches_probe() {
        use ReasoningEffort::*;
        let openai = supported_reasoning_efforts(ProviderKind::OpenAi, "gpt-5.5");
        assert!(openai.contains(&None));
        assert!(!openai.contains(&Minimal));
        assert!(!openai.contains(&Max));
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert!(supported_reasoning_efforts(ProviderKind::OpenAi, model).contains(&Max));
        }
        let claude = supported_reasoning_efforts(ProviderKind::Claude, "claude-opus-4-8");
        assert!(claude.contains(&Max));
        assert!(!claude.contains(&None));
        assert!(!claude.contains(&Minimal));
    }
}
