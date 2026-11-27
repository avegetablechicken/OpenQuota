//! Fold-in: how a Card collects the usage every other Home client places on it.
//!
//! Each Home client is one `HomeClient` in `CLIENTS`. A Card with its own logs reads them itself
//! and hands that scan to `card_history`; a Card without them uses `folded_history`. Both fold
//! in every other client in one order, decide what a failing client means for the Card, and name
//! the contributing clients in the source note. Terms are defined in `GLOSSARY.md`.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::{
    models::UsageHistory,
    pricing::{ModelPricing, PricingStore},
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

/// What every Home client needs to fold its usage into a Card.
pub(crate) struct FoldContext<'a> {
    pub storage: &'a Storage,
    pub now: DateTime<Utc>,
    pub pricing: &'a ModelPricing,
}

/// The Card being filled: its ID and the name log messages use for it.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Card<'a> {
    pub id: &'a str,
    pub name: &'a str,
}

pub(crate) type FoldError = Box<dyn std::error::Error + Send + Sync>;

/// One Home client whose logs can place usage on any Card.
pub(crate) trait HomeClient: Sync {
    /// How log messages name the client.
    fn name(&self) -> &'static str;
    /// Log target for the client's warnings.
    fn log_target(&self) -> &'static str;
    /// The Card whose own adapter reads this client's logs; it never folds this client in.
    fn native_card(&self) -> Option<&'static str> {
        None
    }
    /// Adds the client's usage for `card_id` to `accumulator`, returning the source labels that
    /// contributed, in source-note order.
    fn fold_into(
        &self,
        context: &FoldContext<'_>,
        card_id: &str,
        accumulator: &mut DailyUsageAccumulator,
    ) -> Result<Vec<&'static str>, FoldError>;
}

impl std::fmt::Debug for dyn HomeClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.name())
    }
}

/// Every Home client, in Fold-in order.
pub(crate) static CLIENTS: [&dyn HomeClient; 9] = [
    &Pi,
    &Antigravity,
    &ClaudeCode,
    &CodexCli,
    &GrokCli,
    &KimiCode,
    &GeminiCli,
    &OpenCode,
    &ZCode,
];

/// Neither the Card's own logs nor any Home client contributed, and some client could not be
/// read, so the Card has no fresh history.
#[derive(Debug, thiserror::Error)]
#[error("local usage sources unavailable")]
pub(crate) struct FoldInUnavailable;

/// The history of a Card that reads its own logs: `scan_native` adds them and says whether any
/// landed, named `native_source` in the source note, then every other client is folded in. A
/// failing native scan fails the Card before anything is folded in.
pub(crate) fn card_history<E: From<FoldInUnavailable>>(
    clients: &[&dyn HomeClient],
    context: &FoldContext<'_>,
    card: Card<'_>,
    native_source: &str,
    scan_native: impl FnOnce(&mut DailyUsageAccumulator) -> Result<bool, E>,
) -> Result<UsageHistory, E> {
    let mut accumulator = DailyUsageAccumulator::default();
    let native_contributed = scan_native(&mut accumulator)?;
    Ok(history(
        clients,
        context,
        card,
        Some(native_source),
        native_contributed,
        accumulator,
    )?)
}

/// The history of a Card without logs of its own, from every Home client.
pub(crate) fn folded_history(
    clients: &[&dyn HomeClient],
    context: &FoldContext<'_>,
    card: Card<'_>,
) -> Result<UsageHistory, FoldInUnavailable> {
    let accumulator = DailyUsageAccumulator::default();
    history(clients, context, card, None, false, accumulator)
}

/// Folds every other client into `accumulator`. The Card fails when nothing contributed, its own
/// logs included, and at least one client could not be read: its cached history is then shown
/// rather than an empty one. Otherwise a failing client is only logged.
fn history(
    clients: &[&dyn HomeClient],
    context: &FoldContext<'_>,
    card: Card<'_>,
    native_source: Option<&str>,
    native_contributed: bool,
    mut accumulator: DailyUsageAccumulator,
) -> Result<UsageHistory, FoldInUnavailable> {
    let folded = fold(clients, context, card, &mut accumulator);
    if !native_contributed && folded.sources.is_empty() && folded.failed {
        return Err(FoldInUnavailable);
    }
    let source_note = source_note(native_source, &folded.sources);
    Ok(accumulator.build(context.now, &source_note))
}

struct Folded {
    sources: Vec<&'static str>,
    failed: bool,
}

fn fold(
    clients: &[&dyn HomeClient],
    context: &FoldContext<'_>,
    card: Card<'_>,
    accumulator: &mut DailyUsageAccumulator,
) -> Folded {
    let mut folded = Folded {
        sources: Vec::new(),
        failed: false,
    };
    for client in fold_order(clients, card.id) {
        match client.fold_into(context, card.id, accumulator) {
            Ok(sources) => folded.sources.extend(sources),
            Err(error) => {
                folded.failed = true;
                crate::app_warn!(
                    client.log_target(),
                    "{} usage history could not be folded into {}: {error}",
                    client.name(),
                    card.name
                );
            }
        }
    }
    folded
}

/// The clients folded into `card_id`, in order: every client except the one the Card reads
/// itself.
fn fold_order<'a>(clients: &[&'a dyn HomeClient], card_id: &str) -> Vec<&'a dyn HomeClient> {
    clients
        .iter()
        .copied()
        .filter(|client| client.native_card() != Some(card_id))
        .collect()
}

/// Names the Card's own logs, if any, and the contributing clients.
fn source_note(native_source: Option<&str>, sources: &[&str]) -> String {
    let (base, extras, tail) = match (native_source, sources) {
        (Some(base), extras) => (base, extras, ""),
        (None, [first, extras @ ..]) => (*first, extras, " history"),
        (None, []) => return DEFAULT_SOURCE_NOTE.into(),
    };
    let suffix = match extras {
        [] => String::new(),
        [only] => format!(" and {only}"),
        [leading @ .., last] => format!(", {}, and {last}", leading.join(", ")),
    };
    format!("From your {base}{suffix}{tail} (estimated)")
}

fn contributed(contributed: bool, source: &'static str) -> Vec<&'static str> {
    if contributed {
        vec![source]
    } else {
        Vec::new()
    }
}

struct Pi;

impl HomeClient for Pi {
    fn name(&self) -> &'static str {
        "pi/oh-my-pi"
    }
    fn log_target(&self) -> &'static str {
        "plugin:pi"
    }
    fn fold_into(
        &self,
        context: &FoldContext<'_>,
        card_id: &str,
        accumulator: &mut DailyUsageAccumulator,
    ) -> Result<Vec<&'static str>, FoldError> {
        let sources = pi_usage::scan_into(
            context.storage,
            context.now,
            context.pricing,
            card_id,
            accumulator,
        )?;
        let mut labels = contributed(sources.pi, "pi");
        labels.extend(contributed(sources.oh_my_pi, "oh-my-pi"));
        Ok(labels)
    }
}

struct Antigravity;

impl HomeClient for Antigravity {
    fn name(&self) -> &'static str {
        "Antigravity"
    }
    fn log_target(&self) -> &'static str {
        "plugin:antigravity"
    }
    fn native_card(&self) -> Option<&'static str> {
        Some("antigravity")
    }
    fn fold_into(
        &self,
        context: &FoldContext<'_>,
        card_id: &str,
        accumulator: &mut DailyUsageAccumulator,
    ) -> Result<Vec<&'static str>, FoldError> {
        let landed = antigravity::local_usage::scan_routed_usage_into(
            context.storage,
            context.now,
            context.pricing,
            card_id,
            accumulator,
        )?;
        Ok(contributed(landed, "Antigravity"))
    }
}

struct ClaudeCode;

impl HomeClient for ClaudeCode {
    fn name(&self) -> &'static str {
        "Claude Code"
    }
    fn log_target(&self) -> &'static str {
        "plugin:claude"
    }
    fn native_card(&self) -> Option<&'static str> {
        Some("claude")
    }
    fn fold_into(
        &self,
        context: &FoldContext<'_>,
        card_id: &str,
        accumulator: &mut DailyUsageAccumulator,
    ) -> Result<Vec<&'static str>, FoldError> {
        let landed = claude::local_usage::scan_routed_usage_into(
            context.storage,
            context.now,
            context.pricing,
            card_id,
            accumulator,
        )?;
        Ok(contributed(landed, "Claude Code"))
    }
}

struct CodexCli;

impl HomeClient for CodexCli {
    fn name(&self) -> &'static str {
        "Codex CLI"
    }
    fn log_target(&self) -> &'static str {
        "plugin:codex"
    }
    fn native_card(&self) -> Option<&'static str> {
        Some("codex")
    }
    fn fold_into(
        &self,
        context: &FoldContext<'_>,
        card_id: &str,
        accumulator: &mut DailyUsageAccumulator,
    ) -> Result<Vec<&'static str>, FoldError> {
        let landed = codex::local_usage::scan_routed_usage_into(
            context.storage,
            context.now,
            context.pricing,
            card_id,
            accumulator,
        )?;
        Ok(contributed(landed, "Codex"))
    }
}

struct GrokCli;

impl HomeClient for GrokCli {
    fn name(&self) -> &'static str {
        "Grok CLI"
    }
    fn log_target(&self) -> &'static str {
        "plugin:grok"
    }
    fn native_card(&self) -> Option<&'static str> {
        Some("grok")
    }
    fn fold_into(
        &self,
        context: &FoldContext<'_>,
        card_id: &str,
        accumulator: &mut DailyUsageAccumulator,
    ) -> Result<Vec<&'static str>, FoldError> {
        let landed = grok::local_usage::scan_routed_usage_into(
            context.storage,
            context.now,
            context.pricing,
            card_id,
            accumulator,
        )?;
        Ok(contributed(landed, "Grok"))
    }
}

/// Kimi Code's logs have no Card that reads them itself: the Kimi Card folds them in too.
struct KimiCode;

impl HomeClient for KimiCode {
    fn name(&self) -> &'static str {
        "Kimi Code"
    }
    fn log_target(&self) -> &'static str {
        "plugin:kimi"
    }
    fn fold_into(
        &self,
        context: &FoldContext<'_>,
        card_id: &str,
        accumulator: &mut DailyUsageAccumulator,
    ) -> Result<Vec<&'static str>, FoldError> {
        let landed = kimi::local_usage::scan_into(
            context.storage,
            context.now,
            context.pricing,
            card_id,
            accumulator,
        )?;
        Ok(contributed(landed, "Kimi Code"))
    }
}

struct GeminiCli;

impl HomeClient for GeminiCli {
    fn name(&self) -> &'static str {
        "Gemini CLI"
    }
    fn log_target(&self) -> &'static str {
        "plugin:gemini-cli"
    }
    fn fold_into(
        &self,
        context: &FoldContext<'_>,
        card_id: &str,
        accumulator: &mut DailyUsageAccumulator,
    ) -> Result<Vec<&'static str>, FoldError> {
        let landed = gemini_cli_usage::scan_into(
            context.storage,
            context.now,
            context.pricing,
            card_id,
            accumulator,
        )?;
        Ok(contributed(landed, "Gemini CLI"))
    }
}

struct OpenCode;

impl HomeClient for OpenCode {
    fn name(&self) -> &'static str {
        "OpenCode"
    }
    fn log_target(&self) -> &'static str {
        "plugin:opencode"
    }
    fn fold_into(
        &self,
        context: &FoldContext<'_>,
        card_id: &str,
        accumulator: &mut DailyUsageAccumulator,
    ) -> Result<Vec<&'static str>, FoldError> {
        let landed =
            opencode::scan_routed_usage_into(context.now, context.pricing, card_id, accumulator)?;
        Ok(contributed(landed, "OpenCode"))
    }
}

struct ZCode;

impl HomeClient for ZCode {
    fn name(&self) -> &'static str {
        "ZCode"
    }
    fn log_target(&self) -> &'static str {
        "plugin:zcode"
    }
    fn fold_into(
        &self,
        context: &FoldContext<'_>,
        card_id: &str,
        accumulator: &mut DailyUsageAccumulator,
    ) -> Result<Vec<&'static str>, FoldError> {
        let Some(target) = ZCodeProvider::from_card_id(card_id) else {
            return Ok(Vec::new());
        };
        let landed = ZCodeUsageScanner::new().scan_into(
            context.now,
            context.pricing,
            target,
            accumulator,
        )?;
        Ok(contributed(landed, "ZCode"))
    }
}

/// Local-device usage for the Cards without logs of their own (Kimi, Z.ai, MiniMax), folded in
/// from every Home client.
pub(crate) struct RoutedLocalUsage {
    storage: Arc<Storage>,
    pricing: Arc<PricingStore>,
}

impl RoutedLocalUsage {
    pub(crate) fn new(storage: Arc<Storage>, pricing: Arc<PricingStore>) -> Self {
        Self { storage, pricing }
    }

    pub(crate) fn history(
        &self,
        provider_id: &str,
        provider_name: &str,
        now: DateTime<Utc>,
        warnings: &mut Vec<String>,
    ) -> UsageHistory {
        let pricing = self.pricing.current();
        let context = FoldContext {
            storage: &self.storage,
            now,
            pricing: &pricing,
        };
        let card = Card {
            id: provider_id,
            name: provider_name,
        };
        scan_or_cached_usage(
            &self.storage,
            provider_id,
            CacheIdentity::Unscoped,
            provider_name,
            || folded_history(&CLIENTS, &context, card),
            warnings,
        )
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use chrono::{TimeZone, Utc};
    use tempfile::tempdir;

    use super::{
        card_history, fold_order, folded_history, source_note, Card, DailyUsageAccumulator,
        FoldContext, FoldError, FoldInUnavailable, HomeClient, CLIENTS, DEFAULT_SOURCE_NOTE,
    };
    use crate::{pricing::test_bundled_pricing, storage::Storage};

    const CARDS: [&str; 7] = [
        "codex",
        "claude",
        "antigravity",
        "grok",
        "kimi",
        "zai",
        "minimax",
    ];

    /// A client that reports fixed sources, or fails, and records which Cards asked it.
    struct Fake {
        name: &'static str,
        native_card: Option<&'static str>,
        sources: Option<Vec<&'static str>>,
        asked: Mutex<Vec<String>>,
    }

    impl Fake {
        fn new(name: &'static str, sources: Option<Vec<&'static str>>) -> Self {
            Self {
                name,
                native_card: None,
                sources,
                asked: Mutex::new(Vec::new()),
            }
        }

        fn asked(&self) -> Vec<String> {
            self.asked.lock().unwrap().clone()
        }
    }

    impl HomeClient for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn log_target(&self) -> &'static str {
            "plugin:test"
        }
        fn native_card(&self) -> Option<&'static str> {
            self.native_card
        }
        fn fold_into(
            &self,
            _context: &FoldContext<'_>,
            card_id: &str,
            _accumulator: &mut DailyUsageAccumulator,
        ) -> Result<Vec<&'static str>, FoldError> {
            self.asked.lock().unwrap().push(card_id.to_owned());
            self.sources.clone().ok_or_else(|| "unreadable".into())
        }
    }

    fn with_context(test: impl FnOnce(&FoldContext<'_>)) {
        let directory = tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("cache.db")).unwrap();
        let pricing = test_bundled_pricing();
        let context = FoldContext {
            storage: &storage,
            now: Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap(),
            pricing: &pricing,
        };
        test(&context);
    }

    fn card(id: &str) -> Card<'_> {
        Card { id, name: id }
    }

    /// Which clients each Card folds in, and in what order: the same order on every Card.
    #[test]
    fn each_card_folds_in_every_client_but_the_one_it_reads_itself() {
        let all = [
            "pi/oh-my-pi",
            "Antigravity",
            "Claude Code",
            "Codex CLI",
            "Grok CLI",
            "Kimi Code",
            "Gemini CLI",
            "OpenCode",
            "ZCode",
        ];
        for card_id in CARDS {
            let order = fold_order(&CLIENTS, card_id)
                .iter()
                .map(|client| client.name())
                .collect::<Vec<_>>();
            let expected = all
                .iter()
                .copied()
                .filter(|name| match card_id {
                    "codex" => *name != "Codex CLI",
                    "claude" => *name != "Claude Code",
                    "antigravity" => *name != "Antigravity",
                    "grok" => *name != "Grok CLI",
                    _ => true,
                })
                .collect::<Vec<_>>();
            assert_eq!(order, expected, "{card_id}");
        }
    }

    #[test]
    fn source_notes_name_the_native_logs_then_contributing_clients() {
        let note = |native, sources: &[&str]| source_note(native, sources);
        assert_eq!(
            note(Some("Codex logs"), &[]),
            "From your Codex logs (estimated)"
        );
        assert_eq!(
            note(Some("Codex logs"), &["pi"]),
            "From your Codex logs and pi (estimated)"
        );
        assert_eq!(
            note(Some("Grok logs"), &["pi", "oh-my-pi", "ZCode"]),
            "From your Grok logs, pi, oh-my-pi, and ZCode (estimated)"
        );
        assert_eq!(note(None, &[]), DEFAULT_SOURCE_NOTE);
        assert_eq!(
            note(None, &["ZCode"]),
            "From your ZCode history (estimated)"
        );
        assert_eq!(
            note(None, &["Claude Code", "OpenCode"]),
            "From your Claude Code and OpenCode history (estimated)"
        );
        assert_eq!(
            note(None, &["Claude Code", "OpenCode", "ZCode"]),
            "From your Claude Code, OpenCode, and ZCode history (estimated)"
        );
    }

    #[test]
    fn a_card_never_asks_the_client_it_reads_itself() {
        with_context(|context| {
            let own = Fake {
                native_card: Some("codex"),
                ..Fake::new("Codex CLI", Some(vec!["Codex"]))
            };
            let other = Fake::new("pi/oh-my-pi", Some(vec![]));
            let clients: [&dyn HomeClient; 2] = [&own, &other];
            card_history(&clients, context, card("codex"), "Codex logs", |_| {
                Ok::<_, FoldInUnavailable>(true)
            })
            .unwrap();
            folded_history(&clients, context, card("zai")).unwrap();
            assert_eq!(own.asked(), ["zai"]);
            assert_eq!(other.asked(), ["codex", "zai"]);
        });
    }

    /// A Card fails over a failing client only when nothing contributed, its own logs included.
    #[test]
    fn a_failing_client_fails_a_card_only_when_nothing_contributed() {
        with_context(|context| {
            for (native, other, expected_ok) in [
                (true, None, true),
                (false, None, false),
                (false, Some(vec!["pi"]), true),
                (false, Some(vec![]), false),
            ] {
                let failing = Fake::new("ZCode", None);
                let other = Fake::new("pi/oh-my-pi", other.clone());
                let clients: [&dyn HomeClient; 2] = [&other, &failing];
                let history = card_history(&clients, context, card("codex"), "Codex logs", |_| {
                    Ok::<_, FoldInUnavailable>(native)
                });
                assert_eq!(history.is_ok(), expected_ok, "native {native}");
            }
        });
    }

    #[test]
    fn a_failing_native_scan_fails_the_card_before_folding_in() {
        with_context(|context| {
            let other = Fake::new("pi/oh-my-pi", Some(vec!["pi"]));
            let clients: [&dyn HomeClient; 1] = [&other];
            let history = card_history(&clients, context, card("codex"), "Codex logs", |_| {
                Err::<bool, _>(NativeFailed)
            });
            assert!(matches!(history, Err(NativeFailed)));
            assert!(other.asked().is_empty());
        });
    }

    #[derive(Debug)]
    struct NativeFailed;

    impl From<FoldInUnavailable> for NativeFailed {
        fn from(_: FoldInUnavailable) -> Self {
            unreachable!("only the native scan fails here")
        }
    }

    /// Without logs of its own, a Card fails only when nothing contributed and a client failed.
    #[test]
    fn a_card_without_its_own_logs_fails_only_when_nothing_contributed() {
        with_context(|context| {
            for (sources, expected_ok) in [
                ([None, None], false),
                ([None, Some(vec![])], false),
                ([None, Some(vec!["ZCode"])], true),
                ([Some(vec![]), Some(vec![])], true),
            ] {
                let [first, second] = sources.clone();
                let first = Fake::new("pi/oh-my-pi", first);
                let second = Fake::new("ZCode", second);
                let clients: [&dyn HomeClient; 2] = [&first, &second];
                let history = folded_history(&clients, context, card("zai"));
                assert_eq!(history.is_ok(), expected_ok, "{sources:?}");
            }
        });
    }
}
