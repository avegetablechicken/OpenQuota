import { afterEach, beforeEach, expect, it, vi } from 'vitest';

beforeEach(() => {
  vi.resetModules();
  const stored = new Map<string, string>();
  vi.stubGlobal('localStorage', {
    get length() {
      return stored.size;
    },
    key: (index: number) => [...stored.keys()][index] ?? null,
    getItem: (key: string) => stored.get(key) ?? null,
    setItem: (key: string, value: string) => stored.set(key, value),
    removeItem: (key: string) => stored.delete(key),
  });
});
afterEach(() => vi.unstubAllGlobals());

it('restores an interrupted request as uncertain without issuing a new ID', async () => {
  const first = await import('./resetClaims');
  const requestId = first.startResetClaim('claude:owner:expiry:fable');
  expect(first.startResetClaim('claude:owner:expiry:fable')).toBeNull();
  vi.resetModules();
  const reloaded = await import('./resetClaims');
  expect(reloaded.getResetClaim('claude:owner:expiry:fable')).toEqual({
    requestId,
    status: 'uncertain',
  });
  expect(reloaded.startResetClaim('claude:owner:expiry:fable')).toBeNull();
  expect(reloaded.startResetClaim('claude:another-owner:expiry:fable')).not.toBeNull();
});

it('reuses a retryable request after a reload and clears it only on a definite result', async () => {
  const first = await import('./resetClaims');
  const requestId = first.startResetClaim('claude:owner:expiry:full');
  first.finishResetClaim('claude:owner:expiry:full', 'failed');
  vi.resetModules();
  const reloaded = await import('./resetClaims');
  expect(reloaded.startResetClaim('claude:owner:expiry:full')).toBe(requestId);
  reloaded.finishResetClaim('claude:owner:expiry:full', 'success');
  expect(reloaded.getResetClaim('claude:owner:expiry:full')).toBeUndefined();
  expect(localStorage.length).toBe(0);
});

it('restores uncertainty even when another saved entry is malformed', async () => {
  localStorage.setItem('openquota.reset-claim.v1:broken', '{');
  const first = await import('./resetClaims');
  const requestId = first.startResetClaim('claude:owner:expiry:full');
  first.finishResetClaim('claude:owner:expiry:full', 'uncertain');
  vi.resetModules();
  const reloaded = await import('./resetClaims');
  expect(reloaded.getResetClaim('claude:owner:expiry:full')).toEqual({
    requestId,
    status: 'uncertain',
  });
  expect(reloaded.startResetClaim('claude:owner:expiry:full')).toBeNull();
});
