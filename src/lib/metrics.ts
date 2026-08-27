import type {
  MetricDefinition,
  ProviderCatalog,
  ProviderDefinition,
  ProviderSnapshot,
  UsageHistory,
  UsageScope,
} from './types';
import { sub2ApiDisplayName } from './sub2ApiUpstreams';

export class ProviderCatalogIndex {
  readonly providers: ProviderDefinition[];
  readonly #providersById: Map<string, ProviderDefinition>;
  readonly #metricsById: Map<string, MetricDefinition>;
  readonly #apiKeyProviderIds: Set<string>;
  readonly #connectionConfigProviderIds: Set<string>;

  constructor(catalog: ProviderCatalog) {
    this.providers = catalog.providers;
    this.#providersById = new Map();
    this.#metricsById = new Map();
    this.#apiKeyProviderIds = new Set(catalog.apiKeyProviderIds ?? []);
    this.#connectionConfigProviderIds = new Set(catalog.connectionConfigProviderIds ?? []);

    for (const provider of catalog.providers) {
      if (this.#providersById.has(provider.id)) {
        throw new Error(`Duplicate provider definition: ${provider.id}`);
      }
      this.#providersById.set(provider.id, provider);
      for (const metric of provider.metrics) {
        if (this.#metricsById.has(metric.id)) {
          throw new Error(`Duplicate metric definition: ${metric.id}`);
        }
        this.#metricsById.set(metric.id, metric);
      }
    }
  }

  provider(id: string) {
    return this.#providersById.get(id);
  }

  metric(id: string) {
    return this.#metricsById.get(id);
  }

  displayName(id: string, providerNames?: Record<string, string>) {
    const customName = providerNames?.[id]?.trim();
    if (customName) return customName;
    return this.provider(id)?.displayName ?? id;
  }

  resolvedDisplayName(id: string, providerNames?: Record<string, string>) {
    const customName = providerNames?.[id]?.trim();
    if (customName) return customName;
    return sub2ApiDisplayName(id) ?? this.displayName(id);
  }

  displayMessage(id: string, message: string, resolvedName?: string) {
    const displayName = resolvedName?.trim() || this.displayName(id);
    const family = id.split('@', 1)[0];
    const aliases = [this.provider(id)?.displayName, this.provider(family)?.displayName]
      .filter((name): name is string => Boolean(name && name !== displayName))
      .filter((name, index, names) => names.indexOf(name) === index)
      .sort((left, right) => right.length - left.length);
    if (aliases.length === 0) return message;
    const pattern = new RegExp(aliases.map(escapeRegExp).join('|'), 'g');
    return message.replace(pattern, () => displayName);
  }

  supportsSpend(id: string) {
    return this.provider(id)?.metrics.some((metric) => metric.source.kind === 'usage') ?? false;
  }

  supportsApiKeyConfiguration(id: string) {
    return this.#apiKeyProviderIds.has(id);
  }

  supportsConnectionConfiguration(id: string) {
    return this.#connectionConfigProviderIds.has(id);
  }

  connectionConfigurationLabel() {
    return 'Unofficial';
  }

  localUsageSourceNote(id: string, displayName?: string) {
    const provider = this.provider(id);
    return (
      provider?.localUsageSourceNote ??
      `From your ${displayName ?? provider?.displayName ?? id} usage history`
    );
  }
}

function escapeRegExp(value: string) {
  return value.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

export function usageSourceNote(
  catalog: ProviderCatalogIndex,
  snapshot: ProviderSnapshot,
  history: UsageHistory,
  scope: UsageScope,
  providerName?: string,
) {
  const displayName = providerName ?? catalog.displayName(snapshot.providerId);
  return (
    history.last30Days?.modelBreakdown?.sourceNote ??
    history.today?.modelBreakdown?.sourceNote ??
    history.yesterday?.modelBreakdown?.sourceNote ??
    (scope === 'localDevice'
      ? catalog.localUsageSourceNote(snapshot.providerId, displayName)
      : `From your ${displayName} account usage history`)
  );
}

export const emptyProviderCatalog = new ProviderCatalogIndex({ providers: [] });
