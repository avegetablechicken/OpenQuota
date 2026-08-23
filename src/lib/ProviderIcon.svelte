<script lang="ts">
  import {
    providerFamily,
    providerIconColor,
    providerIconPath,
    providerIconViewBox,
  } from './providerIconPaths';

  interface Props {
    providerId: string;
    size?: number;
  }

  let { providerId, size = 18 }: Props = $props();
  const family = $derived(providerFamily(providerId));
  const composite = $derived(family === 'sub2api');
  const path = $derived(providerIconPath(providerId));
  const codexPath = providerIconPath('codex');
  const color = $derived(providerIconColor(providerId));
  const viewBox = $derived(composite ? '0 0 100 100' : providerIconViewBox(providerId));
</script>

<svg class="provider-icon" width={size} height={size} {viewBox} fill="none" aria-hidden="true">
  {#if composite}
    <g transform="translate(4 4) scale(3.55)">
      <path
        d={path}
        fill="none"
        stroke={color ?? 'currentColor'}
        stroke-width="2.7"
        stroke-linecap="round"
        stroke-linejoin="round"
      />
    </g>
    <circle cx="76" cy="76" r="21" fill="var(--card)" stroke="var(--separator)" stroke-width="4" />
    <g transform="translate(60 60) scale(.32)">
      <path d={codexPath} fill="currentColor" />
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
