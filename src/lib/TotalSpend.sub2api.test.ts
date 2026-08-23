import { cleanup, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import TotalSpend from './TotalSpend.svelte';
import { ProviderCatalogIndex } from './metrics';
import { forgetSub2ApiUpstream, rememberSub2ApiUpstream } from './sub2ApiUpstreams';
import type { AppSettings, ProviderCatalog, UsageHistory } from './types';

const catalog: ProviderCatalog = {
  providers: [
    {
      id: 'sub2api',
      displayName: 'Sub2API',
      shortName: 'S2',
      fallbackEnabled: false,
      localUsageSourceNote: null,
      links: [],
      metrics: [],
    },
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
};

const settings: AppSettings = {
  schemaVersion: 7,
  providerNames: {},
  knownProviderIds: ['sub2api', 'sub2api@2'],
  providers: [],
  showTotalSpend: true,
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

function usage(tokens: number): UsageHistory {
  return {
    today: null,
    yesterday: null,
    last30Days: {
      tokens,
      estimatedCostUsd: null,
      costEstimated: false,
      estimateComplete: true,
      modelBreakdown: null,
      unknownModels: [],
    },
    daily: [],
    unknownModels: [],
  };
}

describe('TotalSpend Sub2API labels', () => {
  afterEach(() => {
    cleanup();
    forgetSub2ApiUpstream('sub2api');
    forgetSub2ApiUpstream('sub2api@2');
  });

  it('uses upstream-aware names in the ring legend and inclusion note', () => {
    rememberSub2ApiUpstream('sub2api', 'codex');
    rememberSub2ApiUpstream('sub2api@2', 'claude');

    render(TotalSpend, {
      providers: [
        { id: 'sub2api', usage: usage(100) },
        { id: 'sub2api@2', usage: usage(200) },
      ],
      settings,
      catalog: new ProviderCatalogIndex(catalog),
      onChange: vi.fn(),
      onShare: vi.fn(),
    });

    expect(screen.getByText('Sub2API · Codex')).toBeInTheDocument();
    expect(screen.getByText('Sub2API · Claude')).toBeInTheDocument();
    expect(
      screen.getByRole('img', {
        name: 'Only includes Sub2API · Codex and Sub2API · Claude',
      }),
    ).toBeInTheDocument();
    expect(screen.queryByText('Sub2API 2')).not.toBeInTheDocument();
  });
});
