import { describe, expect, it } from 'vitest';
import { codexState, providerCatalog } from '../test/appFixtures';
import { ProviderCatalogIndex, usageSourceNote } from './metrics';

describe('provider catalog index', () => {
  it('indexes provider identity and metric metadata from bootstrap data', () => {
    const catalog = new ProviderCatalogIndex(providerCatalog);

    expect(catalog.displayName('codex')).toBe('Codex');
    expect(catalog.displayName('codex', { codex: '  Work Account  ' })).toBe('Work Account');
    expect(catalog.metric('claude.session')).toMatchObject({
      label: 'Session',
      source: { kind: 'quota', sourceId: 'session', sessionWindow: true },
    });
    expect(catalog.supportsSpend('claude')).toBe(true);
    expect(catalog.supportsSpend('antigravity')).toBe(false);
    expect(catalog.supportsApiKeyConfiguration('openrouter')).toBe(true);
    expect(catalog.supportsApiKeyConfiguration('codex')).toBe(false);
    expect(catalog.metric('openrouter.balance')).toMatchObject({
      label: 'Balance',
      source: { kind: 'value', sourceId: 'balance' },
    });
    expect(catalog.localUsageSourceNote('codex')).toBe('From your Codex logs (estimated)');
    expect(catalog.provider('codex')?.links).toEqual([
      { label: 'Status', url: 'https://status.openai.com/' },
      { label: 'Dashboard', url: 'https://chatgpt.com/codex/settings/usage' },
    ]);
  });

  it('uses safe unknown-provider fallbacks without borrowing another provider identity', () => {
    const catalog = new ProviderCatalogIndex(providerCatalog);

    expect(catalog.displayName('future-provider')).toBe('future-provider');
    expect(catalog.metric('future-provider.session')).toBeUndefined();
    expect(catalog.localUsageSourceNote('future-provider')).toBe(
      'From your future-provider usage history',
    );
  });

  it('prefers the snapshot usage source when an additional local source contributed', () => {
    const catalog = new ProviderCatalogIndex(providerCatalog);
    const snapshot = structuredClone(codexState.snapshot!);
    const history = snapshot.usageHistories.localDevice!;
    history.last30Days!.modelBreakdown = {
      models: [],
      sourceNote: 'From your Codex logs and pi (estimated)',
    };

    expect(usageSourceNote(catalog, snapshot, history, 'localDevice')).toBe(
      'From your Codex logs and pi (estimated)',
    );
    history.last30Days!.modelBreakdown = null;
    history.today!.modelBreakdown = null;
    history.yesterday!.modelBreakdown = null;
    expect(usageSourceNote(catalog, snapshot, history, 'localDevice')).toBe(
      'From your Codex logs (estimated)',
    );
  });

  it('uses the resolved provider name for fallback usage notes', () => {
    const catalog = new ProviderCatalogIndex({
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
    });
    const snapshot = {
      ...codexState.snapshot!,
      providerId: 'sub2api@2',
      usageHistories: {
        localDevice: {
          ...codexState.snapshot!.usageHistories.localDevice!,
          today: null,
          yesterday: null,
          last30Days: null,
        },
      },
    };

    expect(
      usageSourceNote(
        catalog,
        snapshot,
        snapshot.usageHistories.localDevice!,
        'localDevice',
        'Sub2API 2',
      ),
    ).toBe('From your Sub2API 2 usage history');
  });

  it('uses resolved provider names in provider messages', () => {
    const catalog = new ProviderCatalogIndex({
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
          displayName: 'Sub2API',
          shortName: 'S2',
          fallbackEnabled: false,
          localUsageSourceNote: null,
          links: [],
          metrics: [],
        },
        {
          id: 'codex',
          displayName: 'Codex',
          shortName: 'Cx',
          fallbackEnabled: false,
          localUsageSourceNote: null,
          links: [],
          metrics: [],
        },
      ],
    });

    expect(
      catalog.displayMessage(
        'sub2api@2',
        'Could not reach Sub2API. Check the Base URL and connection.',
        'Team gateway',
      ),
    ).toBe('Could not reach Team gateway. Check the Base URL and connection.');
    expect(catalog.displayMessage('codex', 'Could not connect to Codex.', 'Work')).toBe(
      'Could not connect to Work.',
    );
  });

  it('resolves custom and upstream-aware provider names in one place', () => {
    const catalog = new ProviderCatalogIndex({
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
          id: 'codex',
          displayName: 'Codex',
          shortName: 'Cx',
          fallbackEnabled: false,
          localUsageSourceNote: null,
          links: [],
          metrics: [],
        },
      ],
    });

    expect(catalog.resolvedDisplayName('sub2api')).toBe('Sub2API');
    expect(catalog.resolvedDisplayName('sub2api', { sub2api: 'Team gateway' })).toBe(
      'Team gateway',
    );
    expect(catalog.resolvedDisplayName('codex', { codex: 'Work' })).toBe('Work');
  });

  it('keeps the catalog slot name until an upstream is known', () => {
    const catalog = new ProviderCatalogIndex({
      providers: [
        {
          id: 'sub2api@2',
          displayName: 'Unconfigured slot',
          shortName: 'S2',
          fallbackEnabled: false,
          localUsageSourceNote: null,
          links: [],
          metrics: [],
        },
      ],
    });

    expect(catalog.resolvedDisplayName('sub2api@2')).toBe(`Sub2API ${2}`);
  });

  it('rejects duplicate provider and metric ids at the frontend boundary', () => {
    const provider = structuredClone(providerCatalog.providers[1]);
    expect(
      () => new ProviderCatalogIndex({ providers: [provider, structuredClone(provider)] }),
    ).toThrow('Duplicate provider definition: codex');

    const duplicateMetric = structuredClone(provider);
    duplicateMetric.metrics.push(structuredClone(duplicateMetric.metrics[0]));
    expect(() => new ProviderCatalogIndex({ providers: [duplicateMetric] })).toThrow(
      'Duplicate metric definition: codex.session',
    );
  });
});
