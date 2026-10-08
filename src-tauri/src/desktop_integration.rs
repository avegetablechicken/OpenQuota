use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

use crate::models::WindowMode;

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxSessionType {
    X11,
    Wayland,
    Unknown,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinuxDesktop {
    Gnome,
    Kde,
    Other,
}

#[derive(Debug, Clone)]
pub struct DesktopIntegration {
    tray_available: Arc<AtomicBool>,
    floating_window: Arc<AtomicBool>,
    platform_label: Option<String>,
}

impl DesktopIntegration {
    pub fn detect() -> Self {
        #[cfg(target_os = "linux")]
        {
            let session = parse_session_type(std::env::var("XDG_SESSION_TYPE").ok().as_deref());
            let desktop = parse_desktop(std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref());
            let tray_available = status_notifier_host_available();
            linux_integration(session, desktop, tray_available)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self {
                tray_available: Arc::new(AtomicBool::new(true)),
                floating_window: Arc::new(AtomicBool::new(false)),
                platform_label: None,
            }
        }
    }

    pub fn tray_available(&self) -> bool {
        self.tray_available.load(Ordering::SeqCst)
    }

    pub fn is_floating(&self) -> bool {
        self.floating_window.load(Ordering::SeqCst)
    }

    pub fn exits_on_close(&self) -> bool {
        self.is_floating() && !self.tray_available()
    }

    pub fn apply_window_mode(&self, mode: WindowMode) -> bool {
        let floating = !self.tray_available() || mode == WindowMode::Floating;
        self.set_floating(floating);
        floating
    }

    pub fn disable_tray(&self) -> bool {
        let changed = self.tray_available.swap(false, Ordering::SeqCst);
        self.set_floating(true);
        changed
    }

    #[cfg(any(target_os = "linux", test))]
    pub fn enable_tray(&self) {
        self.tray_available.store(true, Ordering::SeqCst);
    }

    pub(crate) fn set_floating(&self, floating: bool) {
        self.floating_window.store(floating, Ordering::SeqCst);
    }

    pub fn platform_summary(&self) -> Option<String> {
        self.platform_label.as_ref().map(|label| {
            let mode = if self.tray_available() {
                "StatusNotifier tray"
            } else {
                "standalone window"
            };
            format!("{label} · {mode}")
        })
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_integration(
    session: LinuxSessionType,
    desktop: LinuxDesktop,
    tray_available: bool,
) -> DesktopIntegration {
    let desktop = match desktop {
        LinuxDesktop::Gnome => "GNOME",
        LinuxDesktop::Kde => "KDE Plasma",
        LinuxDesktop::Other => "Linux desktop",
    };
    let session = match session {
        LinuxSessionType::X11 => "X11",
        LinuxSessionType::Wayland => "Wayland",
        LinuxSessionType::Unknown => "unknown session",
    };
    DesktopIntegration {
        tray_available: Arc::new(AtomicBool::new(tray_available)),
        floating_window: Arc::new(AtomicBool::new(!tray_available)),
        platform_label: Some(format!("{desktop} · {session}")),
    }
}

#[cfg(any(target_os = "linux", test))]
pub fn parse_desktop(value: Option<&str>) -> LinuxDesktop {
    let names = value
        .unwrap_or_default()
        .split(':')
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    if names.iter().any(|name| name.contains("gnome")) {
        LinuxDesktop::Gnome
    } else if names
        .iter()
        .any(|name| name.contains("kde") || name.contains("plasma"))
    {
        LinuxDesktop::Kde
    } else {
        LinuxDesktop::Other
    }
}

#[cfg(any(target_os = "linux", test))]
pub fn parse_session_type(value: Option<&str>) -> LinuxSessionType {
    match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("x11") => LinuxSessionType::X11,
        Some("wayland") => LinuxSessionType::Wayland,
        _ => LinuxSessionType::Unknown,
    }
}

#[cfg(target_os = "linux")]
fn status_notifier_host_available() -> bool {
    match std::env::var("OPENQUOTA_LINUX_TRAY_HOST").as_deref() {
        Ok("available") => return true,
        Ok("unavailable") => return false,
        _ => {}
    }
    status_notifier_state().is_ok_and(|(available, _)| available)
}

#[cfg(target_os = "linux")]
pub fn status_notifier_monitor_forced_off() -> bool {
    matches!(
        std::env::var("OPENQUOTA_LINUX_TRAY_HOST").as_deref(),
        Ok("available" | "unavailable")
    )
}

#[cfg(target_os = "linux")]
pub fn status_notifier_state() -> Result<(bool, bool), String> {
    const WATCHER_NAME: &str = "org.kde.StatusNotifierWatcher";

    let connection = zbus::blocking::Connection::session()
        .map_err(|error| format!("session bus unavailable: {error}"))?;
    let proxy = zbus::blocking::fdo::DBusProxy::new(&connection)
        .map_err(|error| format!("session bus proxy unavailable: {error}"))?;
    let available =
        match proxy.get_name_owner(WATCHER_NAME.try_into().expect("valid watcher bus name")) {
            Ok(_) => true,
            Err(zbus::fdo::Error::NameHasNoOwner(_)) => false,
            Err(error) => return Err(format!("watcher snapshot failed: {error}")),
        };
    // GNOME 3.36 disables AppIndicators while locked. The indicator library
    // already re-registers the existing icon when the watcher returns.
    let locked = zbus::blocking::Proxy::new(
        &connection,
        "org.gnome.ScreenSaver",
        "/org/gnome/ScreenSaver",
        "org.gnome.ScreenSaver",
    )
    .and_then(|proxy| proxy.call::<_, _, bool>("GetActive", &()))
    .unwrap_or(false);
    Ok((available, locked))
}

#[cfg(any(target_os = "linux", test))]
#[derive(Default)]
pub struct TrayHostMonitor {
    missing_since: Option<std::time::Instant>,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, PartialEq, Eq)]
pub enum TrayHostAction {
    Unavailable,
    Recreate,
    Unchanged,
}

#[cfg(any(target_os = "linux", test))]
impl TrayHostMonitor {
    pub fn observe(
        &mut self,
        host_available: bool,
        locked: bool,
        tray_available: bool,
        now: std::time::Instant,
    ) -> TrayHostAction {
        if host_available {
            self.missing_since = None;
            return if tray_available {
                TrayHostAction::Unchanged
            } else {
                TrayHostAction::Recreate
            };
        }
        if locked {
            self.missing_since = None;
            return TrayHostAction::Unchanged;
        }
        let missing_since = self.missing_since.get_or_insert(now);
        if now.duration_since(*missing_since) >= std::time::Duration::from_secs(30) {
            return TrayHostAction::Unavailable;
        }
        TrayHostAction::Unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_desktop, parse_session_type, LinuxDesktop, LinuxSessionType};
    use crate::models::WindowMode;

    #[test]
    fn recognizes_x11_and_wayland_sessions_case_insensitively() {
        assert_eq!(parse_session_type(Some("x11")), LinuxSessionType::X11);
        assert_eq!(
            parse_session_type(Some(" Wayland ")),
            LinuxSessionType::Wayland
        );
        assert_eq!(parse_session_type(Some("tty")), LinuxSessionType::Unknown);
        assert_eq!(parse_session_type(None), LinuxSessionType::Unknown);
    }

    #[test]
    fn recognizes_gnome_and_kde_desktop_name_lists() {
        assert_eq!(parse_desktop(Some("ubuntu:GNOME")), LinuxDesktop::Gnome);
        assert_eq!(parse_desktop(Some("KDE")), LinuxDesktop::Kde);
        assert_eq!(parse_desktop(Some("plasma:wayland")), LinuxDesktop::Kde);
        assert_eq!(parse_desktop(Some("sway")), LinuxDesktop::Other);
    }

    #[test]
    fn platform_summary_explains_the_linux_fallback_mode() {
        let integration =
            super::linux_integration(LinuxSessionType::Wayland, LinuxDesktop::Gnome, false);
        assert_eq!(
            integration.platform_summary().as_deref(),
            Some("GNOME · Wayland · standalone window")
        );
    }

    #[test]
    fn window_mode_is_independent_from_tray_availability() {
        let with_tray =
            super::linux_integration(LinuxSessionType::Wayland, LinuxDesktop::Kde, true);
        assert!(with_tray.tray_available());
        assert!(!with_tray.apply_window_mode(WindowMode::Popup));
        assert!(with_tray.apply_window_mode(WindowMode::Floating));
        assert!(!with_tray.exits_on_close());

        let without_tray =
            super::linux_integration(LinuxSessionType::Wayland, LinuxDesktop::Gnome, false);
        assert!(!without_tray.tray_available());
        assert!(without_tray.apply_window_mode(WindowMode::Popup));
        assert!(without_tray.exits_on_close());
    }

    #[test]
    fn tray_loss_and_recovery_restore_the_selected_window_mode() {
        let integration = super::linux_integration(LinuxSessionType::X11, LinuxDesktop::Kde, true);
        assert!(!integration.apply_window_mode(WindowMode::Popup));

        assert!(integration.disable_tray());
        assert!(!integration.tray_available());
        assert!(integration.is_floating());
        assert!(integration.exits_on_close());
        assert_eq!(
            integration.platform_summary().as_deref(),
            Some("KDE Plasma · X11 · standalone window")
        );
        assert!(!integration.disable_tray());

        integration.enable_tray();
        assert!(!integration.apply_window_mode(WindowMode::Popup));
        assert!(!integration.exits_on_close());
        assert!(integration.apply_window_mode(WindowMode::Floating));
        assert!(!integration.exits_on_close());
    }

    #[test]
    fn lock_unlock_and_brief_host_restarts_preserve_the_native_tray() {
        use super::{TrayHostAction::*, TrayHostMonitor};
        use std::time::{Duration, Instant};
        let mut monitor = TrayHostMonitor::default();
        let now = Instant::now();
        assert_eq!(monitor.observe(true, false, true, now), Unchanged);
        // The watcher may disappear before the lock signal arrives.
        assert_eq!(monitor.observe(false, false, true, now), Unchanged);
        assert_eq!(
            monitor.observe(false, true, true, now + Duration::from_secs(3600)),
            Unchanged
        );
        // Allow the host time to start after unlocking.
        assert_eq!(
            monitor.observe(false, false, true, now + Duration::from_secs(3602)),
            Unchanged
        );
        assert_eq!(
            monitor.observe(true, false, true, now + Duration::from_secs(3604)),
            Unchanged
        );
    }

    #[test]
    fn monitor_handles_late_startup_and_retries_failed_tray_creation() {
        use super::{TrayHostAction::*, TrayHostMonitor};
        use std::time::Instant;
        let mut monitor = TrayHostMonitor::default();
        let now = Instant::now();
        assert_eq!(monitor.observe(false, false, false, now), Unchanged);
        assert_eq!(monitor.observe(true, false, false, now), Recreate);
        assert_eq!(monitor.observe(true, false, false, now), Recreate);
        assert_eq!(monitor.observe(true, false, true, now), Unchanged);
    }

    #[test]
    fn persistent_host_loss_only_falls_back_after_unlocked_grace_period() {
        use super::{TrayHostAction::*, TrayHostMonitor};
        use std::time::{Duration, Instant};
        let mut monitor = TrayHostMonitor::default();
        let now = Instant::now();
        assert_eq!(monitor.observe(false, false, true, now), Unchanged);
        assert_eq!(
            monitor.observe(false, false, true, now + Duration::from_secs(29)),
            Unchanged
        );
        assert_eq!(
            monitor.observe(false, false, true, now + Duration::from_secs(30)),
            Unavailable
        );
        assert_eq!(
            monitor.observe(true, false, false, now + Duration::from_secs(32)),
            Recreate
        );
    }
}
