fn normalized(model: &str) -> String {
    model.trim().to_ascii_lowercase()
}

fn is_codex_model(model: &str) -> bool {
    let model = normalized(model);
    ((model.starts_with("gpt-") || model.starts_with("chatgpt-")) && !model.starts_with("gpt-oss"))
        || model.starts_with("codex")
        || model.contains("-codex")
        || ["o1", "o3", "o4"].iter().any(|prefix| {
            model == *prefix
                || model
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with(['-', '.']))
        })
}

fn is_obviously_non_claude_model(model: &str) -> bool {
    let model = normalized(model);
    [
        "qwen",
        "gemini",
        "gpt-",
        "chatgpt-",
        "codex",
        "gpt-oss",
        "ollama/",
        "llama",
        "mistral",
        "deepseek",
        "grok",
        "glm",
        "minimax",
        "kimi",
        "moonshot",
        "openrouter/",
    ]
    .iter()
    .any(|marker| model.starts_with(marker) || model.contains(&format!("/{marker}")))
}

pub(super) fn model_belongs_to_card(card_id: &str, model: &str) -> bool {
    match card_id {
        "codex" => is_codex_model(model),
        "claude" => !is_obviously_non_claude_model(model),
        _ => true,
    }
}

pub(super) fn is_model_obviously_foreign_to_card(card_id: &str, model: &str) -> bool {
    match card_id {
        "codex" => !is_codex_model(model),
        "claude" => is_obviously_non_claude_model(model),
        _ => false,
    }
}
