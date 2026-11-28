import { cleanup, render, screen } from '@testing-library/svelte';
import { afterEach, expect, it, vi } from 'vitest';
import { codexState, providerCatalogIndex, settingsState } from '../test/appFixtures';
import Dashboard from './Dashboard.svelte';
import { handleDashboardShortcut } from './dashboardShortcuts';
import type { ProviderViewState } from './types';

afterEach(cleanup);

function dashboard(state: ProviderViewState, showTotalSpend = false) {
  const settings = structuredClone(settingsState.settings);
  settings.showTotalSpend = showTotalSpend;
  settings.providers = settings.providers.filter((provider) => provider.id === 'codex');
  settings.providers[0].expanded = true;
  return render(Dashboard, {
    settings,
    catalog: providerCatalogIndex,
    now: Date.parse('2026-07-10T11:00:00Z'),
    renamableProviderIds: [],
    metricPolicies: settingsState.metricPolicies,
    reducedMotion: true,
    viewState: { providers: { codex: state } },
    updateStatus: null,
    installingUpdate: false,
    updateProgress: null,
    updateError: null,
    onSettingsChange: vi.fn(),
    onCustomizationChange: vi.fn(),
    onReorderStart: vi.fn(),
    onReorderEnd: vi.fn(),
    onCustomize: vi.fn(),
    onOpenProviderCustomize: vi.fn(),
    onRenameProvider: vi.fn(),
    onShare: vi.fn(),
    onShareTotal: vi.fn(),
    onRefresh: vi.fn(),
    onOpenProviderLink: vi.fn(),
    onContentMorph: vi.fn(),
    onInstallUpdate: vi.fn(),
    onOpenUpdatePage: vi.fn(),
  });
}

it('shows device-history errors and attribution warnings independently of live quotas', () => {
  const state = structuredClone(codexState);
  state.snapshot!.usageHistories.localDeviceStatus = {
    refreshedAt: '2026-07-10T10:00:00Z',
    error: 'Local usage history could not be refreshed.',
    warnings: ['Model attribution could not be verified.'],
  };
  dashboard(state);
  expect(
    screen.getByRole('status', { name: /Local usage history could not be refreshed/ }),
  ).toHaveTextContent('Model attribution could not be verified.');
  expect(screen.getByRole('region', { name: 'Codex usage' })).toBeInTheDocument();
});

it('uses the device timestamp for a history-only card and keeps data visible after login failure', () => {
  const state = structuredClone(codexState);
  state.error = 'Login expired';
  state.errorKind = 'authentication';
  state.stale = true;
  state.snapshot!.refreshedAt = '1970-01-01T00:00:00Z';
  state.snapshot!.quotas = [];
  state.snapshot!.usageHistories.localDeviceStatus = {
    refreshedAt: '2026-07-10T10:00:00Z',
    error: null,
    warnings: [],
  };
  const { container } = dashboard(state);
  expect(screen.getByRole('alert')).toHaveTextContent('Login expired');
  expect(container.querySelector('[data-tooltip="Last updated 1h ago"]')).toBeInTheDocument();
  expect(screen.getByRole('region', { name: 'Codex usage' })).toHaveTextContent('3.84');
});

it('does not create total spend or bind shortcuts for status-only history', () => {
  const state = structuredClone(codexState);
  state.snapshot!.usageHistories = {
    localDeviceStatus: {
      refreshedAt: null,
      error: 'Logs unreadable',
      warnings: [],
    },
  };
  dashboard(state, true);
  expect(screen.queryByRole('region', { name: 'Total Spend' })).not.toBeInTheDocument();
  expect(
    handleDashboardShortcut(
      new KeyboardEvent('keydown', { key: 'ArrowRight', altKey: true, metaKey: true }),
    ),
  ).toBe(false);
});

it('does not describe the epoch as a successful update', () => {
  const state = structuredClone(codexState);
  state.snapshot!.refreshedAt = '1970-01-01T00:00:00Z';
  state.snapshot!.usageHistories = {
    localDeviceStatus: {
      refreshedAt: null,
      error: 'Logs unreadable',
      warnings: [],
    },
  };
  // Even a stale flag from an older backend must not produce a 1970 elapsed-time tooltip.
  state.stale = true;
  const { container } = dashboard(state);
  expect(
    container.querySelector('[data-tooltip="Last update time unavailable"]'),
  ).toBeInTheDocument();
  expect(container.textContent).not.toMatch(/Last updated \d+h/);
});
