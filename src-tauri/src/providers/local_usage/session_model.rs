//! Shared session-level model inheritance for local usage logs.
//!
//! When a token-bearing log entry omits its `model` field, local-log parsers use
//! [`SessionModelTracker`] (or [`inherit_in_session`]) to inherit the most recently observed model
//! within the same session log instead of losing attribution as `Unattributed`. Sessions that never
//! name a model still remain `Unattributed` so usage is never guessed across sessions.
//!
//! Whether omitted-model entries are verified to run on the session's active model is a property of
//! the CLI that wrote the log (not the model family or destination Card):
//! - Verified whitelist: Codex CLI (`token_count` following `turn_context`) and Antigravity
//!   (`CHECKPOINT` compaction steps reusing the active model's prompt cache). Inherited entries
//!   from these CLIs are attributed silently.
//! - All other CLIs: inherited entries are still attributed and estimated, and every Card receiving
//!   them displays a warning noting that session-model inheritance is unverified for that CLI.

use std::collections::BTreeSet;

/// Local coding CLI (log producer) whose session log inherited a missing `model` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(in crate::providers) enum SessionModelCli {
    Antigravity,
    ClaudeCode,
    CodexCli,
    GeminiCli,
    KimiCode,
    OhMyPi,
    OpenCode,
    Pi,
}

impl SessionModelCli {
    /// Returns `true` only for CLIs whose log format is verified to reuse the session's active
    /// model when a token event omits `model`.
    pub(in crate::providers) const fn is_verified(self) -> bool {
        matches!(self, Self::Antigravity | Self::CodexCli)
    }

    pub(in crate::providers) const fn display_name(self) -> &'static str {
        match self {
            Self::Antigravity => "Antigravity",
            Self::ClaudeCode => "Claude Code",
            Self::CodexCli => "Codex CLI",
            Self::GeminiCli => "Gemini CLI",
            Self::KimiCode => "Kimi Code",
            Self::OhMyPi => "oh-my-pi",
            Self::OpenCode => "OpenCode",
            Self::Pi => "pi",
        }
    }
}

/// Outcome of resolving one event's model within a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::providers) struct ResolvedSessionModel {
    pub model: Option<String>,
    /// `true` when the event omitted `model` and inherited the session's active model.
    pub inherited: bool,
}

/// Tracks the active model across events in a single session log.
#[derive(Debug, Default, Clone)]
pub(in crate::providers) struct SessionModelTracker {
    active_model: Option<String>,
}

impl SessionModelTracker {
    /// Records a model from a non-token context event (such as `turn_context` or `llm.request`).
    pub(in crate::providers) fn observe(&mut self, model: Option<&str>) {
        if let Some(model) = trimmed_non_empty(model) {
            self.active_model = Some(model.to_owned());
        }
    }

    /// Resolves a token-bearing event's model. Explicit models become the new active model;
    /// omitted models inherit the last active model in the same session.
    pub(in crate::providers) fn resolve(
        &mut self,
        explicit: Option<String>,
    ) -> ResolvedSessionModel {
        if let Some(model) = explicit
            .as_deref()
            .and_then(|value| trimmed_non_empty(Some(value)))
        {
            let model = model.to_owned();
            self.active_model = Some(model.clone());
            return ResolvedSessionModel {
                model: Some(model),
                inherited: false,
            };
        }
        match self.active_model.clone() {
            Some(model) => ResolvedSessionModel {
                model: Some(model),
                inherited: true,
            },
            None => ResolvedSessionModel {
                model: None,
                inherited: false,
            },
        }
    }
}

/// Applies [`SessionModelTracker`] in chronological order across a single session's parsed events.
pub(in crate::providers) fn inherit_in_session<T>(
    events: &mut [T],
    mut model_of: impl FnMut(&mut T) -> (&mut Option<String>, &mut bool),
) {
    let mut tracker = SessionModelTracker::default();
    for event in events {
        let (model, inherited) = model_of(event);
        let resolved = tracker.resolve(model.take());
        *model = resolved.model;
        *inherited = resolved.inherited;
    }
}

/// Formats the card-level warning for any unverified CLIs whose inherited-model usage landed on
/// the Card.
pub(in crate::providers) fn unverified_inheritance_warning(
    clis: &BTreeSet<SessionModelCli>,
) -> Option<String> {
    let names = clis
        .iter()
        .filter(|cli| !cli.is_verified())
        .map(|cli| cli.display_name())
        .collect::<Vec<_>>();
    match names.as_slice() {
        [] => None,
        [only] => Some(format!(
            "Some {only} usage entries omitted their model and were attributed to the session's active model (unverified for {only})."
        )),
        [first, second] => Some(format!(
            "Some {first} and {second} usage entries omitted their model and were attributed to the session's active model (unverified for these clients)."
        )),
        [leading @ .., last] => Some(format!(
            "Some {}, and {last} usage entries omitted their model and were attributed to the session's active model (unverified for these clients).",
            leading.join(", ")
        )),
    }
}

fn trimmed_non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{
        inherit_in_session, unverified_inheritance_warning, SessionModelCli, SessionModelTracker,
    };

    #[test]
    fn whitelist_only_permits_codex_and_antigravity() {
        assert!(SessionModelCli::Antigravity.is_verified());
        assert!(SessionModelCli::CodexCli.is_verified());
        for cli in [
            SessionModelCli::ClaudeCode,
            SessionModelCli::GeminiCli,
            SessionModelCli::KimiCode,
            SessionModelCli::OhMyPi,
            SessionModelCli::OpenCode,
            SessionModelCli::Pi,
        ] {
            assert!(!cli.is_verified(), "{cli:?} must remain unverified");
        }
    }

    #[test]
    fn tracker_inherits_active_model_and_leaves_leading_missing_events_unattributed() {
        let mut tracker = SessionModelTracker::default();

        let leading = tracker.resolve(None);
        assert_eq!(leading.model, None);
        assert!(!leading.inherited);

        let first = tracker.resolve(Some("  gemini-4-neon-high  ".into()));
        assert_eq!(first.model.as_deref(), Some("gemini-4-neon-high"));
        assert!(!first.inherited);

        let inherited = tracker.resolve(Some("   ".into()));
        assert_eq!(inherited.model.as_deref(), Some("gemini-4-neon-high"));
        assert!(inherited.inherited);

        tracker.observe(Some("claude-opus-4-6"));
        let after_context = tracker.resolve(None);
        assert_eq!(after_context.model.as_deref(), Some("claude-opus-4-6"));
        assert!(after_context.inherited);
    }

    #[test]
    fn inherit_in_session_updates_events_in_order() {
        let mut events = vec![
            (None, false),
            (Some("model-a".to_owned()), false),
            (None, false),
            (Some("model-b".to_owned()), false),
            (None, false),
        ];
        inherit_in_session(&mut events, |(model, inherited)| (model, inherited));
        assert_eq!(
            events,
            vec![
                (None, false),
                (Some("model-a".to_owned()), false),
                (Some("model-a".to_owned()), true),
                (Some("model-b".to_owned()), false),
                (Some("model-b".to_owned()), true),
            ]
        );
    }

    #[test]
    fn warning_formatting_ignores_whitelisted_clis_and_lists_unverified_clis() {
        let verified = BTreeSet::from([SessionModelCli::Antigravity, SessionModelCli::CodexCli]);
        assert_eq!(unverified_inheritance_warning(&verified), None);

        let single = BTreeSet::from([SessionModelCli::ClaudeCode]);
        assert_eq!(
            unverified_inheritance_warning(&single).as_deref(),
            Some(
                "Some Claude Code usage entries omitted their model and were attributed to the session's active model (unverified for Claude Code)."
            )
        );

        let pair = BTreeSet::from([SessionModelCli::GeminiCli, SessionModelCli::ClaudeCode]);
        assert_eq!(
            unverified_inheritance_warning(&pair).as_deref(),
            Some(
                "Some Claude Code and Gemini CLI usage entries omitted their model and were attributed to the session's active model (unverified for these clients)."
            )
        );

        let triple = BTreeSet::from([
            SessionModelCli::Pi,
            SessionModelCli::ClaudeCode,
            SessionModelCli::GeminiCli,
        ]);
        assert_eq!(
            unverified_inheritance_warning(&triple).as_deref(),
            Some(
                "Some Claude Code, Gemini CLI, and pi usage entries omitted their model and were attributed to the session's active model (unverified for these clients)."
            )
        );
    }
}
