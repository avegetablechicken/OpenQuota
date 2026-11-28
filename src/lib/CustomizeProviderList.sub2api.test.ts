import { cleanup, render, screen, waitFor, within } from '@testing-library/svelte';
import { get } from 'svelte/store';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import CustomizeProviderList from './CustomizeProviderList.svelte';
import { ProviderCatalogIndex } from './metrics';
import {
  forgetSub2ApiEndpoint,
  rememberSub2ApiEndpoint,
  sub2ApiEndpoints,
  syncSub2ApiTracks,
} from './sub2ApiUpstreams';
import type { AppSettings, MetricPolicy, ProviderCatalog } from './types';

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));

const catalog: ProviderCatalog = {
  providers: [
    {
      id: 'sub2api@2',
      displayName: 'Sub2API',
      shortName: 'S2',
      fallbackEnabled: false,
      localUsageSourceNote: null,
      links: [],
      metrics: [],
    },
  ],
  connectionConfigProviderIds: ['sub2api@2'],
};

const settings: AppSettings = {
  schemaVersion: 7,
  providerNames: {},
  knownProviderIds: ['sub2api@2'],
  providers: [
    {
      id: 'sub2api@2',
      enabled: true,
      detected: true,
      expanded: false,
      metrics: [
        { id: 'sub2api@2.session', enabled: true, section: 'alwaysVisible', pinned: true },
        { id: 'sub2api@2.sonnet', enabled: false, section: 'onDemand', pinned: false },
        { id: 'sub2api@2.spark', enabled: false, section: 'onDemand', pinned: false },
        { id: 'sub2api@2.sparkWeekly', enabled: false, section: 'onDemand', pinned: false },
        { id: 'sub2api@2.rateLimitResets', enabled: false, section: 'onDemand', pinned: false },
        { id: 'sub2api@2.extra', enabled: false, section: 'onDemand', pinned: false },
      ],
    },
  ],
  showTotalSpend: false,
  theme: 'system',
  density: 'default',
  reduceAnimations: false,
  windowMode: 'popup',
  menuBarStyle: 'icon',
  usageDisplay: 'used',
  updateFrequency: 'adaptive',
  resetDisplay: 'countdown',
  timeFormat: 'system',
  alwaysShowPacing: false,
  launchAtLogin: false,
  autoCheckUpdates: true,
  dismissedUpdateVersion: null,
  lastUpdateCheckAt: null,
  globalShortcut: null,
  logLevel: 'info',
  notifications: {
    almostOut: false,
    cuttingItClose: false,
    willRunOut: false,
    fullyUsed: false,
    usageReset: false,
  },
  totalSpendMetric: 'tokens',
  totalSpendPeriod: 'last30Days',
  detectionNoticeDismissed: true,
};

function policy(track: MetricPolicy['track']): MetricPolicy {
  return { track, visibleMetricIds: [], starrableMetricIds: [], maxStars: 2 };
}

const claudeRelays = { 'sub2api@2': policy('claude'), 'sub2api@3': policy('claude') };

describe('CustomizeProviderList Sub2API labels', () => {
  beforeEach(() => {
    mocks.invoke.mockReset();
    forgetSub2ApiEndpoint('sub2api@2');
    syncSub2ApiTracks(claudeRelays);
    mocks.invoke.mockResolvedValue({
      configured: true,
      baseUrl: 'https://192.0.2.8:6060/api/v1',
      email: 'admin@example.com',
      upstream: 'claude',
    });
  });

  afterEach(() => {
    cleanup();
    forgetSub2ApiEndpoint('sub2api@2');
    forgetSub2ApiEndpoint('sub2api@3');
    syncSub2ApiTracks({});
  });

  it.each([true, false])(
    'shows the base URL for a unique upstream (enabled: %s)',
    async (enabled) => {
      render(CustomizeProviderList, {
        settings: {
          ...settings,
          providers: settings.providers.map((provider) => ({ ...provider, enabled })),
        },
        catalog: new ProviderCatalogIndex(catalog),
        metricPolicies: claudeRelays,
        onOpen: vi.fn(),
        onChange: vi.fn(),
        onReorderStart: vi.fn(),
        onReorderEnd: vi.fn(),
        onSettings: vi.fn(),
        reducedMotion: false,
      });

      expect(await screen.findByText('Sub2API · Claude')).toBeInTheDocument();
      expect(
        within(screen.getByRole('group', { name: 'Self-hostable providers' })).getByText(
          'Sub2API · Claude',
        ),
      ).toBeInTheDocument();
      expect(await screen.findByText('Claude relay · 192.0.2.8:6060')).toBeInTheDocument();
      expect(screen.queryByText(/Sub2API \d/)).not.toBeInTheDocument();
      expect(screen.queryByText(/\d+ metrics/)).not.toBeInTheDocument();
      expect(mocks.invoke).toHaveBeenCalledWith('get_sub2api_config_state', {
        providerId: 'sub2api@2',
      });
    },
  );

  it('uses a neutral name until the upstream is configured', async () => {
    mocks.invoke.mockResolvedValue({
      configured: false,
      baseUrl: '',
      email: '',
      upstream: 'codex',
    });
    const unconfigured = { 'sub2api@2': policy(null) };
    syncSub2ApiTracks(unconfigured);
    render(CustomizeProviderList, {
      settings,
      catalog: new ProviderCatalogIndex(catalog),
      metricPolicies: unconfigured,
      onOpen: vi.fn(),
      onChange: vi.fn(),
      onReorderStart: vi.fn(),
      onReorderEnd: vi.fn(),
      onSettings: vi.fn(),
      reducedMotion: false,
    });

    expect(await screen.findByText('Account name')).toBeInTheDocument();
    expect(screen.getByText('Not configured')).toBeInTheDocument();
    expect(screen.queryByText(/Sub2API/)).not.toBeInTheDocument();
  });

  it('shows endpoints for every configuration sharing an upstream category', async () => {
    const duplicateCatalog = structuredClone(catalog);
    duplicateCatalog.providers.push({
      ...duplicateCatalog.providers[0],
      id: 'sub2api@3',
      displayName: 'Sub2API 3',
    });
    duplicateCatalog.connectionConfigProviderIds?.push('sub2api@3');
    const duplicateSettings = structuredClone(settings);
    duplicateSettings.providers.push({
      ...duplicateSettings.providers[0],
      id: 'sub2api@3',
      metrics: duplicateSettings.providers[0].metrics.map((metric) => ({
        ...metric,
        id: metric.id.replace('sub2api@2.', 'sub2api@3.'),
      })),
    });
    duplicateSettings.knownProviderIds.push('sub2api@3');
    mocks.invoke.mockImplementation((_command, args: { providerId: string }) =>
      Promise.resolve({
        configured: true,
        baseUrl:
          args.providerId === 'sub2api@2'
            ? 'https://192.0.2.8:6060/api/v1'
            : 'https://claude.example.com',
        email: 'admin@example.com',
        upstream: 'claude',
      }),
    );

    render(CustomizeProviderList, {
      settings: duplicateSettings,
      catalog: new ProviderCatalogIndex(duplicateCatalog),
      metricPolicies: claudeRelays,
      onOpen: vi.fn(),
      onChange: vi.fn(),
      onReorderStart: vi.fn(),
      onReorderEnd: vi.fn(),
      onSettings: vi.fn(),
      reducedMotion: false,
    });

    expect(await screen.findByText('Claude relay · 192.0.2.8:6060')).toBeInTheDocument();
    expect(screen.getByText('Claude relay · claude.example.com')).toBeInTheDocument();
    expect(screen.getAllByText('Sub2API · Claude')).toHaveLength(2);
    expect(screen.queryByText(/Sub2API \d/)).not.toBeInTheDocument();
    expect(screen.queryByText('2 metrics')).not.toBeInTheDocument();
  });

  it('still reads a disabled connection when the endpoint is cached but its track is unknown', async () => {
    rememberSub2ApiEndpoint('sub2api@2', 'https://192.0.2.8:6060/api/v1');
    const unknownTrack = { 'sub2api@2': policy(null) };
    syncSub2ApiTracks(unknownTrack);

    render(CustomizeProviderList, {
      settings: {
        ...settings,
        providers: settings.providers.map((provider) => ({ ...provider, enabled: false })),
      },
      catalog: new ProviderCatalogIndex(catalog),
      metricPolicies: unknownTrack,
      onOpen: vi.fn(),
      onChange: vi.fn(),
      onReorderStart: vi.fn(),
      onReorderEnd: vi.fn(),
      onSettings: vi.fn(),
      reducedMotion: false,
    });

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('get_sub2api_config_state', {
        providerId: 'sub2api@2',
      }),
    );
  });

  it('skips reading a connection when both its track and endpoint are already known', () => {
    rememberSub2ApiEndpoint('sub2api@2', 'https://192.0.2.8:6060/api/v1');

    render(CustomizeProviderList, {
      settings,
      catalog: new ProviderCatalogIndex(catalog),
      metricPolicies: claudeRelays,
      onOpen: vi.fn(),
      onChange: vi.fn(),
      onReorderStart: vi.fn(),
      onReorderEnd: vi.fn(),
      onSettings: vi.fn(),
      reducedMotion: false,
    });

    expect(screen.getByText('Claude relay · 192.0.2.8:6060')).toBeInTheDocument();
    expect(mocks.invoke).not.toHaveBeenCalled();
  });

  it('forgets a stale cached endpoint when the backend reports the connection is not configured', async () => {
    rememberSub2ApiEndpoint('sub2api@2', 'https://192.0.2.8:6060/api/v1');
    const unknownTrack = { 'sub2api@2': policy(null) };
    syncSub2ApiTracks(unknownTrack);
    mocks.invoke.mockResolvedValue({
      configured: false,
      baseUrl: '',
      email: '',
      upstream: 'codex',
    });

    render(CustomizeProviderList, {
      settings,
      catalog: new ProviderCatalogIndex(catalog),
      metricPolicies: unknownTrack,
      onOpen: vi.fn(),
      onChange: vi.fn(),
      onReorderStart: vi.fn(),
      onReorderEnd: vi.fn(),
      onSettings: vi.fn(),
      reducedMotion: false,
    });

    await waitFor(() => expect(get(sub2ApiEndpoints)['sub2api@2']).toBeUndefined());
  });
});
