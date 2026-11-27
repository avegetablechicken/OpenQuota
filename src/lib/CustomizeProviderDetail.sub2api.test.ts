import { cleanup, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import CustomizeProviderDetail from './CustomizeProviderDetail.svelte';
import { ProviderCatalogIndex } from './metrics';
import { syncSub2ApiTracks } from './sub2ApiUpstreams';
import type { AppSettings, MetricDefinition, MetricPolicy, ProviderCatalog } from './types';

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));

function quota(id: string, label: string): MetricDefinition {
  return {
    id: `sub2api@2.${id}`,
    label,
    source: { kind: 'quota', sourceId: id, sessionWindow: false },
    pinnable: true,
    defaultEnabled: true,
    defaultSection: 'onDemand',
    defaultPinned: false,
    tray: { shortLabel: label.slice(0, 2), suffix: null },
  };
}

const definitions = [
  quota('session', 'Session'),
  quota('sonnet', 'Sonnet'),
  quota('fable', 'Fable'),
  quota('spark', 'Spark'),
  quota('sparkWeekly', 'Spark Weekly'),
  {
    ...quota('extra', 'Extra Usage'),
    source: { kind: 'quotaOrValue' as const, sourceId: 'extra', sessionWindow: false },
  },
  {
    ...quota('rateLimitResets', 'Rate Limit Resets'),
    source: { kind: 'value' as const, sourceId: 'sub2apiRateLimitResets' },
  },
];

const catalog: ProviderCatalog = {
  providers: [
    {
      id: 'sub2api@2',
      displayName: 'Sub2API',
      shortName: 'S2',
      fallbackEnabled: false,
      localUsageSourceNote: null,
      links: [],
      metrics: definitions,
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
      metrics: definitions.map((metric) => ({
        id: metric.id,
        enabled: false,
        section: metric.id.endsWith('.session') ? 'alwaysVisible' : 'onDemand',
        pinned: false,
      })),
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

const visible = {
  codex: ['session', 'spark', 'sparkWeekly', 'rateLimitResets'],
  claude: ['session', 'sonnet', 'fable'],
};

function renderDetail(upstream: 'codex' | 'claude', renamableProviderIds: string[] = []) {
  const ids = visible[upstream].map((suffix) => `sub2api@2.${suffix}`);
  const metricPolicy: MetricPolicy = {
    track: upstream,
    visibleMetricIds: ids,
    starrableMetricIds: ids,
    maxStars: 2,
  };
  syncSub2ApiTracks({ 'sub2api@2': metricPolicy });
  mocks.invoke.mockResolvedValue({
    configured: true,
    baseUrl: 'https://sub2api.example.com',
    email: 'admin@example.com',
    upstream,
  });
  render(CustomizeProviderDetail, {
    settings,
    providerId: 'sub2api@2',
    catalog: new ProviderCatalogIndex(catalog),
    renamableProviderIds,
    metricPolicy,
    onChange: vi.fn(),
    onNameChange: vi.fn(),
    onReorderStart: vi.fn(),
    onReorderEnd: vi.fn(),
    reducedMotion: false,
  });
}

describe('CustomizeProviderDetail Sub2API metric availability', () => {
  afterEach(() => {
    cleanup();
    syncSub2ApiTracks({});
  });

  it('hides Codex-only metrics for a Claude upstream', () => {
    renderDetail('claude');

    expect(screen.queryByRole('checkbox', { name: 'Show Spark' })).not.toBeInTheDocument();
    expect(screen.queryByRole('checkbox', { name: 'Show Spark Weekly' })).not.toBeInTheDocument();
    expect(
      screen.queryByRole('checkbox', { name: 'Show Rate Limit Resets' }),
    ).not.toBeInTheDocument();
    expect(screen.getByRole('checkbox', { name: 'Show Sonnet' })).toBeEnabled();
    expect(screen.getByRole('checkbox', { name: 'Show Fable' })).toBeEnabled();
    expect(screen.queryByRole('checkbox', { name: 'Show Extra Usage' })).not.toBeInTheDocument();
    expect(screen.queryByRole('heading', { name: 'Name' })).not.toBeInTheDocument();
  });

  it('shows the name row only for a configured Sub2API slot admitted by the backend', () => {
    renderDetail('codex', ['sub2api@2']);

    expect(screen.getByRole('heading', { name: 'Name' })).toBeInTheDocument();
    expect(screen.getByRole('textbox', { name: 'Name for Sub2API · Codex' })).toHaveAttribute(
      'placeholder',
      'Sub2API · Codex',
    );
    expect(screen.queryByText(/Sub2API \d/)).not.toBeInTheDocument();
  });

  it('hides Claude-only metrics for a Codex upstream', () => {
    renderDetail('codex');

    expect(screen.queryByRole('checkbox', { name: 'Show Sonnet' })).not.toBeInTheDocument();
    expect(screen.queryByRole('checkbox', { name: 'Show Fable' })).not.toBeInTheDocument();
    expect(screen.getByRole('checkbox', { name: 'Show Spark' })).toBeEnabled();
    expect(screen.getByRole('checkbox', { name: 'Show Spark Weekly' })).toBeEnabled();
    expect(screen.getByRole('checkbox', { name: 'Show Rate Limit Resets' })).toBeEnabled();
    expect(screen.queryByRole('checkbox', { name: 'Show Extra Usage' })).not.toBeInTheDocument();
  });
});
