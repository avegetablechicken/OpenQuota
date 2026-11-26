/// Option-Command arrows on the dashboard. Left and right move Today, Yesterday,
/// and 30 Days. Up and down move All, Device, and Accounts. Only macOS has them:
/// elsewhere Meta is the Windows or Super key, whose arrows the desktop keeps
/// for window and workspace moves.

import { editsText } from './menuKeys';
import { desktopPlatform, type DesktopPlatform } from './platform';
import type { AppSettings, UsageViewMode } from './types';

export const SPEND_PERIODS = [
  'today',
  'yesterday',
  'last30Days',
] as const satisfies readonly AppSettings['totalSpendPeriod'][];

export const USAGE_VIEW_MODES = [
  'all',
  'localDevice',
  'account',
] as const satisfies readonly UsageViewMode[];

/// Whether `platform` registers the dashboard shortcuts.
export function hasDashboardShortcuts(platform: DesktopPlatform = desktopPlatform()) {
  return platform === 'macos';
}

/// The `aria-keyshortcuts` of the period and scope switchers, or none where the
/// shortcuts are not registered.
export function dashboardKeyshortcuts(platform: DesktopPlatform = desktopPlatform()) {
  return hasDashboardShortcuts(platform)
    ? {
        period: 'Alt+Meta+ArrowLeft Alt+Meta+ArrowRight',
        view: 'Alt+Meta+ArrowUp Alt+Meta+ArrowDown',
      }
    : null;
}

export type DashboardShortcut =
  { kind: 'period'; direction: -1 | 1 } | { kind: 'view'; direction: -1 | 1 };

/// Steps `current` through `items`, wrapping at either end.
export function stepCycle<T>(items: readonly T[], current: T, direction: -1 | 1): T {
  const index = Math.max(0, items.indexOf(current));
  return items[(index + direction + items.length) % items.length];
}

function dialogOpen() {
  return document.querySelector('[role="dialog"], [role="alertdialog"]') !== null;
}

/// The dashboard move `event` asks for, when it is Option-Command and an arrow
/// the page can use. Text fields and open dialogs keep the keys.
export function dashboardShortcut(event: KeyboardEvent): DashboardShortcut | null {
  if (event.defaultPrevented || event.isComposing || event.repeat) return null;
  if (!event.altKey || !event.metaKey || event.ctrlKey || event.shiftKey) return null;
  if (editsText(event.target) || dialogOpen()) return null;
  if (event.key === 'ArrowLeft') return { kind: 'period', direction: -1 };
  if (event.key === 'ArrowRight') return { kind: 'period', direction: 1 };
  if (event.key === 'ArrowUp') return { kind: 'view', direction: -1 };
  if (event.key === 'ArrowDown') return { kind: 'view', direction: 1 };
  return null;
}

let applyDashboardShortcut: ((shortcut: DashboardShortcut) => void) | null = null;

/// The open dashboard registers the switcher it owns. A page that is only
/// finishing its exit transition must not stay registered.
export function bindDashboardShortcuts(apply: (shortcut: DashboardShortcut) => void) {
  applyDashboardShortcut = apply;
  return () => {
    if (applyDashboardShortcut === apply) applyDashboardShortcut = null;
  };
}

/// Uses `event` when the dashboard is open and the shortcut is one of its
/// arrows. Returns whether it did.
export function handleDashboardShortcut(event: KeyboardEvent) {
  const shortcut = dashboardShortcut(event);
  if (!shortcut || !applyDashboardShortcut) return false;
  event.preventDefault();
  event.stopPropagation();
  applyDashboardShortcut(shortcut);
  return true;
}
