<script lang="ts">
  import {
    providerFamily,
    providerIconColor,
    providerIconPath,
    providerIconViewBox,
  } from './providerIconPaths';
  import { sub2ApiUpstreams } from './sub2ApiUpstreams';

  interface Props {
    providerId: string;
    size?: number;
    upstreamProvider?: 'codex' | 'claude';
  }

  let { providerId, size = 18, upstreamProvider }: Props = $props();
  const family = $derived(providerFamily(providerId));
  const composite = $derived(family === 'sub2api');
  const path = $derived(providerIconPath(providerId));
  const resolvedUpstream = $derived(upstreamProvider ?? $sub2ApiUpstreams[providerId] ?? 'codex');
  const upstreamPath = $derived(providerIconPath(resolvedUpstream));
  const upstreamColor = $derived(providerIconColor(resolvedUpstream));
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
    <circle
      cx="72"
      cy="72"
      r="28"
      fill="var(--card)"
      stroke="var(--separator)"
      stroke-width="3.5"
    />
    <g data-icon-layer="upstream" transform="translate(44 44) scale(.56)">
      <path d={upstreamPath} fill={upstreamColor ?? 'currentColor'} />
    </g>
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
