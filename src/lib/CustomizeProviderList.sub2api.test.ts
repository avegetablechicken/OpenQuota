import { cleanup, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import CustomizeProviderList from './CustomizeProviderList.svelte';
import { ProviderCatalogIndex } from './metrics';
import { forgetSub2ApiUpstream } from './sub2ApiUpstreams';
import type { AppSettings, ProviderCatalog } from './types';

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));

const catalog: ProviderCatalog = {
  providers: [
    {
      id: 'sub2api@2',
      displayName: 'Sub2API 2',
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
  resetDisplay: 'countdown',
  timeFormat: 'system',
  alwaysShowPacing: false,
  launchAtLogin: false,
  autoCheckUpdates: true,
  dismissedUpdateVersion: null,
  lastUpdateCheckAt: null,
  globalShortcut: null,
  logLevel: 'info',
  notifications: { almostOut: false, cuttingItClose: false, willRunOut: false },
  totalSpendMetric: 'tokens',
  totalSpendPeriod: 'last30Days',
  detectionNoticeDismissed: true,
};

describe('CustomizeProviderList Sub2API labels', () => {
  beforeEach(() => {
    forgetSub2ApiUpstream('sub2api@2');
    mocks.invoke.mockResolvedValue({
      configured: true,
      baseUrl: 'https://192.0.2.8:6060/api/v1',
      email: 'admin@example.com',
      upstream: 'claude',
    });
  });

  afterEach(() => {
    cleanup();
    forgetSub2ApiUpstream('sub2api@2');
    forgetSub2ApiUpstream('sub2api@3');
  });

  it('keeps the metric count when the upstream category is unique', async () => {
    render(CustomizeProviderList, {
      settings,
      catalog: new ProviderCatalogIndex(catalog),
      onOpen: vi.fn(),
      onChange: vi.fn(),
      onReorderStart: vi.fn(),
      onReorderEnd: vi.fn(),
      onSettings: vi.fn(),
      reducedMotion: false,
    });

    expect(await screen.findByText('Sub2API · Claude')).toBeInTheDocument();
    expect(screen.queryByText(/Sub2API 2/)).not.toBeInTheDocument();
    expect(screen.queryByText('192.0.2.8:6060')).not.toBeInTheDocument();
    expect(screen.getByText('2 metrics')).toBeInTheDocument();
    expect(mocks.invoke).toHaveBeenCalledWith('get_sub2api_config_state', {
      providerId: 'sub2api@2',
    });
  });

  it('shows endpoints only when another item uses the same upstream category', async () => {
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
      onOpen: vi.fn(),
      onChange: vi.fn(),
      onReorderStart: vi.fn(),
      onReorderEnd: vi.fn(),
      onSettings: vi.fn(),
      reducedMotion: false,
    });

    expect(await screen.findByText('192.0.2.8:6060')).toBeInTheDocument();
    expect(screen.getByText('claude.example.com')).toBeInTheDocument();
    expect(screen.getAllByText('Sub2API · Claude')).toHaveLength(2);
    expect(screen.queryByText('2 metrics')).not.toBeInTheDocument();
  });
});
