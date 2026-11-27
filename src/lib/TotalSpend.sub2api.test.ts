import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import TotalSpend from './TotalSpend.svelte';
import { chooseMenuItem, menuLabels } from '../test/nativeMenu';
import { ProviderCatalogIndex } from './metrics';
import { clearSub2ApiTrack, setSub2ApiTrack } from '../test/sub2ApiTracks';
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
      displayName: 'Sub2API',
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
    clearSub2ApiTrack('sub2api');
    clearSub2ApiTrack('sub2api@2');
  });

  it('uses upstream-aware names in the ring legend and inclusion note', () => {
    setSub2ApiTrack('sub2api', 'codex');
    setSub2ApiTrack('sub2api@2', 'claude');

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

    expect(screen.getByText('Sub2API')).toBeInTheDocument();
    expect(screen.getByText('Sub2API 2')).toBeInTheDocument();
    expect(
      screen.getByRole('img', {
        name: 'Account-wide history from Sub2API and Sub2API 2.',
      }),
    ).toBeInTheDocument();
    expect(screen.getByText('Accounts')).toBeInTheDocument();
  });

  it('compresses an unsupported account scope while keeping its empty state visible', () => {
    render(TotalSpend, {
      providers: [{ id: 'codex', usageHistories: scopedUsage(100, 'localDevice') }],
      settings,
      catalog: new ProviderCatalogIndex(catalog),
      viewMode: 'all',
      onViewModeChange: vi.fn(),
      onChange: vi.fn(),
      onShare: vi.fn(),
    });

    expect(screen.getByRole('region', { name: 'Accounts Spend' })).toHaveClass(
      'total-card__scope--empty-history',
    );
    expect(screen.getByRole('region', { name: 'Local Device Spend' })).not.toHaveClass(
      'total-card__scope--empty-history',
    );
    expect(screen.getByText('No token data for this period')).toBeInTheDocument();
  });

  it('uses the same compact empty state when history exists outside the selected period', () => {
    render(TotalSpend, {
      providers: [{ id: 'codex', usageHistories: scopedUsage(100, 'localDevice') }],
      settings: { ...settings, totalSpendPeriod: 'today' },
      catalog: new ProviderCatalogIndex(catalog),
      viewMode: 'localDevice',
      onViewModeChange: vi.fn(),
      onChange: vi.fn(),
      onShare: vi.fn(),
    });

    const scope = screen.getByRole('region', { name: 'Local Device Spend' });
    expect(scope).not.toHaveClass('total-card__scope--empty-history');
    expect(scope.querySelector('.total-card__empty')).toHaveTextContent(
      'No token data for this period',
    );
  });

  it('keeps independent Sub2API accounts visually distinct in Accounts and All', async () => {
    setSub2ApiTrack('sub2api', 'codex');
    setSub2ApiTrack('sub2api@2', 'claude');
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
      ['Sub2API', 'Sub2API 2'].map((name) =>
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
    expect(legendColors()[0]).toContain('--provider-sub2api-codex');
    expect(legendColors()[1]).toContain('--provider-sub2api-claude');

    setSub2ApiTrack('sub2api', 'claude');
    await waitFor(() => {
      expect(legendColors()[0]).toContain('--provider-sub2api-claude');
    });
    expect(new Set(legendColors()).size).toBe(2);
    expect(new Set(ringColors()).size).toBe(2);

    await view.rerender({ ...props, viewMode: 'all' });

    expect(new Set(legendColors()).size).toBe(2);
    expect(new Set(ringColors()).size).toBe(2);
  });

  it('switches scopes without combining local and account usage', async () => {
    setSub2ApiTrack('sub2api', 'codex');

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

    const scope = screen.getByRole('button', { name: 'Usage Scope' });
    expect(scope).toHaveTextContent('Device');
    expect(screen.getByText('Codex')).toBeInTheDocument();
    expect(screen.queryByText('Sub2API')).not.toBeInTheDocument();

    await fireEvent.click(scope);
    expect(menuLabels()).toEqual(['All', '✓ Device', 'Accounts']);
    await chooseMenuItem('Accounts');
    expect(onViewModeChange).toHaveBeenCalledWith('account');
    await view.rerender({ ...props, viewMode: 'account' });

    expect(screen.getByRole('button', { name: 'Usage Scope' })).toHaveTextContent('Accounts');
    expect(screen.getByText('Sub2API')).toBeInTheDocument();
    expect(screen.queryByText('Codex')).not.toBeInTheDocument();
    expect(
      screen.getByRole('img', {
        name: 'Account-wide history from Sub2API.',
      }),
    ).toBeInTheDocument();
  });

  it('combines the same model across agents on the priced and unpriced Others tooltips', async () => {
    const providers = [
      {
        id: 'codex',
        model: 'deepseek-flash',
        tokens: 1_000,
        pricedTokens: 400,
        estimatedCostUsd: 2,
        costEstimated: true,
      },
      {
        id: 'claude',
        model: 'deepseek/DeepSeek-Flash',
        tokens: 800,
        pricedTokens: 700,
        estimatedCostUsd: 1,
        costEstimated: false,
      },
    ].map(({ id, model, ...billing }) => ({
      id,
      usageHistories: {
        localDevice: {
          ...usage(100),
          otherUsage: {
            today: { ...billing, estimateComplete: false, models: { [model]: billing } },
            yesterday: null,
            last30Days: null,
          },
        },
      },
    }));
    render(TotalSpend, {
      providers,
      settings: { ...settings, totalSpendMetric: 'cost', totalSpendPeriod: 'today' },
      catalog: new ProviderCatalogIndex(catalog),
      viewMode: 'localDevice',
      onViewModeChange: vi.fn(),
      onChange: vi.fn(),
      onShare: vi.fn(),
    });

    const label = screen.getByText('Others (unpriced)');
    expect(
      label.closest('.spend-legend')?.querySelector('.spend-legend__value--hidden'),
    ).toBeTruthy();
    const arcs = screen
      .getByRole('region', { name: 'Local Device Spend' })
      .querySelectorAll('svg .spend-ring__segment');
    const [priced, unpriced] = screen.getAllByRole('tooltip', { hidden: true });
    await fireEvent.pointerEnter(arcs[0]);
    expect(priced).toHaveClass('spend-billing-tooltip--open');
    expect(priced).toHaveTextContent(/^deepseek-flash ≈ \$3\.00 · 1,100 tokens$/);
    await fireEvent.pointerLeave(arcs[0]);
    await fireEvent.pointerEnter(arcs[1]);
    expect(priced).not.toHaveClass('spend-billing-tooltip--open');
    expect(unpriced).toHaveClass('spend-billing-tooltip--open');
    expect(unpriced).toHaveTextContent(/^deepseek-flash Unpriced · 700 tokens$/);
    expect(unpriced).not.toHaveTextContent('$');
  });

  it('closes a billing tooltip when the period changes under the pointer', async () => {
    const billing = {
      tokens: 1_000,
      pricedTokens: 400,
      estimatedCostUsd: 2,
      costEstimated: true,
    };
    const props = {
      providers: [
        {
          id: 'codex',
          usageHistories: {
            localDevice: {
              ...usage(100),
              otherUsage: {
                today: {
                  ...billing,
                  estimateComplete: false,
                  models: { 'deepseek-flash': billing },
                },
                yesterday: null,
                last30Days: null,
              },
            },
          },
        },
      ],
      settings: {
        ...settings,
        totalSpendMetric: 'cost' as const,
        totalSpendPeriod: 'today' as const,
      },
      catalog: new ProviderCatalogIndex(catalog),
      viewMode: 'localDevice' as const,
      onViewModeChange: vi.fn(),
      onChange: vi.fn(),
      onShare: vi.fn(),
    };
    const { rerender } = render(TotalSpend, props);
    const arc = screen
      .getByRole('region', { name: 'Local Device Spend' })
      .querySelector('svg .spend-ring__segment')!;
    await fireEvent.pointerEnter(arc);
    expect(document.querySelector('.spend-billing-tooltip--open')).not.toBeNull();

    // Yesterday has no Others slice, so the hovered one unmounts without a pointerleave.
    await rerender({ ...props, settings: { ...props.settings, totalSpendPeriod: 'yesterday' } });
    await rerender(props);
    expect(document.querySelector('.spend-billing-tooltip--open')).toBeNull();
  });
});
