import { writable } from 'svelte/store';
import type { ProviderSnapshot, Sub2ApiSource, Sub2ApiUpstream } from './types';

export const sub2ApiSnapshotContext = Symbol('Sub2API snapshot');

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

export const sub2ApiUpstreamOptions: Sub2ApiUpstream[] = ['codex', 'claude', 'antigravity', 'grok'];
const sources: Sub2ApiSource[] = ['subscription', ...sub2ApiUpstreamOptions];
const upstreamLabels: Record<Sub2ApiUpstream, string> = {
  codex: 'Codex',
  claude: 'Claude',
  antigravity: 'Antigravity',
  grok: 'Grok',
};

/** Whether the Base URL can be read from a named Codex or Claude client configuration. */
export function sub2ApiResolvesProviderNames(upstream: Sub2ApiUpstream) {
  return upstream === 'codex' || upstream === 'claude';
}

function loadUpstreams(): Record<string, Sub2ApiSource> {
  try {
    const parsed = JSON.parse(storage()?.getItem(storageKey) ?? '{}') as Record<string, unknown>;
    return Object.fromEntries(
      Object.entries(parsed).filter(
        (entry): entry is [string, Sub2ApiSource] =>
          typeof entry[1] === 'string' && sources.includes(entry[1] as Sub2ApiSource),
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

/** Inspect the connection snapshot, before flattening its administrator account rows. */
export function isSub2ApiSubscription(snapshot: ProviderSnapshot | null | undefined) {
  return Boolean(snapshot && sub2ApiSlotDisplayName(snapshot.providerId) && !snapshot.accounts);
}

export function showSub2ApiMetric(
  snapshot: ProviderSnapshot,
  metricId: string,
  subscription = isSub2ApiSubscription(snapshot),
  upstream: Sub2ApiSource | undefined = sub2ApiUpstreamOptions.find(
    (id) => id === snapshot.upstreamProviderId,
  ),
) {
  const source = subscription ? 'subscription' : upstream;
  // Retain saved layout preferences, but omit metrics unavailable on this track/upstream.
  if (source && !sub2ApiMetricSupported(snapshot.providerId, metricId, source)) return false;
  if (metricId !== `${snapshot.providerId}.monthly` || !subscription) return true;
  return snapshot.quotas.some((window) => window.id === 'monthly' && (window.limitValue ?? 0) > 0);
}

export function sub2ApiDisplayName(providerId: string, customName?: string) {
  const slotName = sub2ApiSlotDisplayName(providerId);
  if (!slotName) return null;
  if (customName?.trim()) return customName.trim();
  return slotName;
}

export function sub2ApiUpstreamLabel(upstream: Sub2ApiSource) {
  if (upstream === 'subscription') return 'User subscription';
  return upstreamLabels[upstream];
}

export function sub2ApiPublicName(upstream?: Sub2ApiSource) {
  return upstream ? `Sub2API · ${sub2ApiUpstreamLabel(upstream)}` : 'Account name';
}

export function sub2ApiPinAllowed(metricId: string, source?: Sub2ApiSource) {
  return source !== 'subscription' || /\.(daily|weekly|monthly)$/.test(metricId);
}

export function sub2ApiMetricSupported(
  providerId: string,
  metricId: string,
  upstream?: Sub2ApiSource,
) {
  if (sub2ApiDisplayName(providerId) === null) return true;
  const prefix = `${providerId}.`;
  if (!metricId.startsWith(prefix)) return true;
  const suffix = metricId.slice(prefix.length);
  if (upstream === 'subscription') {
    return ['daily', 'weekly', 'monthly', 'today', 'yesterday', 'last30', 'trend'].includes(suffix);
  }
  if (!upstream) {
    return !['daily', 'monthly', 'extra', 'claude', 'claudeWeekly'].includes(suffix);
  }
  const upstreamOnly: Record<Sub2ApiUpstream, string[]> = {
    codex: ['session', 'spark', 'sparkWeekly', 'rateLimitResets'],
    claude: ['session', 'sonnet', 'fable'],
    antigravity: ['session', 'claude', 'claudeWeekly'],
    grok: ['monthly'],
  };
  return (
    ['weekly', 'trend', 'today', 'yesterday', 'last30'].includes(suffix) ||
    upstreamOnly[upstream].includes(suffix)
  );
}

function persist(upstreams: Record<string, Sub2ApiSource>) {
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
  upstream: Sub2ApiSource,
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

export function sub2ApiUpstreamAccountName(name: string): string {
  return name.replace(/^Sub2API · (?:Codex|Claude|Antigravity|Grok) · /, '');
}
