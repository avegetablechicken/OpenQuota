import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import TotalSpend from './TotalSpend.svelte';
import { ProviderCatalogIndex } from './metrics';
import { forgetSub2ApiUpstream, rememberSub2ApiUpstream } from './sub2ApiUpstreams';
import type { AppSettings, ProviderCatalog, UsageHistory, UsageScope } from './types';

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

function scopedUsage(tokens: number, scope: UsageScope = 'account') {
  return { [scope]: usage(tokens) };
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
        { id: 'sub2api', usageHistories: scopedUsage(100) },
        { id: 'sub2api@2', usageHistories: scopedUsage(200) },
      ],
      settings,
      catalog: new ProviderCatalogIndex(catalog),
      viewMode: 'account',
      onViewModeChange: vi.fn(),
      onChange: vi.fn(),
      onShare: vi.fn(),
    });

    expect(screen.getByText('Sub2API · Codex')).toBeInTheDocument();
    expect(screen.getByText('Sub2API · Claude')).toBeInTheDocument();
    expect(
      screen.getByRole('img', {
        name: 'Account-wide history from Sub2API · Codex and Sub2API · Claude.',
      }),
    ).toBeInTheDocument();
    expect(screen.getByText('Accounts')).toBeInTheDocument();
    expect(screen.queryByText('Sub2API 2')).not.toBeInTheDocument();
  });

  it('keeps independent Sub2API accounts visually distinct in Accounts and All', async () => {
    rememberSub2ApiUpstream('sub2api', 'codex');
    rememberSub2ApiUpstream('sub2api@2', 'claude');
    const props = {
      providers: [
        { id: 'sub2api', usageHistories: scopedUsage(100) },
        { id: 'sub2api@2', usageHistories: scopedUsage(200) },
      ],
      settings,
      catalog: new ProviderCatalogIndex(catalog),
      viewMode: 'account' as const,
      onViewModeChange: vi.fn(),
      onChange: vi.fn(),
      onShare: vi.fn(),
    };
    const view = render(TotalSpend, props);

    const legendColors = () =>
      ['Sub2API · Codex', 'Sub2API · Claude'].map((name) =>
        screen.getByText(name).querySelector('i')?.getAttribute('style'),
      );
    const ringColors = () =>
      Array.from(
        screen
          .getByRole('region', { name: 'Accounts Spend' })
          .querySelectorAll('.spend-ring__segment'),
        (segment) => segment.getAttribute('style'),
      );

    expect(new Set(legendColors()).size).toBe(2);
    expect(new Set(ringColors()).size).toBe(2);

    await view.rerender({ ...props, viewMode: 'all' });

    expect(new Set(legendColors()).size).toBe(2);
    expect(new Set(ringColors()).size).toBe(2);
  });

  it('switches scopes without combining local and account usage', async () => {
    rememberSub2ApiUpstream('sub2api', 'codex');

    const onViewModeChange = vi.fn();
    const props = {
      providers: [
        { id: 'codex', usageHistories: scopedUsage(100, 'localDevice') },
        { id: 'sub2api', usageHistories: scopedUsage(200) },
      ],
      settings,
      catalog: new ProviderCatalogIndex(catalog),
      viewMode: 'localDevice' as const,
      onViewModeChange,
      onChange: vi.fn(),
      onShare: vi.fn(),
    };
    const view = render(TotalSpend, props);

    expect(screen.getByRole('button', { name: 'Device' })).toHaveAttribute('aria-pressed', 'true');
    expect(screen.getByText('Codex')).toBeInTheDocument();
    expect(screen.queryByText('Sub2API · Codex')).not.toBeInTheDocument();

    await fireEvent.click(screen.getByRole('button', { name: 'Accounts' }));
    expect(onViewModeChange).toHaveBeenCalledWith('account');
    await view.rerender({ ...props, viewMode: 'account' });

    expect(screen.getByRole('button', { name: 'Accounts' })).toHaveAttribute(
      'aria-pressed',
      'true',
    );
    expect(screen.getByText('Sub2API · Codex')).toBeInTheDocument();
    expect(screen.queryByText('Codex')).not.toBeInTheDocument();
    expect(
      screen.getByRole('img', {
        name: 'Account-wide history from Sub2API · Codex.',
      }),
    ).toBeInTheDocument();
  });
});
