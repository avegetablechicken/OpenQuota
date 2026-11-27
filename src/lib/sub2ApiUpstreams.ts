import { writable } from 'svelte/store';
import type { MetricPolicy, ProviderSnapshot, Sub2ApiSource, Sub2ApiUpstream } from './types';

export const sub2ApiSnapshotContext = Symbol('Sub2API snapshot');

/** Tracks were kept in browser storage before the backend sent them; only the key is cleared. */
const legacyTrackStorageKey = 'openquota.sub2api-upstreams';
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

try {
  storage()?.removeItem(legacyTrackStorageKey);
} catch {
  // A stale key is harmless.
}

/** Each connection's Track, as the latest settings view state reports it. */
export const sub2ApiUpstreams = writable<Record<string, Sub2ApiSource>>({});

export function syncSub2ApiTracks(policies: Record<string, MetricPolicy>) {
  const tracks = Object.fromEntries(
    Object.entries(policies).flatMap(([providerId, policy]) =>
      policy.track ? [[providerId, policy.track] as const] : [],
    ),
  );
  sub2ApiUpstreams.set(tracks);
}

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
  policy: MetricPolicy | undefined,
  subscription = isSub2ApiSubscription(snapshot),
) {
  // Retain saved layout preferences, but omit metrics unavailable on this Track.
  // TEMPORARY: while the Track is unknown, the Dashboard and share card hide nothing.
  if (policy?.track && !policy.visibleMetricIds.includes(metricId)) return false;
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

/** Records the host of a connection's Base URL for its label; an empty URL removes it. */
export function rememberSub2ApiEndpoint(providerId: string, baseUrl: string) {
  sub2ApiEndpoints.update((current) => {
    const next = { ...current };
    const endpoint = endpointLabel(baseUrl);
    if (endpoint) next[providerId] = endpoint;
    else delete next[providerId];
    persistEndpoints(next);
    return next;
  });
}

export function forgetSub2ApiEndpoint(providerId: string) {
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
