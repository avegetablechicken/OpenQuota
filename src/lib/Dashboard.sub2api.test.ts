import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, expect, it, vi } from 'vitest';
import { settingsState } from '../test/appFixtures';
import { chooseMenuItem, openMenu } from '../test/nativeMenu';
import Dashboard from './Dashboard.svelte';
import { ProviderCatalogIndex } from './metrics';
import { buildProviderShareRows } from './shareCard';
import { forgetSub2ApiUpstream, rememberSub2ApiUpstream } from './sub2ApiUpstreams';
import type { MetricDefinition, ProviderSnapshot, Sub2ApiSource } from './types';

afterEach(() => {
  cleanup();
  forgetSub2ApiUpstream('sub2api');
});

function dashboard(source: Sub2ApiSource = 'subscription', pins = ['session', 'spark']) {
  // A cached label may still describe the previous track; the live snapshot owns its data.
  rememberSub2ApiUpstream('sub2api', 'codex', 'https://example.com');
  const definitions: MetricDefinition[] = ['session', 'spark', 'weekly', 'daily', 'sonnet'].map(
    (id) => ({
      id: `sub2api.${id}`,
      label: id,
      source: { kind: 'quota', sourceId: id, sessionWindow: id === 'session' },
      pinnable: true,
      defaultEnabled: true,
      defaultSection: 'alwaysVisible',
      defaultPinned: false,
      tray: { shortLabel: id, suffix: null },
    }),
  );
  const catalog = new ProviderCatalogIndex({
    providers: [
      {
        id: 'sub2api',
        displayName: 'Sub2API',
        shortName: 'S2',
        fallbackEnabled: false,
        localUsageSourceNote: null,
        links: [],
        metrics: definitions,
      },
    ],
    connectionConfigProviderIds: ['sub2api'],
  });
  const settings = structuredClone(settingsState.settings);
  settings.showTotalSpend = false;
  settings.providers = [
    {
      id: 'sub2api',
      enabled: true,
      detected: true,
      expanded: true,
      metrics: definitions.map((d) => ({
        id: d.id,
        enabled: true,
        section: 'alwaysVisible',
        pinned: pins.includes(d.source.kind === 'quota' ? d.source.sourceId : ''),
      })),
    },
  ];
  const snapshot: ProviderSnapshot = {
    providerId: 'sub2api',
    upstreamProviderId: source === 'subscription' ? 'codex' : source,
    plan: 'Plan',
    quotas: [
      {
        id: 'weekly',
        label: 'weekly',
        usedPercent: 20,
        resetsAt: null,
        periodSeconds: 604800,
        format: 'percent',
        usedValue: 2,
        limitValue: 10,
        estimated: false,
      },
    ],
    valueMetrics: [],
    statusMetrics: [],
    notices: [],
    warnings: [],
    usageHistories: {},
    refreshedAt: '2026-11-27T00:00:00Z',
  };
  if (source !== 'subscription') {
    snapshot.accounts = [{ id: '1', name: 'Account', snapshot: { ...snapshot } }];
  }
  const onCustomizationChange = vi.fn();
  const now = Date.parse(snapshot.refreshedAt);
  const rendered = render(Dashboard, {
    settings,
    catalog,
    now,
    renamableProviderIds: [],
    reducedMotion: true,
    viewState: {
      providers: {
        sub2api: {
          snapshot,
          source: 'live',
          stale: false,
          refreshing: false,
          error: null,
          errorKind: null,
          lastAttemptAt: null,
        },
      },
    },
    updateStatus: null,
    installingUpdate: false,
    updateProgress: null,
    updateError: null,
    onSettingsChange: vi.fn(),
    onCustomizationChange,
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
  return { ...rendered, settings, snapshot, catalog, now, onCustomizationChange };
}

it('hides retained admin metrics in subscriptions without changing saved preferences', () => {
  const { settings, onCustomizationChange } = dashboard();
  expect(screen.queryByRole('group', { name: 'session options' })).not.toBeInTheDocument();
  expect(screen.queryByRole('group', { name: 'spark options' })).not.toBeInTheDocument();
  expect(screen.queryByRole('group', { name: 'sonnet options' })).not.toBeInTheDocument();
  expect(screen.getByRole('group', { name: 'weekly options' })).toBeInTheDocument();
  // Supported quotas remain visible even when the server has not supplied their data yet.
  expect(screen.getByRole('group', { name: 'daily options' })).toHaveTextContent('No data');
  expect(settings.providers[0].metrics[0]).toMatchObject({ enabled: true, pinned: true });
  expect(onCustomizationChange).not.toHaveBeenCalled();
});

it('uses the current admin upstream to hide retained quotas of a different upstream', () => {
  dashboard('claude');
  expect(screen.queryByRole('group', { name: 'spark options' })).not.toBeInTheDocument();
  expect(screen.queryByRole('group', { name: 'daily options' })).not.toBeInTheDocument();
  expect(screen.getByRole('group', { name: 'sonnet options' })).toBeInTheDocument();
  expect(screen.getByRole('group', { name: 'session options' })).toBeInTheDocument();
});

it('does not let unsupported retained pins consume the subscription star limit', async () => {
  const { onCustomizationChange } = dashboard();
  await fireEvent.contextMenu(screen.getByRole('group', { name: 'weekly options' }));
  await chooseMenuItem('Star for menu bar');
  const updated = onCustomizationChange.mock.calls[0][0];
  expect(
    updated.providers[0].metrics.find((m: { id: string }) => m.id === 'sub2api.weekly'),
  ).toMatchObject({ pinned: true });
  expect(
    updated.providers[0].metrics.find((m: { id: string }) => m.id === 'sub2api.session'),
  ).toMatchObject({ pinned: true });
});

it('still enforces the limit for two supported admin pins and allows removing a star', async () => {
  dashboard('claude', ['session', 'sonnet']);
  await fireEvent.contextMenu(screen.getByRole('group', { name: 'weekly options' }));
  expect(
    openMenu()?.entries.find((item) => item !== 'separator' && item.text === 'Star for menu bar'),
  ).toMatchObject({ enabled: false });
  await fireEvent.contextMenu(screen.getByRole('group', { name: 'session options' }));
  await chooseMenuItem('Unstar');
});

it.each(['subscription', 'claude'] as const)('shares only supported %s metrics', (source) => {
  const { settings, snapshot, catalog, now } = dashboard(source);
  const rows = buildProviderShareRows(
    catalog,
    snapshot.accounts?.[0].snapshot ?? snapshot,
    settings.providers[0],
    settings,
    now,
    'all',
    source === 'subscription',
  );
  const labels = rows.map((row) => row.label);
  expect(labels).not.toContain('spark');
  expect(labels).toContain('weekly');
  expect(labels).toContain(source === 'subscription' ? 'daily' : 'sonnet');
});
