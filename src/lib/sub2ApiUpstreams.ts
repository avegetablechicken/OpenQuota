import { writable } from 'svelte/store';
import type { Sub2ApiUpstream } from './types';

const storageKey = 'openquota.sub2api-upstreams';

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

function persist(upstreams: Record<string, Sub2ApiUpstream>) {
  try {
    storage()?.setItem(storageKey, JSON.stringify(upstreams));
  } catch {
    // The icon preference is nonessential when browser storage is unavailable.
  }
}

export function rememberSub2ApiUpstream(providerId: string, upstream: Sub2ApiUpstream) {
  sub2ApiUpstreams.update((current) => {
    const next = { ...current, [providerId]: upstream };
    persist(next);
    return next;
  });
}

export function forgetSub2ApiUpstream(providerId: string) {
  sub2ApiUpstreams.update((current) => {
    const next = { ...current };
    delete next[providerId];
    persist(next);
    return next;
  });
}
