import { OTHERS_SPEND_ID, UNPRICED_OTHERS_SPEND_ID } from './providerIconPaths';
import type {
  AppSettings,
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

export interface SpendSlice {
  id: string;
  value: number;
  period: UsagePeriod;
  showValue?: boolean;
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
  if (period.estimatedCostUsd === null || period.estimatedCostUsd <= 0) return null;
  if (metric === 'costPerMillion') {
    return period.tokens > 0 ? (period.estimatedCostUsd / period.tokens) * 1_000_000 : null;
  }
  return period.estimatedCostUsd;
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

function aggregateOtherPeriod(periods: OtherUsagePeriod[]): OtherUsagePeriod {
  const pricedPeriods = periods.filter((period) => period.estimatedCostUsd !== null);
  return {
    tokens: periods.reduce((sum, period) => sum + period.tokens, 0),
    pricedTokens: periods.reduce((sum, period) => sum + period.pricedTokens, 0),
    estimatedCostUsd:
      pricedPeriods.length > 0
        ? pricedPeriods.reduce((sum, period) => sum + (period.estimatedCostUsd ?? 0), 0)
        : null,
    costEstimated: pricedPeriods.some((period) => period.costEstimated),
    estimateComplete: periods.every((period) => period.estimateComplete),
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

function otherSpendSlices(
  providers: SpendProvider[],
  periodSelection: AppSettings['totalSpendPeriod'],
  metric: AppSettings['totalSpendMetric'],
): SpendSlice[] {
  const periods = providers
    .map((provider) =>
      selectedOtherPeriod(provider.usageHistories.localDevice?.otherUsage, periodSelection),
    )
    .filter((period): period is OtherUsagePeriod => period !== null);
  if (periods.length === 0) return [];

  const aggregate = aggregateOtherPeriod(periods);
  if (metric === 'tokens') {
    return aggregate.tokens > 0
      ? [
          {
            id: OTHERS_SPEND_ID,
            value: aggregate.tokens,
            period: otherPeriod(
              aggregate.tokens,
              aggregate.estimatedCostUsd,
              aggregate.costEstimated,
              aggregate.estimateComplete,
            ),
          },
        ]
      : [];
  }

  const slices: SpendSlice[] = [];
  if (aggregate.estimatedCostUsd !== null && aggregate.estimatedCostUsd > 0) {
    const pricedValue =
      metric === 'costPerMillion'
        ? aggregate.pricedTokens > 0
          ? (aggregate.estimatedCostUsd / aggregate.pricedTokens) * 1_000_000
          : null
        : aggregate.estimatedCostUsd;
    if (pricedValue !== null) {
      slices.push({
        id: OTHERS_SPEND_ID,
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
    const pseudoCost =
      metric === 'costPerMillion' ? (pseudoValue * unpricedTokens) / 1_000_000 : pseudoValue;
    slices.push({
      id: UNPRICED_OTHERS_SPEND_ID,
      value: pseudoValue,
      showValue: false,
      period: otherPeriod(unpricedTokens, pseudoCost, true, false),
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
  const slices = [
    ...providerSlices,
    ...(scope === 'localDevice' ? otherSpendSlices(providers, periodSelection, metric) : []),
  ].sort(spendSliceSort);

  if (slices.length === 0) {
    return { scope, slices, centerValue: null, costEstimated: false, estimateComplete: true };
  }

  const centerValue =
    metric === 'costPerMillion'
      ? (() => {
          const totalCost = slices.reduce(
            (sum, slice) => sum + (slice.period.estimatedCostUsd ?? 0),
            0,
          );
          const totalTokens = slices.reduce((sum, slice) => sum + slice.period.tokens, 0);
          return totalTokens > 0 ? (totalCost / totalTokens) * 1_000_000 : null;
        })()
      : slices.reduce((sum, slice) => sum + slice.value, 0);

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
