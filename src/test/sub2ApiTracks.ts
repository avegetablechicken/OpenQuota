import { sub2ApiUpstreams } from '../lib/sub2ApiUpstreams';
import type { Sub2ApiSource } from '../lib/types';

/** Sets a connection's Track as a settings view state would. */
export function setSub2ApiTrack(providerId: string, track: Sub2ApiSource) {
  sub2ApiUpstreams.update((current) => ({ ...current, [providerId]: track }));
}

export function clearSub2ApiTrack(providerId: string) {
  sub2ApiUpstreams.update((current) => {
    const next = { ...current };
    delete next[providerId];
    return next;
  });
}
