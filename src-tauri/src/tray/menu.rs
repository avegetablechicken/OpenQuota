//! The tray menu as data, and the one place its entry ids are written and read.
//!
//! The Tauri side turns [`TrayEntry`] values into menu items and hands each clicked id back to
//! [`TrayAction::from_id`]. The macOS menu also compiles under test, so its content is checked on
//! every platform.

#[cfg(any(target_os = "macos", test))]
use crate::tray_presentation::MenuBarMenu;
use crate::{
    commands::settings::TraySetting,
    models::{AppSettings, MenuBarStyle, UpdateFrequency, UsageDisplay, WindowMode},
};

#[cfg(any(target_os = "macos", test))]
const MENU_BAR_HIDE_PREFIX: &str = "menu-bar-hide:";
#[cfg(any(target_os = "macos", test))]
const MENU_BAR_PIN_PREFIX: &str = "menu-bar-pin:";
#[cfg(any(target_os = "macos", test))]
const MENU_BAR_UNPIN_PREFIX: &str = "menu-bar-unpin:";

/// What clicking a tray menu entry does.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TrayAction {
    Setting(TraySetting),
    /// Removes a metric from the menu bar from the list of what it shows now. Its Add entry
    /// does the same, but each menu entry needs its own id.
    #[cfg(any(target_os = "macos", test))]
    HideMenuBarMetric(String),
    Open,
    Customize,
    Settings,
    Quit,
}

/// Every action whose id names no metric: what [`TrayAction::from_id`] reads back besides the
/// menu bar entries.
const FIXED: [TrayAction; 15] = [
    TrayAction::Setting(TraySetting::MenuBarStyle(MenuBarStyle::Icon)),
    TrayAction::Setting(TraySetting::MenuBarStyle(MenuBarStyle::Text)),
    TrayAction::Setting(TraySetting::MenuBarStyle(MenuBarStyle::Bars)),
    TrayAction::Setting(TraySetting::WindowMode(WindowMode::Popup)),
    TrayAction::Setting(TraySetting::WindowMode(WindowMode::Floating)),
    TrayAction::Setting(TraySetting::UsageDisplay(UsageDisplay::Left)),
    TrayAction::Setting(TraySetting::UsageDisplay(UsageDisplay::Used)),
    TrayAction::Setting(TraySetting::UpdateFrequency(UpdateFrequency::OneMinute)),
    TrayAction::Setting(TraySetting::UpdateFrequency(UpdateFrequency::FiveMinutes)),
    TrayAction::Setting(TraySetting::UpdateFrequency(UpdateFrequency::Adaptive)),
    TrayAction::Setting(TraySetting::LaunchAtLoginToggle),
    TrayAction::Open,
    TrayAction::Customize,
    TrayAction::Settings,
    TrayAction::Quit,
];

impl TrayAction {
    /// The menu entry id that [`Self::from_id`] reads back as this action.
    pub(crate) fn id(&self) -> String {
        let fixed = match self {
            #[cfg(any(target_os = "macos", test))]
            Self::Setting(TraySetting::MenuBarMetricPinned { metric_id, pinned }) => {
                let prefix = if *pinned {
                    MENU_BAR_PIN_PREFIX
                } else {
                    MENU_BAR_UNPIN_PREFIX
                };
                return format!("{prefix}{metric_id}");
            }
            #[cfg(any(target_os = "macos", test))]
            Self::HideMenuBarMetric(metric_id) => {
                return format!("{MENU_BAR_HIDE_PREFIX}{metric_id}")
            }
            Self::Setting(TraySetting::MenuBarStyle(style)) => match style {
                MenuBarStyle::Icon => "icon-style-openquota",
                MenuBarStyle::Text => "icon-style-text",
                MenuBarStyle::Bars => "icon-style-bars",
            },
            Self::Setting(TraySetting::WindowMode(mode)) => match mode {
                WindowMode::Popup => "tray-mode-popup",
                WindowMode::Floating => "tray-mode-floating",
            },
            Self::Setting(TraySetting::UsageDisplay(display)) => match display {
                UsageDisplay::Left => "usage-display-left",
                UsageDisplay::Used => "usage-display-used",
            },
            Self::Setting(TraySetting::UpdateFrequency(frequency)) => match frequency {
                UpdateFrequency::OneMinute => "update-frequency-one-minute",
                UpdateFrequency::FiveMinutes => "update-frequency-five-minutes",
                UpdateFrequency::Adaptive => "update-frequency-adaptive",
            },
            Self::Setting(TraySetting::LaunchAtLoginToggle) => "launch-at-login",
            Self::Open => "open",
            Self::Customize => "customize",
            Self::Settings => "settings",
            Self::Quit => "quit",
        };
        fixed.to_owned()
    }

    /// The action of the entry with `id`; `None` for submenus and unknown ids.
    pub(crate) fn from_id(id: &str) -> Option<Self> {
        #[cfg(any(target_os = "macos", test))]
        {
            if let Some(metric_id) = id.strip_prefix(MENU_BAR_HIDE_PREFIX) {
                return Some(Self::HideMenuBarMetric(metric_id.to_owned()));
            }
            let pinned = |metric_id: &str, pinned| {
                Self::Setting(TraySetting::MenuBarMetricPinned {
                    metric_id: metric_id.to_owned(),
                    pinned,
                })
            };
            if let Some(metric_id) = id.strip_prefix(MENU_BAR_PIN_PREFIX) {
                return Some(pinned(metric_id, true));
            }
            if let Some(metric_id) = id.strip_prefix(MENU_BAR_UNPIN_PREFIX) {
                return Some(pinned(metric_id, false));
            }
        }
        FIXED.iter().find(|action| action.id() == id).cloned()
    }
}

/// One entry of the tray menu.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum TrayEntry {
    Item {
        action: TrayAction,
        label: String,
        /// A check mark, for entries that show a state.
        checked: Option<bool>,
        enabled: bool,
        accelerator: Option<&'static str>,
    },
    Submenu {
        id: String,
        label: String,
        enabled: bool,
        entries: Vec<TrayEntry>,
    },
    Separator,
}

fn choice(setting: TraySetting, label: &str, checked: bool) -> TrayEntry {
    TrayEntry::Item {
        action: TrayAction::Setting(setting),
        label: label.to_owned(),
        checked: Some(checked),
        enabled: true,
        accelerator: None,
    }
}

fn command(action: TrayAction, label: &str, accelerator: Option<&'static str>) -> TrayEntry {
    TrayEntry::Item {
        action,
        label: label.to_owned(),
        checked: None,
        enabled: true,
        accelerator,
    }
}

fn submenu(id: &str, label: &str, entries: Vec<TrayEntry>) -> TrayEntry {
    TrayEntry::Submenu {
        id: id.to_owned(),
        label: label.to_owned(),
        enabled: true,
        entries,
    }
}

/// Tray Mode, Show Usage as and Update Frequency, which every platform offers.
fn shared_submenus(settings: &AppSettings) -> [TrayEntry; 3] {
    let mode = |value, label| {
        choice(
            TraySetting::WindowMode(value),
            label,
            settings.window_mode == value,
        )
    };
    let display = |value, label| {
        choice(
            TraySetting::UsageDisplay(value),
            label,
            settings.usage_display == value,
        )
    };
    let frequency = |value, label| {
        choice(
            TraySetting::UpdateFrequency(value),
            label,
            settings.update_frequency == value,
        )
    };
    [
        submenu(
            "tray-mode",
            "Tray Mode",
            vec![
                mode(WindowMode::Popup, "Tray Popup"),
                mode(WindowMode::Floating, "Floating Window"),
            ],
        ),
        submenu(
            "usage-display",
            "Show Usage as",
            vec![
                display(UsageDisplay::Left, "Left"),
                display(UsageDisplay::Used, "Used"),
            ],
        ),
        submenu(
            "update-frequency",
            "Update Frequency",
            vec![
                frequency(UpdateFrequency::OneMinute, "1 minute"),
                frequency(UpdateFrequency::FiveMinutes, "5 minutes"),
                frequency(UpdateFrequency::Adaptive, "Adaptive"),
            ],
        ),
    ]
}

fn launch_at_login(settings: &AppSettings) -> TrayEntry {
    choice(
        TraySetting::LaunchAtLoginToggle,
        "Launch at Login",
        settings.launch_at_login,
    )
}

/// The tray menu on Windows and Linux, where the window is opened from the menu.
#[cfg(any(not(target_os = "macos"), test))]
pub(crate) fn standard_menu(settings: &AppSettings) -> Vec<TrayEntry> {
    let [tray_mode, show_usage_as, update_frequency] = shared_submenus(settings);
    vec![
        command(TrayAction::Open, "Open OpenQuota", None),
        command(TrayAction::Customize, "Customize…", None),
        tray_mode,
        show_usage_as,
        update_frequency,
        TrayEntry::Separator,
        command(TrayAction::Settings, "More Settings…", None),
        launch_at_login(settings),
        command(TrayAction::Quit, "Quit OpenQuota", None),
    ]
}

/// The macOS status item menu, with the icon style and what the menu bar shows.
#[cfg(any(target_os = "macos", test))]
pub(crate) fn mac_menu(settings: &AppSettings, menu_bar: MenuBarMenu) -> Vec<TrayEntry> {
    let style = |value, label| {
        choice(
            TraySetting::MenuBarStyle(value),
            label,
            settings.menu_bar_style == value,
        )
    };
    let mut entries = vec![submenu(
        "icon-style",
        "Icon Style",
        vec![
            style(MenuBarStyle::Icon, "OpenQuota"),
            style(MenuBarStyle::Text, "Text"),
            style(MenuBarStyle::Bars, "Bars"),
        ],
    )];
    entries.extend(menu_bar_submenu(menu_bar));
    entries.extend(shared_submenus(settings));
    entries.extend([
        command(TrayAction::Settings, "More Settings…", Some("CmdOrCtrl+,")),
        TrayEntry::Separator,
        launch_at_login(settings),
        command(TrayAction::Quit, "Quit OpenQuota", Some("CmdOrCtrl+Q")),
    ]);
    entries
}

/// Show in Menu Bar, only while the menu bar draws usage. Hiding a metric unstars it, so it
/// leaves this list until it is starred again from Add or the Customize page.
#[cfg(any(target_os = "macos", test))]
fn menu_bar_submenu(menu: MenuBarMenu) -> Option<TrayEntry> {
    if menu.shown.is_empty() {
        return None;
    }
    let mut entries = menu
        .shown
        .into_iter()
        .map(|item| TrayEntry::Item {
            action: TrayAction::HideMenuBarMetric(item.metric_id),
            label: item.label,
            checked: Some(true),
            enabled: true,
            accelerator: None,
        })
        .collect::<Vec<_>>();
    let providers = menu
        .providers
        .into_iter()
        .map(|provider| TrayEntry::Submenu {
            id: format!("menu-bar-add:{}", provider.id),
            label: provider.name,
            enabled: true,
            entries: provider
                .options
                .into_iter()
                .map(|option| TrayEntry::Item {
                    action: TrayAction::Setting(TraySetting::MenuBarMetricPinned {
                        metric_id: option.metric_id,
                        pinned: !option.pinned,
                    }),
                    label: option.label,
                    checked: Some(option.pinned),
                    enabled: option.available,
                    accelerator: None,
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    entries.push(TrayEntry::Separator);
    entries.push(TrayEntry::Submenu {
        id: "menu-bar-add".to_owned(),
        label: "Add".to_owned(),
        enabled: !providers.is_empty(),
        entries: providers,
    });
    Some(submenu("menu-bar-items", "Show in Menu Bar", entries))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::{mac_menu, standard_menu, TrayAction, TrayEntry, FIXED};
    use crate::{
        commands::settings::TraySetting,
        models::{AppSettings, MenuBarStyle, UpdateFrequency, UsageDisplay, WindowMode},
        tray_presentation::{MenuBarItem, MenuBarMenu, MenuBarOption, MenuBarProvider},
    };

    fn settings() -> AppSettings {
        AppSettings {
            window_mode: WindowMode::Floating,
            usage_display: UsageDisplay::Used,
            update_frequency: UpdateFrequency::FiveMinutes,
            menu_bar_style: MenuBarStyle::Bars,
            launch_at_login: true,
            ..AppSettings::default()
        }
    }

    /// Every item and submenu id in `entries`, depth first.
    fn ids(entries: &[TrayEntry]) -> Vec<String> {
        entries
            .iter()
            .flat_map(|entry| match entry {
                TrayEntry::Item { action, .. } => vec![action.id()],
                TrayEntry::Submenu { id, entries, .. } => {
                    let mut ids = vec![id.clone()];
                    ids.extend(self::ids(entries));
                    ids
                }
                TrayEntry::Separator => Vec::new(),
            })
            .collect()
    }

    /// The labels of the checked items in `entries`, depth first.
    fn checked(entries: &[TrayEntry]) -> Vec<String> {
        entries
            .iter()
            .flat_map(|entry| match entry {
                TrayEntry::Item {
                    label,
                    checked: Some(true),
                    ..
                } => vec![label.clone()],
                TrayEntry::Submenu { entries, .. } => self::checked(entries),
                _ => Vec::new(),
            })
            .collect()
    }

    fn labels(entries: &[TrayEntry]) -> Vec<&str> {
        entries
            .iter()
            .map(|entry| match entry {
                TrayEntry::Item { label, .. } | TrayEntry::Submenu { label, .. } => label.as_str(),
                TrayEntry::Separator => "—",
            })
            .collect()
    }

    fn menu_bar() -> MenuBarMenu {
        MenuBarMenu {
            shown: vec![MenuBarItem {
                metric_id: "codex.session".into(),
                label: "Codex Session".into(),
            }],
            providers: vec![MenuBarProvider {
                id: "codex".into(),
                name: "Codex".into(),
                options: vec![
                    MenuBarOption {
                        metric_id: "codex.session".into(),
                        label: "Session".into(),
                        pinned: true,
                        available: true,
                    },
                    MenuBarOption {
                        metric_id: "codex.weekly".into(),
                        label: "Weekly".into(),
                        pinned: false,
                        available: false,
                    },
                ],
            }],
        }
    }

    fn find<'a>(entries: &'a [TrayEntry], wanted: &str) -> &'a TrayEntry {
        fn search<'a>(entries: &'a [TrayEntry], wanted: &str) -> Option<&'a TrayEntry> {
            entries.iter().find_map(|entry| match entry {
                TrayEntry::Item { action, .. } if action.id() == wanted => Some(entry),
                TrayEntry::Submenu { id, .. } if id == wanted => Some(entry),
                TrayEntry::Submenu { entries, .. } => search(entries, wanted),
                _ => None,
            })
        }
        search(entries, wanted).unwrap_or_else(|| panic!("no entry {wanted}"))
    }

    #[test]
    fn every_action_reads_back_from_its_id() {
        let mut actions = FIXED.to_vec();
        actions.extend([
            TrayAction::HideMenuBarMetric("sub2api@2.session".into()),
            TrayAction::Setting(TraySetting::MenuBarMetricPinned {
                metric_id: "codex.weekly".into(),
                pinned: true,
            }),
            TrayAction::Setting(TraySetting::MenuBarMetricPinned {
                metric_id: "codex.weekly".into(),
                pinned: false,
            }),
        ]);
        let mut seen = HashSet::new();
        for action in actions {
            let id = action.id();
            assert!(seen.insert(id.clone()), "{id} is used twice");
            assert_eq!(TrayAction::from_id(&id), Some(action), "{id}");
        }
        for submenu in ["tray-mode", "menu-bar-add", "menu-bar-add:codex", "unknown"] {
            assert_eq!(TrayAction::from_id(submenu), None, "{submenu}");
        }
    }

    #[test]
    fn every_menu_item_reads_back_as_its_action() {
        fn items(entries: &[TrayEntry]) -> Vec<&TrayAction> {
            entries
                .iter()
                .flat_map(|entry| match entry {
                    TrayEntry::Item { action, .. } => vec![action],
                    TrayEntry::Submenu { entries, .. } => items(entries),
                    TrayEntry::Separator => Vec::new(),
                })
                .collect()
        }
        let standard = standard_menu(&settings());
        let mac = mac_menu(&settings(), menu_bar());
        for action in items(&standard).into_iter().chain(items(&mac)) {
            assert_eq!(
                TrayAction::from_id(&action.id()).as_ref(),
                Some(action),
                "{action:?}"
            );
        }
    }

    #[test]
    fn the_standard_menu_opens_the_window_and_checks_the_current_settings() {
        let menu = standard_menu(&settings());
        assert_eq!(
            labels(&menu),
            [
                "Open OpenQuota",
                "Customize…",
                "Tray Mode",
                "Show Usage as",
                "Update Frequency",
                "—",
                "More Settings…",
                "Launch at Login",
                "Quit OpenQuota",
            ]
        );
        assert_eq!(
            checked(&menu),
            ["Floating Window", "Used", "5 minutes", "Launch at Login"]
        );
        assert!(!ids(&menu).iter().any(|id| id.starts_with("icon-style")));
    }

    #[test]
    fn the_mac_menu_adds_the_icon_style_and_shortcuts_but_no_window_entries() {
        let menu = mac_menu(&settings(), MenuBarMenu::default());
        assert_eq!(
            labels(&menu),
            [
                "Icon Style",
                "Tray Mode",
                "Show Usage as",
                "Update Frequency",
                "More Settings…",
                "—",
                "Launch at Login",
                "Quit OpenQuota",
            ]
        );
        assert_eq!(
            checked(&menu),
            [
                "Bars",
                "Floating Window",
                "Used",
                "5 minutes",
                "Launch at Login"
            ]
        );
        let TrayEntry::Item { accelerator, .. } = find(&menu, "quit") else {
            unreachable!()
        };
        assert_eq!(*accelerator, Some("CmdOrCtrl+Q"));
    }

    #[test]
    fn show_in_menu_bar_hides_what_is_shown_and_stars_from_add() {
        let menu = mac_menu(&settings(), menu_bar());
        assert_eq!(labels(&menu)[1], "Show in Menu Bar");
        let all = ids(&menu);
        assert_eq!(
            all.len(),
            all.iter().collect::<HashSet<_>>().len(),
            "menu ids must be unique: {all:?}"
        );
        assert!(matches!(
            find(&menu, "menu-bar-hide:codex.session"),
            TrayEntry::Item {
                checked: Some(true),
                enabled: true,
                ..
            }
        ));
        // A starred option unstars; an unstarred one stars, disabled at the limit.
        assert!(matches!(
            find(&menu, "menu-bar-unpin:codex.session"),
            TrayEntry::Item {
                checked: Some(true),
                enabled: true,
                ..
            }
        ));
        assert!(matches!(
            find(&menu, "menu-bar-pin:codex.weekly"),
            TrayEntry::Item {
                checked: Some(false),
                enabled: false,
                ..
            }
        ));
        assert!(matches!(
            find(&menu, "menu-bar-add"),
            TrayEntry::Submenu { enabled: true, .. }
        ));

        let nothing_to_add = mac_menu(
            &settings(),
            MenuBarMenu {
                providers: Vec::new(),
                ..menu_bar()
            },
        );
        assert!(matches!(
            find(&nothing_to_add, "menu-bar-add"),
            TrayEntry::Submenu { enabled: false, .. }
        ));
    }
}
