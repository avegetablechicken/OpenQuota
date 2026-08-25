import { writable } from 'svelte/store';
import type { Sub2ApiUpstream } from './types';

const storageKey = 'openquota.sub2api-upstreams';
const endpointStorageKey = 'openquota.sub2api-endpoints';

function storage() {
  if (typeof window === 'undefined' || import.meta.env.MODE === 'test') return null;
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

function loadUpstreams(): Record<string, Sub2ApiUpstream> {
  try {
    const parsed = JSON.parse(storage()?.getItem(storageKey) ?? '{}') as Record<string, unknown>;
    return Object.fromEntries(
      Object.entries(parsed).filter(
        (entry): entry is [string, Sub2ApiUpstream] =>
          entry[1] === 'codex' || entry[1] === 'claude',
      ),
    );
  } catch {
    return {};
  }
}

export const sub2ApiUpstreams = writable(loadUpstreams());

function loadEndpoints(): Record<string, string> {
  try {
    const parsed = JSON.parse(storage()?.getItem(endpointStorageKey) ?? '{}') as Record<
      string,
      unknown
    >;
    return Object.fromEntries(
      Object.entries(parsed).filter(
        (entry): entry is [string, string] =>
          typeof entry[1] === 'string' && entry[1].trim().length > 0,
      ),
    );
  } catch {
    return {};
  }
}

export const sub2ApiEndpoints = writable(loadEndpoints());

function sub2ApiSlotDisplayName(providerId: string) {
  const match = providerId.match(/^sub2api(?:@(\d+))?$/);
  if (!match) return null;
  return match[1] ? `Sub2API ${match[1]}` : 'Sub2API';
}

export function sub2ApiDisplayName(providerId: string, customName?: string) {
  const slotName = sub2ApiSlotDisplayName(providerId);
  if (!slotName) return null;
  if (customName?.trim()) return customName.trim();
  return slotName;
}

export function sub2ApiMetricSupported(
  providerId: string,
  metricId: string,
  upstream?: Sub2ApiUpstream,
) {
  if (sub2ApiDisplayName(providerId) === null || !upstream) return true;
  const prefix = `${providerId}.`;
  if (!metricId.startsWith(prefix)) return true;
  const suffix = metricId.slice(prefix.length);
  if (suffix === 'extra') return false;
  if (upstream === 'claude') {
    return !['spark', 'sparkWeekly', 'rateLimitResets'].includes(suffix);
  }
  return !['sonnet', 'fable'].includes(suffix);
}

function persist(upstreams: Record<string, Sub2ApiUpstream>) {
  try {
    storage()?.setItem(storageKey, JSON.stringify(upstreams));
  } catch {
    // The icon preference is nonessential when browser storage is unavailable.
  }
}

function persistEndpoints(endpoints: Record<string, string>) {
  try {
    storage()?.setItem(endpointStorageKey, JSON.stringify(endpoints));
  } catch {
    // The connection label is nonessential when browser storage is unavailable.
  }
}

function endpointLabel(baseUrl: string) {
  try {
    return new URL(baseUrl).host;
  } catch {
    return '';
  }
}

export function rememberSub2ApiUpstream(
  providerId: string,
  upstream: Sub2ApiUpstream,
  baseUrl?: string,
) {
  sub2ApiUpstreams.update((current) => {
    const next = { ...current, [providerId]: upstream };
    persist(next);
    return next;
  });
  if (baseUrl !== undefined) {
    sub2ApiEndpoints.update((current) => {
      const next = { ...current };
      const endpoint = endpointLabel(baseUrl);
      if (endpoint) next[providerId] = endpoint;
      else delete next[providerId];
      persistEndpoints(next);
      return next;
    });
  }
}

export function forgetSub2ApiUpstream(providerId: string) {
  sub2ApiUpstreams.update((current) => {
    const next = { ...current };
    delete next[providerId];
    persist(next);
    return next;
  });
  sub2ApiEndpoints.update((current) => {
    const next = { ...current };
    delete next[providerId];
    persistEndpoints(next);
    return next;
  });
}
