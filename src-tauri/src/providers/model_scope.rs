use crate::pricing::is_local_runtime_model;

fn normalized(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

fn model_slug(normalized_model: &str) -> Option<&str> {
    if normalized_model.is_empty() || is_local_runtime_model(normalized_model) {
        return None;
    }
    let slug = normalized_model
        .rsplit('/')
        .next()
        .unwrap_or(normalized_model)
        .trim();
    (!slug.is_empty()).then_some(slug)
}

fn has_prefix_or_exact(slug: &str, prefix: &str) -> bool {
    slug == prefix
        || slug
            .strip_prefix(prefix)
            .is_some_and(|suffix| suffix.starts_with(['-', '.', '_', ':', ' ']))
}

fn is_claude_slug(slug: &str) -> bool {
    has_prefix_or_exact(slug, "claude")
}

fn is_codex_slug(slug: &str) -> bool {
    ((slug.starts_with("gpt-") || slug.starts_with("chatgpt-")) && !slug.starts_with("gpt-oss"))
        || slug.starts_with("codex")
        || slug.contains("-codex")
        || ["o1", "o3", "o4"].iter().any(|prefix| {
            slug == *prefix
                || slug
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with(['-', '.']))
        })
}

fn is_antigravity_slug(slug: &str) -> bool {
    has_prefix_or_exact(slug, "gemini")
}

fn is_gpt_oss_slug(slug: &str) -> bool {
    has_prefix_or_exact(slug, "gpt-oss")
}

fn is_grok_slug(slug: &str) -> bool {
    has_prefix_or_exact(slug, "grok")
}

fn is_kimi_slug(slug: &str) -> bool {
    has_prefix_or_exact(slug, "kimi") || has_prefix_or_exact(slug, "moonshot")
}

fn is_kimi_namespaced_short_model(normalized_model: &str, slug: &str) -> bool {
    let bytes = slug.as_bytes();
    let is_short_series = bytes.len() >= 2 && bytes[0] == b'k' && bytes[1].is_ascii_digit();
    is_short_series
        && [
            "kimi/",
            "kimi-code/",
            "kimi-coding/",
            "moonshot/",
            "moonshot-cn/",
            "moonshotai/",
        ]
        .iter()
        .any(|prefix| normalized_model.starts_with(prefix))
}

fn is_minimax_slug(slug: &str) -> bool {
    has_prefix_or_exact(slug, "minimax") || slug.starts_with("abab")
}

fn is_zai_slug(slug: &str) -> bool {
    has_prefix_or_exact(slug, "glm") || slug.starts_with("chatglm") || slug.starts_with("codegeex")
}

pub(super) fn is_local_runtime_provider(provider_id: &str) -> bool {
    provider_id.trim().eq_ignore_ascii_case("ollama")
}

pub(super) fn is_kimi_provider_alias(provider_id: &str) -> bool {
    matches!(
        provider_id.trim().to_ascii_lowercase().as_str(),
        "kimi" | "kimi-code" | "kimi-coding" | "moonshot" | "moonshot-cn" | "moonshotai"
    )
}

/// Expands bare Kimi short-series slugs (`k2p5`, `k3`, `k3.1-pro`) when paired with a Kimi provider.
pub(super) fn normalize_provider_model(provider_id: &str, model: &str) -> String {
    let trimmed = model.trim();
    if trimmed.is_empty() || trimmed.contains('/') || !is_kimi_provider_alias(provider_id) {
        return trimmed.to_owned();
    }
    let lower = trimmed.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    if bytes.len() >= 2 && bytes[0] == b'k' && bytes[1].is_ascii_digit() {
        format!("kimi-{lower}")
    } else {
        trimmed.to_owned()
    }
}

/// Dedicated model-family card that owns `model`, based solely on the model name.
///
/// Local runtimes (`ollama/*`) and model families without a dedicated OpenQuota card
/// (`deepseek`, `qwen`, `llama`, `mistral`, `gpt-oss`, ...) return `None`.
pub(super) fn card_for_model(model: &str) -> Option<&'static str> {
    let normalized = normalized(model);
    let slug = model_slug(&normalized)?;
    if is_claude_slug(slug) {
        Some("claude")
    } else if is_codex_slug(slug) {
        Some("codex")
    } else if is_antigravity_slug(slug) {
        Some("antigravity")
    } else if is_grok_slug(slug) {
        Some("grok")
    } else if is_kimi_slug(slug) || is_kimi_namespaced_short_model(&normalized, slug) {
        Some("kimi")
    } else if is_minimax_slug(slug) {
        Some("minimax")
    } else if is_zai_slug(slug) {
        Some("zai")
    } else {
        None
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LocalModelDisposition {
    /// Belongs on the home card itself.
    Home,
    /// Belongs on another dedicated model-family card; do not add to the home card or Others.
    RoutedTo(&'static str),
    /// Foreign model without a dedicated card; keep in the home card's `other_usage` (Others).
    Other,
}

/// Classifies a model seen in `home_card`'s native CLI logs.
pub(super) fn native_log_disposition(home_card: &str, model: &str) -> LocalModelDisposition {
    if let Some(owner) = card_for_model(model) {
        return if owner == home_card {
            LocalModelDisposition::Home
        } else {
            LocalModelDisposition::RoutedTo(owner)
        };
    }
    if model_belongs_to_card(home_card, model) {
        LocalModelDisposition::Home
    } else {
        LocalModelDisposition::Other
    }
}

pub(super) fn model_belongs_to_card(card_id: &str, model: &str) -> bool {
    match card_id {
        "claude" | "codex" | "grok" | "kimi" | "minimax" | "zai" => {
            card_for_model(model) == Some(card_id)
        }
        "antigravity" => {
            card_for_model(model) == Some("antigravity")
                || model_slug(&normalized(model)).is_some_and(is_gpt_oss_slug)
        }
        _ => true,
    }
}

#[cfg(test)]
pub(super) fn is_model_obviously_foreign_to_card(card_id: &str, model: &str) -> bool {
    match card_id {
        "claude" | "codex" | "grok" | "kimi" | "minimax" | "zai" | "antigravity" => {
            !model_belongs_to_card(card_id, model)
        }
        _ => false,
    }
}

/// Formats a dynamic local usage source note naming `base` followed by contributing clients.
pub(super) fn format_source_note(base: &str, extras: &[&str]) -> String {
    let suffix = match extras {
        [] => String::new(),
        [source] => format!(" and {source}"),
        [leading @ .., last] => format!(", {}, and {last}", leading.join(", ")),
    };
    format!("From your {base}{suffix} (estimated)")
}

#[cfg(test)]
mod tests {
    use super::{
        card_for_model, format_source_note, is_model_obviously_foreign_to_card,
        model_belongs_to_card, native_log_disposition, LocalModelDisposition,
    };

    #[test]
    fn classifies_all_first_party_model_families_including_provider_prefixes() {
        for (model, expected) in [
            ("claude-sonnet-4-6", Some("claude")),
            ("anthropic/claude-opus-4-7", Some("claude")),
            ("openrouter/anthropic/claude-sonnet-4-5", Some("claude")),
            ("gpt-5.4", Some("codex")),
            ("GPT-5.5", Some("codex")),
            ("openai/gpt-5.3-codex", Some("codex")),
            ("codex-auto-review", Some("codex")),
            ("o3", Some("codex")),
            ("o4-mini", Some("codex")),
            ("grok-code-fast-1", Some("grok")),
            ("xai/grok-4", Some("grok")),
            ("kimi-k2.5", Some("kimi")),
            ("kimi-code/k3", Some("kimi")),
            ("kimi-code/k2.7-code", Some("kimi")),
            ("moonshot/kimi-k3.1-pro", Some("kimi")),
            ("moonshot-v1-8k", Some("kimi")),
            ("MiniMax-M2.7", Some("minimax")),
            ("minimax/minimax-m2.7-highspeed", Some("minimax")),
            ("abab6.5s-chat", Some("minimax")),
            ("GLM-5.3-Flash", Some("zai")),
            ("zhipu/glm-4.7", Some("zai")),
            ("chatglm-4", Some("zai")),
            ("deepseek-v4-flash", None),
            ("qwen3.8:27b - mlx", None),
            ("gemini-3-flash", Some("antigravity")),
            ("google/gemini-4-neon-high", Some("antigravity")),
            ("gemma-3-27b", None),
            ("gpt-oss-20b", None),
            ("ollama/glm4", None),
            ("ollama/gemini", None),
            ("ollama/qwen3", None),
            ("openrouter/auto", None),
        ] {
            assert_eq!(card_for_model(model), expected, "{model}");
        }
    }

    #[test]
    fn native_log_disposition_routes_owned_models_and_isolates_others() {
        assert_eq!(
            native_log_disposition("claude", "claude-sonnet-4-6"),
            LocalModelDisposition::Home
        );
        assert_eq!(
            native_log_disposition("claude", "claude-future-unpriced"),
            LocalModelDisposition::Home
        );
        assert_eq!(
            native_log_disposition("claude", "future-unpriced-model"),
            LocalModelDisposition::Other
        );
        assert_eq!(
            native_log_disposition("claude", "gemini-3-flash"),
            LocalModelDisposition::RoutedTo("antigravity")
        );
        assert_eq!(
            native_log_disposition("claude", "glm-5.3-flash"),
            LocalModelDisposition::RoutedTo("zai")
        );
        assert_eq!(
            native_log_disposition("claude", "kimi-k3"),
            LocalModelDisposition::RoutedTo("kimi")
        );
        assert_eq!(
            native_log_disposition("claude", "MiniMax-M2.7"),
            LocalModelDisposition::RoutedTo("minimax")
        );
        assert_eq!(
            native_log_disposition("claude", "gpt-5.4"),
            LocalModelDisposition::RoutedTo("codex")
        );
        assert_eq!(
            native_log_disposition("claude", "qwen3.8:27b - mlx"),
            LocalModelDisposition::Other
        );
        assert_eq!(
            native_log_disposition("codex", "deepseek-v4-flash"),
            LocalModelDisposition::Other
        );
        assert_eq!(
            native_log_disposition("codex", "claude-sonnet-4-6"),
            LocalModelDisposition::RoutedTo("claude")
        );
        assert_eq!(
            native_log_disposition("codex", "gemini-3-flash"),
            LocalModelDisposition::RoutedTo("antigravity")
        );
        assert_eq!(
            native_log_disposition("antigravity", "gemini-4-neon-high"),
            LocalModelDisposition::Home
        );
        assert_eq!(
            native_log_disposition("antigravity", "gpt-oss-120b"),
            LocalModelDisposition::Home
        );
        assert_eq!(
            native_log_disposition("antigravity", "claude-sonnet-4-6"),
            LocalModelDisposition::RoutedTo("claude")
        );
        assert_eq!(
            native_log_disposition("antigravity", "deepseek-v4"),
            LocalModelDisposition::Other
        );
        assert_eq!(
            native_log_disposition("grok", "grok-unknown-model"),
            LocalModelDisposition::Home
        );
        assert_eq!(
            native_log_disposition("grok", "future-unpriced-model"),
            LocalModelDisposition::Other
        );
        assert_eq!(
            native_log_disposition("grok", "deepseek-v4"),
            LocalModelDisposition::Other
        );
    }

    #[test]
    fn card_membership_helpers_stay_consistent() {
        assert!(model_belongs_to_card("codex", "gpt-5.4"));
        assert!(!model_belongs_to_card("codex", "gpt-oss-20b"));
        assert!(model_belongs_to_card("antigravity", "gemini-3-pro"));
        assert!(model_belongs_to_card("antigravity", "gpt-oss-120b"));
        assert!(!model_belongs_to_card("antigravity", "claude-sonnet-4-6"));
        assert!(!model_belongs_to_card("antigravity", "deepseek-v4"));
        assert!(model_belongs_to_card("claude", "claude-sonnet-4-6"));
        assert!(model_belongs_to_card("claude", "claude-future-unpriced"));
        assert!(!model_belongs_to_card("claude", "future-unpriced-model"));
        assert!(!model_belongs_to_card("claude", "glm-5"));
        assert!(is_model_obviously_foreign_to_card("claude", "glm-5"));
        assert!(is_model_obviously_foreign_to_card(
            "claude",
            "future-unpriced-model"
        ));
        assert!(is_model_obviously_foreign_to_card(
            "zai",
            "claude-sonnet-4-6"
        ));
        assert!(is_model_obviously_foreign_to_card("zai", "deepseek-v4"));
    }

    #[test]
    fn source_note_formats_zero_one_and_multiple_extras() {
        assert_eq!(
            format_source_note("Codex logs", &[]),
            "From your Codex logs (estimated)"
        );
        assert_eq!(
            format_source_note("Codex logs", &["OpenCode"]),
            "From your Codex logs and OpenCode (estimated)"
        );
        assert_eq!(
            format_source_note("Codex logs", &["pi", "OpenCode", "ZCode"]),
            "From your Codex logs, pi, OpenCode, and ZCode (estimated)"
        );
    }
}
