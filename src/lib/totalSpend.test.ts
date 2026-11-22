import { describe, expect, it } from 'vitest';
import {
  availableSpendScopes,
  OTHERS_SPEND_ID,
  projectSpend,
  UNPRICED_OTHERS_SPEND_ID,
  type SpendProvider,
} from './totalSpend';
import { providerSpendColorVariable } from './providerIconPaths';
import type { UsageHistory, UsagePeriod, UsageScope } from './types';

const empty: UsageHistory = {
  today: null,
  yesterday: null,
  last30Days: null,
  daily: [],
  unknownModels: [],
};

function history(today: UsagePeriod | null): UsageHistory {
  return { ...empty, today };
}

function provider(
  id: string,
  today: UsagePeriod | null,
  scope: UsageScope = 'localDevice',
): SpendProvider {
  return { id, usageHistories: { [scope]: history(today) } };
}

describe('Total Spend projection', () => {
  it.each<[string, string, string | null]>([
    ['moonshotai/kimi-k2p7', 'kimi-k2.7-code', 'kimi-k2.7-code'],
    ['Sonnet 4.6', 'anthropic/claude-4.6-sonnet', 'claude-sonnet-4-6'],
    ['gpt-5.3-spark', 'openai/gpt-5.3-codex-spark', 'gpt-5.3-codex-spark'],
    ['flash', 'deepseek-flash', null],
    ['gpt-5.1', 'gpt-5', null],
    ['local/deepseek-flash', 'deepseek-flash', null],
  ])('groups %s and %s only when their model identity matches', (first, second, canonical) => {
    const usage = { tokens: 50, pricedTokens: 50, estimatedCostUsd: 1, costEstimated: true };
    const providers = [first, second].map((model, index) => ({
      id: index === 0 ? 'codex' : 'claude',
      usageHistories: {
        localDevice: {
          ...empty,
          otherUsage: {
            today: { ...usage, estimateComplete: true, models: { [model]: usage } },
            yesterday: null,
            last30Days: null,
          },
        },
      },
    }));
    const [others] = projectSpend(providers, 'today', 'cost', 'localDevice').slices;
    expect(others.value).toBe(2);
    if (canonical) {
      expect(others.otherBilling).toMatchObject([
        { model: canonical, tokens: 100, estimatedCostUsd: 2 },
      ]);
    } else {
      expect(others.otherBilling).toMatchObject([{ model: first }, { model: second }]);
    }
  });

  it('uses dedicated colors for the two Others legend categories', () => {
    expect(providerSpendColorVariable(OTHERS_SPEND_ID)).toBe('--provider-others');
    expect(providerSpendColorVariable(UNPRICED_OTHERS_SPEND_ID)).toBe('--provider-others-unpriced');
  });

  it('keeps token-only Codex data available without inventing cost data', () => {
    const providers = [
      provider('codex', {
        tokens: 164_800_000,
        estimatedCostUsd: null,
        costEstimated: true,
        estimateComplete: false,
      }),
      { id: 'antigravity', usageHistories: {} },
    ];

    expect(projectSpend(providers, 'today', 'cost', 'localDevice').slices).toEqual([]);
    expect(projectSpend(providers, 'today', 'tokens', 'localDevice')).toMatchObject({
      centerValue: 164_800_000,
      slices: [{ id: 'codex', value: 164_800_000 }],
    });
  });

  it('does not let a token-only provider erase another provider cost', () => {
    const providers = [
      provider('claude', {
        tokens: 1_000_000,
        estimatedCostUsd: 4,
        costEstimated: true,
        estimateComplete: true,
      }),
      provider('codex', {
        tokens: 9_000_000,
        estimatedCostUsd: null,
        costEstimated: true,
        estimateComplete: false,
      }),
    ];

    expect(projectSpend(providers, 'today', 'cost', 'localDevice')).toMatchObject({
      centerValue: 4,
      slices: [{ id: 'claude', value: 4 }],
    });
  });

  it('calculates a blended cost per million instead of summing provider rates', () => {
    const providers = [
      provider('claude', {
        tokens: 1_000_000,
        estimatedCostUsd: 10,
        costEstimated: true,
        estimateComplete: true,
      }),
      provider('codex', {
        tokens: 3_000_000,
        estimatedCostUsd: 60,
        costEstimated: true,
        estimateComplete: true,
      }),
    ];

    expect(projectSpend(providers, 'today', 'costPerMillion', 'localDevice').centerValue).toBe(
      17.5,
    );
  });

  it('hides cost per million for partially priced periods instead of diluting the rate with unpriced tokens', () => {
    const providers = [
      provider('codex', {
        tokens: 2_000_000,
        estimatedCostUsd: 10,
        costEstimated: true,
        estimateComplete: false,
      }),
    ];

    expect(projectSpend(providers, 'today', 'tokens', 'localDevice')).toMatchObject({
      centerValue: 2_000_000,
      slices: [{ id: 'codex', value: 2_000_000 }],
    });
    expect(projectSpend(providers, 'today', 'cost', 'localDevice')).toMatchObject({
      centerValue: 10,
      estimateComplete: false,
      slices: [{ id: 'codex', value: 10 }],
    });
    expect(projectSpend(providers, 'today', 'costPerMillion', 'localDevice')).toMatchObject({
      centerValue: null,
      slices: [],
    });
  });

  it('tracks local estimation independently from pricing coverage', () => {
    const providers = [
      provider('claude', {
        tokens: 1_000,
        estimatedCostUsd: 2,
        costEstimated: true,
        estimateComplete: false,
      }),
    ];

    expect(projectSpend(providers, 'today', 'cost', 'localDevice')).toMatchObject({
      costEstimated: true,
      estimateComplete: false,
    });
    expect(projectSpend(providers, 'today', 'tokens', 'localDevice')).toMatchObject({
      costEstimated: false,
      estimateComplete: false,
    });
  });

  it('keeps local-device and account histories independent for the same provider', () => {
    const providers: SpendProvider[] = [
      {
        id: 'opencode',
        usageHistories: {
          localDevice: history({
            tokens: 1_000,
            estimatedCostUsd: 2,
            costEstimated: true,
            estimateComplete: true,
          }),
          account: history({
            tokens: 4_000,
            estimatedCostUsd: 8,
            costEstimated: false,
            estimateComplete: true,
          }),
        },
      },
    ];

    expect(availableSpendScopes(providers)).toEqual(['localDevice', 'account']);
    expect(projectSpend(providers, 'today', 'cost', 'localDevice')).toMatchObject({
      scope: 'localDevice',
      centerValue: 2,
      slices: [{ id: 'opencode', value: 2 }],
    });
    expect(projectSpend(providers, 'today', 'cost', 'account')).toMatchObject({
      scope: 'account',
      centerValue: 8,
      slices: [{ id: 'opencode', value: 8 }],
    });
  });

  it('adds token-only Codex account history to the Accounts pie projection', () => {
    const providers: SpendProvider[] = [
      {
        id: 'codex',
        usageHistories: {
          localDevice: history({
            tokens: 1_000,
            estimatedCostUsd: 2,
            costEstimated: true,
            estimateComplete: true,
          }),
          account: history({
            tokens: 9_000,
            estimatedCostUsd: null,
            costEstimated: false,
            estimateComplete: true,
          }),
        },
      },
    ];

    expect(projectSpend(providers, 'today', 'tokens', 'account')).toMatchObject({
      scope: 'account',
      centerValue: 9_000,
      slices: [{ id: 'codex', value: 9_000 }],
    });
    expect(projectSpend(providers, 'today', 'cost', 'account').slices).toEqual([]);
  });

  it('shows one aggregated Others slice for filtered token usage only on local device', () => {
    const otherUsage = {
      today: {
        tokens: 1_000,
        pricedTokens: 400,
        estimatedCostUsd: 2,
        costEstimated: true,
        estimateComplete: false,
      },
      yesterday: null,
      last30Days: null,
    };
    const providers: SpendProvider[] = [
      {
        id: 'codex',
        usageHistories: {
          localDevice: { ...history(null), otherUsage },
          account: history({
            tokens: 200,
            estimatedCostUsd: 1,
            costEstimated: false,
            estimateComplete: true,
          }),
        },
      },
    ];

    expect(projectSpend(providers, 'today', 'tokens', 'localDevice').slices).toMatchObject([
      {
        id: OTHERS_SPEND_ID,
        value: 1_000,
        otherBilling: [
          { providerId: 'codex', tokens: 1_000, pricedTokens: 400, estimatedCostUsd: 2 },
        ],
      },
    ]);
    expect(projectSpend(providers, 'today', 'tokens', 'account').slices).not.toContainEqual(
      expect.objectContaining({ id: OTHERS_SPEND_ID }),
    );
  });

  it('splits priced and unpriced filtered usage for cost charts without showing a fake amount', () => {
    const providers: SpendProvider[] = [
      {
        id: 'codex',
        usageHistories: {
          localDevice: {
            ...history(null),
            otherUsage: {
              today: {
                tokens: 1_000,
                pricedTokens: 400,
                estimatedCostUsd: 2,
                costEstimated: true,
                estimateComplete: false,
              },
              yesterday: null,
              last30Days: null,
            },
          },
        },
      },
    ];

    const cost = projectSpend(providers, 'today', 'cost', 'localDevice');
    expect(cost.centerValue).toBe(2);
    expect(cost.slices).toMatchObject([
      { id: OTHERS_SPEND_ID, value: 2 },
      { id: UNPRICED_OTHERS_SPEND_ID, value: 0.01, showValue: false },
    ]);

    const costPerMillion = projectSpend(providers, 'today', 'costPerMillion', 'localDevice');
    expect(costPerMillion.centerValue).toBe(5_000);
    expect(costPerMillion.slices).toMatchObject([
      { id: OTHERS_SPEND_ID, value: 5_000 },
      { id: UNPRICED_OTHERS_SPEND_ID, value: 0.001, showValue: false },
    ]);
  });

  it('keeps unpriced-only Others slices visible with a null center value instead of a fake total', () => {
    const providers: SpendProvider[] = [
      {
        id: 'codex',
        usageHistories: {
          localDevice: {
            ...history(null),
            otherUsage: {
              today: {
                tokens: 1_000,
                pricedTokens: 0,
                estimatedCostUsd: null,
                costEstimated: true,
                estimateComplete: false,
              },
              yesterday: null,
              last30Days: null,
            },
          },
        },
      },
    ];

    expect(projectSpend(providers, 'today', 'cost', 'localDevice')).toMatchObject({
      centerValue: null,
      slices: [{ id: UNPRICED_OTHERS_SPEND_ID, value: 0.01, showValue: false }],
    });
    expect(projectSpend(providers, 'today', 'costPerMillion', 'localDevice')).toMatchObject({
      centerValue: null,
      slices: [{ id: UNPRICED_OTHERS_SPEND_ID, value: 0.001, showValue: false }],
    });
  });

  it('keeps both Others slices after every real provider regardless of value', () => {
    const providers: SpendProvider[] = [
      {
        id: 'codex',
        usageHistories: {
          localDevice: {
            ...history({
              tokens: 10,
              estimatedCostUsd: 1,
              costEstimated: true,
              estimateComplete: true,
            }),
            otherUsage: {
              today: {
                tokens: 10_000,
                pricedTokens: 8_000,
                estimatedCostUsd: 100,
                costEstimated: true,
                estimateComplete: false,
              },
              yesterday: null,
              last30Days: null,
            },
          },
        },
      },
    ];

    expect(
      projectSpend(providers, 'today', 'cost', 'localDevice').slices.map((slice) => slice.id),
    ).toEqual(['codex', OTHERS_SPEND_ID, UNPRICED_OTHERS_SPEND_ID]);
  });
});
