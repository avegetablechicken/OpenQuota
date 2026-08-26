import type { UsageHistories, UsageHistory, UsageScope, UsageViewMode } from './types';

export const PROVIDER_USAGE_SCOPE_LABELS: Record<UsageScope, string> = {
  localDevice: 'Device',
  account: 'Account',
};

export interface ScopedUsageHistory {
  scope: UsageScope;
  history: UsageHistory;
}

export function availableUsageScopes(histories: UsageHistories): UsageScope[] {
  return (['localDevice', 'account'] as const).filter((scope) => Boolean(histories[scope]));
}

export function usageHistoriesForMode(
  histories: UsageHistories,
  mode: UsageViewMode,
): ScopedUsageHistory[] {
  return availableUsageScopes(histories)
    .filter((scope) => mode === 'all' || mode === scope)
    .map((scope) => ({ scope, history: histories[scope]! }));
}

export function shouldShowProviderForMode(histories: UsageHistories, mode: UsageViewMode): boolean {
  if (mode === 'all') return true;
  return Boolean(histories[mode]);
}
