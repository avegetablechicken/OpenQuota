import { describe, expect, it } from 'vitest';
import { shouldShowProviderForMode } from './usageScopes';
import type { UsageHistory } from './types';

const history: UsageHistory = {
  today: null,
  yesterday: null,
  last30Days: null,
  daily: [],
  unknownModels: [],
};

describe('usage scope provider visibility', () => {
  it('shows providers with no history only in All', () => {
    expect(shouldShowProviderForMode({}, 'localDevice')).toBe(false);
    expect(shouldShowProviderForMode({}, 'account')).toBe(false);
    expect(shouldShowProviderForMode({}, 'all')).toBe(true);
  });

  it('shows only the matching history provider in scoped views', () => {
    expect(shouldShowProviderForMode({ localDevice: history }, 'localDevice')).toBe(true);
    expect(shouldShowProviderForMode({ localDevice: history }, 'account')).toBe(false);
    expect(shouldShowProviderForMode({ account: history }, 'localDevice')).toBe(false);
    expect(shouldShowProviderForMode({ account: history }, 'account')).toBe(true);
  });

  it('keeps providers with both histories in either scoped view and All', () => {
    const histories = { localDevice: history, account: history };
    expect(shouldShowProviderForMode(histories, 'localDevice')).toBe(true);
    expect(shouldShowProviderForMode(histories, 'account')).toBe(true);
    expect(shouldShowProviderForMode(histories, 'all')).toBe(true);
  });
});
