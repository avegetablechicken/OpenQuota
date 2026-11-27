use std::sync::atomic::{AtomicU64, Ordering};

/// Cancels stale delayed dismissals when a tray click races a focus-loss event.
///
/// Windows moves focus away from the popup just before it reports some tray
/// clicks. Keeping a generation makes the click authoritative: the pending
/// focus-loss dismissal becomes stale instead of hiding a freshly opened popup.
#[derive(Default)]
pub struct PopupDismissGuard {
    generation: AtomicU64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DismissAction {
    Cancel,
    WaitForMenu,
    Hide,
}

impl PopupDismissGuard {
    pub fn cancel_pending(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn token(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn is_current(&self, token: u64) -> bool {
        self.token() == token
    }

    pub fn action(
        &self,
        token: u64,
        unfocused: bool,
        floating: bool,
        menu_open: bool,
    ) -> DismissAction {
        if !self.is_current(token) || !unfocused || floating {
            DismissAction::Cancel
        } else if menu_open {
            DismissAction::WaitForMenu
        } else {
            DismissAction::Hide
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DismissAction, PopupDismissGuard};

    #[test]
    fn tray_interaction_invalidates_a_pending_dismissal() {
        let guard = PopupDismissGuard::default();
        let pending = guard.token();

        guard.cancel_pending();

        assert!(!guard.is_current(pending));
    }

    #[test]
    fn unchanged_dismissal_token_remains_current() {
        let guard = PopupDismissGuard::default();
        let pending = guard.token();

        assert!(guard.is_current(pending));
    }

    #[test]
    fn native_menu_focus_loss_waits_until_the_menu_closes() {
        let guard = PopupDismissGuard::default();
        let pending = guard.token();
        assert_eq!(
            guard.action(pending, true, false, true),
            DismissAction::WaitForMenu
        );
        // Selecting an option or cancelling restores the panel's focus.
        assert_eq!(
            guard.action(pending, false, false, false),
            DismissAction::Cancel
        );
        // Clicking another application closes the menu without restoring focus.
        assert_eq!(
            guard.action(pending, true, false, false),
            DismissAction::Hide
        );
    }

    #[test]
    fn tray_interaction_cancels_a_dismissal_waiting_for_a_menu() {
        let guard = PopupDismissGuard::default();
        let pending = guard.token();
        assert_eq!(
            guard.action(pending, true, false, true),
            DismissAction::WaitForMenu
        );
        guard.cancel_pending();
        assert_eq!(
            guard.action(pending, true, false, true),
            DismissAction::Cancel
        );
        assert_eq!(
            guard.action(pending, true, false, false),
            DismissAction::Cancel
        );
    }

    #[test]
    fn floating_window_cancels_pending_dismissal() {
        let guard = PopupDismissGuard::default();
        assert_eq!(
            guard.action(guard.token(), true, true, false),
            DismissAction::Cancel
        );
    }
}
