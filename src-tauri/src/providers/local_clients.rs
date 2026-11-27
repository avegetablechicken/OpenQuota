use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::{
    models::UsageHistory,
    pricing::PricingStore,
    providers::{
        antigravity,
        card_usage::DailyUsageAccumulator,
        claude, codex, gemini_cli_usage, grok, kimi,
        log_usage::scan_or_cached_usage,
        opencode, pi_usage,
        zcode_usage::{ZCodeProvider, ZCodeUsageScanner},
        CacheIdentity,
    },
    storage::Storage,
};

pub(crate) const DEFAULT_SOURCE_NOTE: &str = "From your local usage history (estimated)";

/// Collects local-device usage from all supported local coding clients
/// (pi/oh-my-pi, Antigravity, Claude Code, Codex CLI, Gemini CLI, Grok CLI,
/// Kimi Code, OpenCode, and ZCode) for model-family providers.
pub(crate) struct RoutedLocalUsage {
    storage: Arc<Storage>,
    pricing: Arc<PricingStore>,
    zcode: ZCodeUsageScanner,
}

impl RoutedLocalUsage {
    pub(crate) fn new(storage: Arc<Storage>, pricing: Arc<PricingStore>) -> Self {
        Self {
            storage,
            pricing,
            zcode: ZCodeUsageScanner::new(),
        }
    }

    pub(crate) fn history(
        &self,
        provider_id: &str,
        provider_name: &str,
        zcode_target: ZCodeProvider,
        now: DateTime<Utc>,
        warnings: &mut Vec<String>,
    ) -> UsageHistory {
        let pricing = self.pricing.current();
        scan_or_cached_usage(
            &self.storage,
            provider_id,
            CacheIdentity::Unscoped,
            provider_name,
            || -> Result<UsageHistory, &'static str> {
                let mut accumulator = DailyUsageAccumulator::default();
                let mut sources = Vec::new();
                let mut had_error = false;

                match pi_usage::scan_into(
                    &self.storage,
                    now,
                    &pricing,
                    provider_id,
                    &mut accumulator,
                ) {
                    Ok(pi_sources) => {
                        if pi_sources.pi {
                            sources.push("pi");
                        }
                        if pi_sources.oh_my_pi {
                            sources.push("oh-my-pi");
                        }
                    }
                    Err(_) => {
                        had_error = true;
                        crate::app_warn!(
                            "plugin:pi",
                            "pi/oh-my-pi usage history could not be folded into {provider_name}"
                        );
                    }
                }
                match antigravity::local_usage::scan_routed_usage_into(
                    &self.storage,
                    now,
                    &pricing,
                    provider_id,
                    &mut accumulator,
                ) {
                    Ok(true) => sources.push("Antigravity"),
                    Ok(false) => {}
                    Err(_) => {
                        had_error = true;
                        crate::app_warn!(
                            "plugin:antigravity",
                            "Antigravity usage history could not be folded into {provider_name}"
                        );
                    }
                }
                match claude::local_usage::scan_routed_usage_into(
                    &self.storage,
                    now,
                    &pricing,
                    provider_id,
                    &mut accumulator,
                ) {
                    Ok(true) => sources.push("Claude Code"),
                    Ok(false) => {}
                    Err(_) => {
                        had_error = true;
                        crate::app_warn!(
                            "plugin:claude",
                            "Claude Code usage history could not be folded into {provider_name}"
                        );
                    }
                }
                match codex::local_usage::scan_routed_usage_into(
                    &self.storage,
                    now,
                    &pricing,
                    provider_id,
                    &mut accumulator,
                ) {
                    Ok(true) => sources.push("Codex"),
                    Ok(false) => {}
                    Err(_) => {
                        had_error = true;
                        crate::app_warn!(
                            "plugin:codex",
                            "Codex CLI usage history could not be folded into {provider_name}"
                        );
                    }
                }
                match grok::local_usage::scan_routed_usage_into(
                    &self.storage,
                    now,
                    &pricing,
                    provider_id,
                    &mut accumulator,
                ) {
                    Ok(true) => sources.push("Grok"),
                    Ok(false) => {}
                    Err(_) => {
                        had_error = true;
                        crate::app_warn!(
                            "plugin:grok",
                            "Grok CLI usage history could not be folded into {provider_name}"
                        );
                    }
                }
                match kimi::local_usage::scan_into(
                    &self.storage,
                    now,
                    &pricing,
                    provider_id,
                    &mut accumulator,
                ) {
                    Ok(true) => sources.push("Kimi Code"),
                    Ok(false) => {}
                    Err(_) => {
                        had_error = true;
                        crate::app_warn!(
                            "plugin:kimi",
                            "Kimi Code usage history could not be folded into {provider_name}"
                        );
                    }
                }
                match gemini_cli_usage::scan_into(
                    &self.storage,
                    now,
                    &pricing,
                    provider_id,
                    &mut accumulator,
                ) {
                    Ok(true) => sources.push("Gemini CLI"),
                    Ok(false) => {}
                    Err(_) => {
                        had_error = true;
                        crate::app_warn!(
                            "plugin:gemini-cli",
                            "Gemini CLI usage history could not be folded into {provider_name}"
                        );
                    }
                }
                match opencode::scan_routed_usage_into(now, &pricing, provider_id, &mut accumulator)
                {
                    Ok(true) => sources.push("OpenCode"),
                    Ok(false) => {}
                    Err(_) => {
                        had_error = true;
                        crate::app_warn!(
                            "plugin:opencode",
                            "OpenCode usage history could not be folded into {provider_name}"
                        );
                    }
                }
                match self
                    .zcode
                    .scan_into(now, &pricing, zcode_target, &mut accumulator)
                {
                    Ok(true) => sources.push("ZCode"),
                    Ok(false) => {}
                    Err(_) => {
                        had_error = true;
                        crate::app_warn!(
                            "plugin:zcode",
                            "ZCode usage history could not be folded into {provider_name}"
                        );
                    }
                }

                if sources.is_empty() && had_error {
                    return Err("local usage sources unavailable");
                }
                let source_note = format_clients_source_note(&sources);
                Ok(accumulator.build(now, &source_note))
            },
            warnings,
        )
    }
}

fn format_clients_source_note(sources: &[&str]) -> String {
    match sources {
        [] => DEFAULT_SOURCE_NOTE.into(),
        [only] => format!("From your {only} history (estimated)"),
        [first, second] => format!("From your {first} and {second} history (estimated)"),
        [leading @ .., last] => {
            format!(
                "From your {}, and {last} history (estimated)",
                leading.join(", ")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{format_clients_source_note, DEFAULT_SOURCE_NOTE};

    #[test]
    fn formats_contributing_client_source_notes() {
        assert_eq!(format_clients_source_note(&[]), DEFAULT_SOURCE_NOTE);
        assert_eq!(
            format_clients_source_note(&["ZCode"]),
            "From your ZCode history (estimated)"
        );
        assert_eq!(
            format_clients_source_note(&["Claude Code", "OpenCode"]),
            "From your Claude Code and OpenCode history (estimated)"
        );
        assert_eq!(
            format_clients_source_note(&["Claude Code", "OpenCode", "ZCode"]),
            "From your Claude Code, OpenCode, and ZCode history (estimated)"
        );
    }
}
