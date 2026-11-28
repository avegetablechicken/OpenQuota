import { OTHERS_SPEND_ID, UNPRICED_OTHERS_SPEND_ID } from './providerIconPaths';
import type {
  AppSettings,
  OtherModelUsage,
  OtherUsageHistory,
  OtherUsagePeriod,
  UsageHistories,
  UsageHistory,
  UsagePeriod,
  UsageScope,
} from './types';

export { OTHERS_SPEND_ID, UNPRICED_OTHERS_SPEND_ID };

export interface SpendProvider {
  id: string;
  usageHistories: UsageHistories;
}

type BillingRow = OtherModelUsage & ({ model: string } | { providerId: string });

export interface SpendSlice {
  id: string;
  value: number;
  period: UsagePeriod;
  showValue?: boolean;
  otherBilling?: BillingRow[];
}

export interface SpendProjection {
  scope: UsageScope;
  slices: SpendSlice[];
  centerValue: number | null;
  costEstimated: boolean;
  estimateComplete: boolean;
}

export const SPEND_SCOPE_LABELS: Record<UsageScope, string> = {
  localDevice: 'Local Device',
  account: 'Accounts',
};

export function availableSpendScopes(providers: SpendProvider[]): UsageScope[] {
  return (['localDevice', 'account'] as const).filter((scope) =>
    providers.some((provider) => provider.usageHistories[scope]),
  );
}

export function selectedPeriod(
  history: UsageHistory,
  selection: AppSettings['totalSpendPeriod'],
): UsagePeriod | null {
  if (selection === 'today') return history.today;
  if (selection === 'yesterday') return history.yesterday;
  return history.last30Days;
}

function valueFor(period: UsagePeriod, metric: AppSettings['totalSpendMetric']): number | null {
  if (metric === 'tokens') return period.tokens > 0 ? period.tokens : null;
  if (period.estimatedCostUsd === null || period.estimatedCostUsd < 0) return null;
  if (metric === 'costPerMillion') {
    // A Recorded $0 is a measured rate, and its tokens belong in the blended rate.
    return period.estimateComplete && period.tokens > 0
      ? (period.estimatedCostUsd / period.tokens) * 1_000_000
      : null;
  }
  return period.estimatedCostUsd > 0 ? period.estimatedCostUsd : null;
}

function selectedOtherPeriod(
  history: OtherUsageHistory | null | undefined,
  selection: AppSettings['totalSpendPeriod'],
) {
  if (!history) return null;
  if (selection === 'today') return history.today;
  if (selection === 'yesterday') return history.yesterday;
  return history.last30Days;
}

function otherPeriod(
  tokens: number,
  estimatedCostUsd: number | null,
  costEstimated: boolean,
  estimateComplete: boolean,
): UsagePeriod {
  return {
    tokens,
    estimatedCostUsd,
    costEstimated,
    estimateComplete,
    modelBreakdown: null,
    unknownModels: [],
  };
}

function aggregateOtherUsage(periods: OtherModelUsage[]): OtherModelUsage {
  const pricedPeriods = periods.filter((period) => period.estimatedCostUsd !== null);
  return {
    tokens: periods.reduce((sum, period) => sum + period.tokens, 0),
    pricedTokens: periods.reduce((sum, period) => sum + period.pricedTokens, 0),
    estimatedCostUsd:
      pricedPeriods.length > 0
        ? pricedPeriods.reduce((sum, period) => sum + (period.estimatedCostUsd ?? 0), 0)
        : null,
    costEstimated: pricedPeriods.some((period) => period.costEstimated),
  };
}

function isOthersSlice(id: string) {
  return id === OTHERS_SPEND_ID || id === UNPRICED_OTHERS_SPEND_ID;
}

function spendSliceSort(left: SpendSlice, right: SpendSlice) {
  const leftIsOthers = isOthersSlice(left.id);
  const rightIsOthers = isOthersSlice(right.id);
  if (leftIsOthers !== rightIsOthers) return leftIsOthers ? 1 : -1;
  return right.value - left.value || left.id.localeCompare(right.id);
}

// Use model identity for display. A pricing fallback can point at a different model.
function otherModelName(raw: string): string {
  const normalized = raw.trim().toLowerCase();
  const name = normalized
    .replace(/^deepseek\/(?=deepseek-)/, '')
    .replace(/^anthropic\/(?=claude-)/, '')
    .replace(/^(?:openai|openai-codex)\/(?=(?:gpt-|codex-))/, '')
    .replace(/^(?:moonshot|moonshotai)\/(?=kimi-)/, '')
    .replace(/^xai\/(?=grok-)/, '')
    .replace(/^google\/(?=gemini-)/, '');

  const claude = /^(?:claude[- ])?(haiku|sonnet|opus|fable|mythos)[- ](\d+)[.-](\d+)$/.exec(name);
  if (claude) return `claude-${claude[1]}-${claude[2]}-${claude[3]}`;
  const reversedClaude = /^claude-(\d+)\.(\d+)-(haiku|sonnet|opus|fable|mythos)$/.exec(name);
  if (reversedClaude)
    return `claude-${reversedClaude[3]}-${reversedClaude[1]}-${reversedClaude[2]}`;

  if (/^kimi-k2[.p]7(?:-code)?$/.test(name)) return 'kimi-k2.7-code';
  if (name === 'kimi-k2p5') return 'kimi-k2.5';
  if (name === 'gpt-5.3-spark') return 'gpt-5.3-codex-spark';
  return name !== normalized ? name : raw.trim();
}

function otherSpendSlices(
  providers: SpendProvider[],
  periodSelection: AppSettings['totalSpendPeriod'],
  metric: AppSettings['totalSpendMetric'],
): SpendSlice[] {
  const periods: OtherUsagePeriod[] = [];
  const otherBilling: BillingRow[] = [];
  const usageByModel = new Map<string, { model: string; entries: OtherModelUsage[] }>();
  for (const provider of providers) {
    const period = selectedOtherPeriod(
      provider.usageHistories.localDevice?.otherUsage,
      periodSelection,
    );
    if (!period) continue;
    periods.push(period);
    const models = Object.entries(period.models ?? {});
    if (models.length) {
      for (const [model, usage] of models) {
        const displayName = otherModelName(model);
        const key = displayName.toLowerCase();
        const group = usageByModel.get(key);
        if (group) group.entries.push(usage);
        else usageByModel.set(key, { model: displayName, entries: [usage] });
      }
    } else {
      // Older snapshots kept only the provider subtotal.
      otherBilling.push({ providerId: provider.id, ...period });
    }
  }
  for (const { model, entries } of usageByModel.values()) {
    otherBilling.push({ model, ...aggregateOtherUsage(entries) });
  }
  otherBilling.sort(
    (a, b) => (b.estimatedCostUsd ?? 0) - (a.estimatedCostUsd ?? 0) || b.tokens - a.tokens,
  );
  if (periods.length === 0) return [];

  const aggregate = aggregateOtherUsage(periods);
  if (metric === 'tokens') {
    return aggregate.tokens > 0
      ? [
          {
            id: OTHERS_SPEND_ID,
            otherBilling,
            value: aggregate.tokens,
            period: otherPeriod(
              aggregate.tokens,
              aggregate.estimatedCostUsd,
              aggregate.costEstimated,
              periods.every((period) => period.estimateComplete),
            ),
          },
        ]
      : [];
  }

  const slices: SpendSlice[] = [];
  if (aggregate.estimatedCostUsd !== null && aggregate.estimatedCostUsd >= 0) {
    const pricedValue =
      metric === 'costPerMillion'
        ? aggregate.pricedTokens > 0
          ? (aggregate.estimatedCostUsd / aggregate.pricedTokens) * 1_000_000
          : null
        : aggregate.estimatedCostUsd > 0
          ? aggregate.estimatedCostUsd
          : null;
    if (pricedValue !== null) {
      slices.push({
        id: OTHERS_SPEND_ID,
        otherBilling,
        value: pricedValue,
        period: otherPeriod(
          aggregate.pricedTokens,
          aggregate.estimatedCostUsd,
          aggregate.costEstimated,
          true,
        ),
      });
    }
  }

  const unpricedTokens = aggregate.tokens - aggregate.pricedTokens;
  if (unpricedTokens > 0) {
    const pseudoValue = metric === 'cost' ? 0.01 : 0.001;
    slices.push({
      id: UNPRICED_OTHERS_SPEND_ID,
      otherBilling,
      value: pseudoValue,
      showValue: false,
      period: otherPeriod(unpricedTokens, null, true, false),
    });
  }
  return slices;
}

export function projectSpend(
  providers: SpendProvider[],
  periodSelection: AppSettings['totalSpendPeriod'],
  metric: AppSettings['totalSpendMetric'],
  scope: UsageScope,
): SpendProjection {
  const providerSlices = providers
    .flatMap((provider) => {
      const history = provider.usageHistories[scope];
      if (!history) return [];
      const period = selectedPeriod(history, periodSelection);
      if (!period) return [];
      const value = valueFor(period, metric);
      return value === null ? [] : [{ id: provider.id, value, period }];
    })
    .sort(spendSliceSort);
  const slices: SpendSlice[] = [
    ...providerSlices,
    ...(scope === 'localDevice' ? otherSpendSlices(providers, periodSelection, metric) : []),
  ].sort(spendSliceSort);

  if (slices.length === 0) {
    return { scope, slices, centerValue: null, costEstimated: false, estimateComplete: true };
  }

  const measuredSlices = slices.filter((slice) => slice.showValue !== false);
  const centerValue =
    measuredSlices.length === 0
      ? null
      : metric === 'costPerMillion'
        ? (() => {
            const totalCost = measuredSlices.reduce(
              (sum, slice) => sum + (slice.period.estimatedCostUsd ?? 0),
              0,
            );
            const totalTokens = measuredSlices.reduce((sum, slice) => sum + slice.period.tokens, 0);
            return totalTokens > 0 ? (totalCost / totalTokens) * 1_000_000 : null;
          })()
        : measuredSlices.reduce((sum, slice) => sum + slice.value, 0);

  return {
    scope,
    slices,
    centerValue,
    costEstimated: metric !== 'tokens' && slices.some((slice) => slice.period.costEstimated),
    estimateComplete: slices.every((slice) => slice.period.estimateComplete),
  };
}

export function emptySpendMessage(metric: AppSettings['totalSpendMetric']) {
  if (metric === 'tokens') return 'No token data for this period';
  if (metric === 'costPerMillion') return 'No cost-per-token data for this period';
  return 'No cost data for this period';
}
