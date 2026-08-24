import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import TotalSpend from './TotalSpend.svelte';
import { ProviderCatalogIndex } from './metrics';
import { forgetSub2ApiUpstream, rememberSub2ApiUpstream } from './sub2ApiUpstreams';
import type { AppSettings, ProviderCatalog, UsageHistory } from './types';

const catalog: ProviderCatalog = {
  providers: [
    {
      id: 'codex',
      displayName: 'Codex',
      shortName: 'Cx',
      fallbackEnabled: true,
      localUsageSourceNote: 'From your Codex logs (estimated)',
      links: [],
      metrics: [],
    },
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

function usage(tokens: number, scope: UsageHistory['scope'] = 'account'): UsageHistory {
  return {
    scope,
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
        name: 'Account-wide history from Sub2API · Codex and Sub2API · Claude. Local-device usage is excluded.',
      }),
    ).toBeInTheDocument();
    expect(screen.getByText('Accounts')).toBeInTheDocument();
    expect(screen.queryByText('Sub2API 2')).not.toBeInTheDocument();
  });

  it('switches scopes without combining local and account usage', async () => {
    rememberSub2ApiUpstream('sub2api', 'codex');

    render(TotalSpend, {
      providers: [
        { id: 'codex', usage: usage(100, 'localDevice') },
        { id: 'sub2api', usage: usage(200) },
      ],
      settings,
      catalog: new ProviderCatalogIndex(catalog),
      onChange: vi.fn(),
      onShare: vi.fn(),
    });

    expect(screen.getByRole('button', { name: 'Device' })).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByText('Codex')).toBeInTheDocument();
    expect(screen.queryByText('Sub2API · Codex')).not.toBeInTheDocument();

    await fireEvent.click(screen.getByRole('button', { name: 'Accounts' }));

    expect(screen.getByRole('button', { name: 'Accounts' })).toHaveAttribute(
      'aria-pressed',
      'true',
    );
    expect(screen.getByText('Sub2API · Codex')).toBeInTheDocument();
    expect(screen.queryByText('Codex')).not.toBeInTheDocument();
    expect(
      screen.getByRole('img', {
        name: 'Account-wide history from Sub2API · Codex. Local-device usage is excluded.',
      }),
    ).toBeInTheDocument();
  });
});
