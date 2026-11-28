import { SvelteMap } from 'svelte/reactivity';
import type { ResetClaimOutcome } from './types';

interface PendingClaim {
  requestId: string;
  status: 'retryable' | 'pending' | 'uncertain';
}

// Shared by all popover instances. Persist before sending so reopening the panel (or app)
// cannot turn an unresolved debit into a fresh request with a different idempotency key.
const prefix = 'openquota.reset-claim.v1:';

function persist(key: string, claim: PendingClaim | null) {
  try {
    if (claim) localStorage.setItem(prefix + key, JSON.stringify(claim));
    else localStorage.removeItem(prefix + key);
  } catch {
    // The shared in-memory entry still protects popover closure when storage is unavailable.
  }
}

function restoreClaims(): Array<[string, PendingClaim]> {
  const restored: Array<[string, PendingClaim]> = [];
  try {
    for (let index = 0; index < localStorage.length; index++) {
      const key = localStorage.key(index);
      if (!key?.startsWith(prefix)) continue;
      let saved: PendingClaim | null;
      try {
        saved = JSON.parse(localStorage.getItem(key) ?? 'null') as PendingClaim | null;
      } catch {
        continue;
      }
      if (saved && typeof saved.requestId === 'string' && saved.requestId) {
        restored.push([
          key.slice(prefix.length),
          {
            requestId: saved.requestId,
            status: saved.status === 'retryable' ? 'retryable' : 'uncertain',
          },
        ]);
      }
    }
  } catch {
    // Keep any usable entries already restored if storage is unavailable or malformed.
  }
  return restored;
}

const claims = new SvelteMap<string, PendingClaim>(restoreClaims());

export function getResetClaim(key: string): PendingClaim | undefined {
  return claims.get(key);
}

export function startResetClaim(key: string): string | null {
  const previous = getResetClaim(key);
  if (previous && previous.status !== 'retryable') return null;
  const requestId =
    previous?.requestId ??
    globalThis.crypto?.randomUUID?.() ??
    `${Date.now()}-${Math.random().toString(16).slice(2)}`;
  const claim: PendingClaim = { requestId, status: 'pending' };
  claims.set(key, claim);
  persist(key, claim);
  return requestId;
}

export function finishResetClaim(key: string, outcome: ResetClaimOutcome) {
  const current = getResetClaim(key);
  if (!current) return;
  if (outcome === 'uncertain' || outcome === 'failed') {
    const claim: PendingClaim = {
      requestId: current.requestId,
      status: outcome === 'uncertain' ? 'uncertain' : 'retryable',
    };
    claims.set(key, claim);
    persist(key, claim);
  } else {
    claims.delete(key);
    persist(key, null);
  }
}
