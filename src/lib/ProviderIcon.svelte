<script lang="ts">
  import {
    providerFamily,
    providerIconColor,
    providerIconPath,
    providerIconOutlined,
    providerIconViewBox,
  } from './providerIconPaths';
  import { getContext } from 'svelte';
  import type { ProviderSnapshot } from './types';
  import { sub2ApiUpstreams, sub2ApiSnapshotContext } from './sub2ApiUpstreams';

  interface Props {
    providerId: string;
    size?: number;
    upstreamProvider?: import('./types').Sub2ApiSource | null;
  }

  let { providerId, size = 18, upstreamProvider }: Props = $props();
  const family = $derived(providerFamily(providerId));
  const snapshotFor = getContext<((id: string) => ProviderSnapshot | null | undefined) | undefined>(
    sub2ApiSnapshotContext,
  );
  const resolvedUpstream = $derived.by(() => {
    if (family !== 'sub2api' || upstreamProvider === null) return undefined;
    const snapshot = snapshotFor?.(providerId);
    // An explicit upstream in the editor previews the draft. Otherwise prefer the
    // latest snapshot, including a neutral icon when discovery is inconclusive.
    if (upstreamProvider === 'subscription')
      return snapshot?.accounts ? undefined : snapshot?.upstreamProviderId;
    if (upstreamProvider) return upstreamProvider;
    if (snapshot && !snapshot.accounts) return snapshot.upstreamProviderId;
    const saved = $sub2ApiUpstreams[providerId];
    return snapshot?.upstreamProviderId ?? (saved === 'subscription' ? undefined : saved);
  });
  const composite = $derived(Boolean(resolvedUpstream && providerIconPath(resolvedUpstream)));
  const path = $derived(providerIconPath(providerId));
  const upstreamPath = $derived(resolvedUpstream ? providerIconPath(resolvedUpstream) : '');
  const upstreamColor = $derived(resolvedUpstream ? providerIconColor(resolvedUpstream) : null);
  const color = $derived(providerIconColor(providerId));
  const viewBox = $derived(composite ? '0 0 100 100' : providerIconViewBox(providerId));
</script>

<svg class="provider-icon" width={size} height={size} {viewBox} fill="none" aria-hidden="true">
  {#if composite}
    <g data-icon-layer="sub2api" transform="scale(2.85)">
      <path
        d={path}
        fill="none"
        stroke={color ?? 'currentColor'}
        stroke-width="2.7"
        stroke-linecap="round"
        stroke-linejoin="round"
      />
    </g>
    <svg
      data-icon-layer="upstream"
      x="44"
      y="44"
      width="56"
      height="56"
      viewBox={providerIconViewBox(resolvedUpstream ?? '')}
    >
      <path d={upstreamPath} fill={upstreamColor ?? 'currentColor'} />
    </svg>
  {:else if providerIconOutlined(providerId)}
    <path
      d={path}
      fill="none"
      stroke={color ?? 'currentColor'}
      stroke-width="2.7"
      stroke-linecap="round"
      stroke-linejoin="round"
    />
  {:else}
    <path d={path} fill={color ?? 'currentColor'} />
  {/if}
</svg>

<style>
  .provider-icon {
    display: block;
    flex: 0 0 auto;
  }
</style>
