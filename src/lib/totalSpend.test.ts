import { describe, expect, it } from 'vitest';
import { availableSpendScopes, projectSpend } from './totalSpend';
import type { UsageHistory } from './types';

const empty: UsageHistory = {
  scope: 'localDevice',
  today: null,
  yesterday: null,
  last30Days: null,
  daily: [],
  unknownModels: [],
};

function usage(today: UsageHistory['today']): UsageHistory {
  return { ...empty, today };
}

describe('Total Spend projection', () => {
  it('keeps token-only Codex data available without inventing cost data', () => {
    const providers = [
      {
        id: 'codex',
        usage: usage({
          tokens: 164_800_000,
          estimatedCostUsd: null,
          costEstimated: true,
          estimateComplete: false,
        }),
      },
      { id: 'antigravity', usage: empty },
    ];

    expect(projectSpend(providers, 'today', 'cost', 'localDevice').slices).toEqual([]);
    expect(projectSpend(providers, 'today', 'tokens', 'localDevice')).toMatchObject({
      centerValue: 164_800_000,
      slices: [{ id: 'codex', value: 164_800_000 }],
    });
  });

  it('does not let a token-only provider erase another provider cost', () => {
    const providers = [
      {
        id: 'claude',
        usage: usage({
          tokens: 1_000_000,
          estimatedCostUsd: 4,
          costEstimated: true,
          estimateComplete: true,
        }),
      },
      {
        id: 'codex',
        usage: usage({
          tokens: 9_000_000,
          estimatedCostUsd: null,
          costEstimated: true,
          estimateComplete: false,
        }),
      },
    ];

    expect(projectSpend(providers, 'today', 'cost', 'localDevice')).toMatchObject({
      centerValue: 4,
      slices: [{ id: 'claude', value: 4 }],
    });
  });

  it('calculates a blended cost per million instead of summing provider rates', () => {
    const providers = [
      {
        id: 'claude',
        usage: usage({
          tokens: 1_000_000,
          estimatedCostUsd: 10,
          costEstimated: true,
          estimateComplete: true,
        }),
      },
      {
        id: 'codex',
        usage: usage({
          tokens: 3_000_000,
          estimatedCostUsd: 60,
          costEstimated: true,
          estimateComplete: true,
        }),
      },
    ];

    expect(projectSpend(providers, 'today', 'costPerMillion', 'localDevice').centerValue).toBe(
      17.5,
    );
  });

  it('tracks local estimation independently from pricing coverage', () => {
    const providers = [
      {
        id: 'claude',
        usage: usage({
          tokens: 1_000,
          estimatedCostUsd: 2,
          costEstimated: true,
          estimateComplete: false,
        }),
      },
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

  it('never combines local-device and account histories', () => {
    const providers = [
      {
        id: 'codex',
        usage: usage({
          tokens: 1_000,
          estimatedCostUsd: 2,
          costEstimated: true,
          estimateComplete: true,
        }),
      },
      {
        id: 'cursor',
        usage: {
          ...usage({
            tokens: 4_000,
            estimatedCostUsd: 8,
            costEstimated: false,
            estimateComplete: true,
          }),
          scope: 'account' as const,
        },
      },
    ];

    expect(availableSpendScopes(providers)).toEqual(['localDevice', 'account']);
    expect(projectSpend(providers, 'today', 'cost', 'localDevice')).toMatchObject({
      scope: 'localDevice',
      centerValue: 2,
      slices: [{ id: 'codex', value: 2 }],
    });
    expect(projectSpend(providers, 'today', 'cost', 'account')).toMatchObject({
      scope: 'account',
      centerValue: 8,
      slices: [{ id: 'cursor', value: 8 }],
    });
  });
});
