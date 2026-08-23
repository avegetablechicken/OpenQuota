<script lang="ts">
  import { onMount } from 'svelte';
  import { flip } from 'svelte/animate';
  import { getSub2ApiConfigState } from './backend';
  import type { AppSettings, ProviderLayout } from './types';
  import type { ProviderCatalogIndex } from './metrics';
  import Icon from './Icon.svelte';
  import ProviderIcon from './ProviderIcon.svelte';
  import { reorderFlip } from './motion';
  import { pointerReorder } from './pointerReorder';
  import { providerFamily } from './providerIconPaths';
  import {
    rememberSub2ApiUpstream,
    sub2ApiDisplayName,
    sub2ApiEndpoints,
    sub2ApiUpstreams,
  } from './sub2ApiUpstreams';

  interface Props {
    settings: AppSettings;
    catalog: ProviderCatalogIndex;
    onOpen: (providerId: string) => void;
    onChange: (settings: AppSettings) => void;
    onReorderStart: () => void;
    onReorderEnd: (moved: boolean, cancelled?: boolean) => void;
    onSettings: () => void;
    reducedMotion: boolean;
  }
  let {
    settings,
    catalog,
    onOpen,
    onChange,
    onReorderStart,
    onReorderEnd,
    onSettings,
    reducedMotion,
  }: Props = $props();
  const providerDisplayName = (id: string) => catalog.displayName(id, settings.providerNames);
  function providerListName(id: string) {
    return sub2ApiDisplayName(id, $sub2ApiUpstreams[id]) ?? providerDisplayName(id);
  }
  const visibleProviders = $derived(
    settings.providers.filter(
      (provider) =>
        catalog.provider(provider.id) &&
        (!catalog.supportsConnectionConfiguration(provider.id) ||
          provider.enabled ||
          provider.detected),
    ),
  );
  const repeatedSub2ApiUpstreams = $derived.by(() => {
    const upstreams = visibleProviders.flatMap((provider) => {
      if (providerFamily(provider.id) !== 'sub2api' || !$sub2ApiEndpoints[provider.id]) return [];
      const upstream = $sub2ApiUpstreams[provider.id];
      return upstream ? [upstream] : [];
    });
    return upstreams.filter((upstream, index) => upstreams.indexOf(upstream) !== index);
  });
  function providerListSubtitle(provider: ProviderLayout) {
    const upstream = $sub2ApiUpstreams[provider.id];
    if (upstream && repeatedSub2ApiUpstreams.includes(upstream)) {
      return $sub2ApiEndpoints[provider.id] ?? `${provider.metrics.length} metrics`;
    }
    return `${provider.metrics.length} metrics`;
  }
  const addableProvider = $derived(
    settings.providers.find(
      (provider) =>
        catalog.provider(provider.id) &&
        catalog.supportsConnectionConfiguration(provider.id) &&
        !provider.enabled &&
        !provider.detected,
    ),
  );
  const addableProviderName = $derived(
    addableProvider ? catalog.connectionConfigurationLabel(addableProvider.id) : '',
  );
  function updateProvider(provider: ProviderLayout) {
    onChange({
      ...settings,
      providers: settings.providers.map((item) => (item.id === provider.id ? provider : item)),
    });
  }
  function addProvider(provider: ProviderLayout) {
    updateProvider({ ...provider, enabled: true });
    onOpen(provider.id);
  }
  function reorder(draggedId: string, targetId: string) {
    if (draggedId === targetId) return;
    const enabled = settings.providers.filter((provider) => provider.enabled);
    const from = enabled.findIndex((provider) => provider.id === draggedId);
    const to = enabled.findIndex((provider) => provider.id === targetId);
    if (from < 0 || to < 0) return;
    const [provider] = enabled.splice(from, 1);
    enabled.splice(to, 0, provider);
    onChange({
      ...settings,
      providers: [...enabled, ...settings.providers.filter((provider) => !provider.enabled)],
    });
  }
  onMount(() => {
    for (const provider of visibleProviders) {
      if (providerFamily(provider.id) !== 'sub2api' || $sub2ApiEndpoints[provider.id]) continue;
      void getSub2ApiConfigState(provider.id)
        .then((state) => {
          if (state.configured) {
            rememberSub2ApiUpstream(provider.id, state.upstream, state.baseUrl);
          }
        })
        .catch(() => undefined);
    }
  });
</script>

<section class="screen customize-screen" aria-label="Customize">
  <div class="customize-list" role="list">
    {#each visibleProviders as provider (provider.id)}
      <div
        role="listitem"
        class:inactive={!provider.enabled}
        class="provider-list-row"
        data-reorder-group={provider.enabled ? 'customize-providers' : undefined}
        data-reorder-id={provider.enabled ? provider.id : undefined}
        use:pointerReorder={{
          id: provider.id,
          group: 'customize-providers',
          label: providerListName(provider.id),
          disabled: !provider.enabled,
          gripOnly: true,
          touchGripOnly: true,
          onReorder: (targetId) => reorder(provider.id, targetId),
          onStart: onReorderStart,
          onEnd: onReorderEnd,
        }}
        animate:flip={reorderFlip(reducedMotion)}
      >
        <span
          class="reorder-grip"
          data-reorder-handle
          data-reorder-touch-handle
          role="button"
          tabindex={provider.enabled ? 0 : undefined}
          aria-label={`Move ${providerListName(provider.id)}`}
          aria-describedby="reorder-instructions"
          aria-keyshortcuts="Alt+ArrowUp Alt+ArrowDown"
          ><Icon name="grip-lines" size={16} strokeWidth={2} /></span
        >
        <button class="provider-list-main" type="button" onclick={() => onOpen(provider.id)}
          ><ProviderIcon providerId={provider.id} /><span
            ><b>{providerListName(provider.id)}</b><small>{providerListSubtitle(provider)}</small
            ></span
          ></button
        >
        <label class="switch"
          ><input
            aria-label={`Enable ${provider.id}`}
            type="checkbox"
            checked={provider.enabled}
            onchange={(event) =>
              updateProvider({ ...provider, enabled: event.currentTarget.checked })}
          /><span></span></label
        >
        <button
          class="chevron"
          type="button"
          aria-label={`Customize ${provider.id}`}
          onclick={() => onOpen(provider.id)}
          ><Icon name="chevron-right" size={13} strokeWidth={2.2} /></button
        >
      </div>
    {/each}
    {#if addableProvider}
      <div class="provider-add-shell" role="listitem">
        <button
          class="provider-add-row"
          type="button"
          aria-label={`Add ${addableProviderName}`}
          onclick={() => addProvider(addableProvider)}
        >
          <span class="provider-add-icon"><Icon name="plus" size={17} strokeWidth={2} /></span>
          <span>{addableProviderName}</span>
        </button>
      </div>
    {/if}
  </div>
  <button class="screen-cross-link" type="button" aria-label="Settings" onclick={onSettings}>
    <Icon name="gear" size={17} />
    <span><b>Settings</b><small>Notifications, appearance and more</small></span>
    <Icon name="chevron-right" size={13} strokeWidth={2.2} />
  </button>
</section>

<style>
  :global {
    .provider-list-row {
      display: flex;
      min-height: 52px;
      align-items: center;
      gap: 5px;
      padding: 5px 7px;
      border-top: 1px solid var(--separator);
    }

    .provider-list-row:first-child {
      border-top: 0;
    }

    .provider-list-row.inactive {
      opacity: 0.55;
    }

    .provider-add-shell {
      border-top: 1px solid var(--separator);
    }

    .provider-add-row {
      display: flex;
      width: 100%;
      min-height: 42px;
      align-items: center;
      gap: 10px;
      padding: 9px 12px;
      border: 0;
      color: var(--secondary);
      background: transparent;
      font-size: 14px;
      font-weight: 600;
      text-align: left;
    }

    .provider-add-row:hover {
      color: var(--text);
      background: var(--button-hover);
    }

    .provider-add-icon {
      display: grid;
      width: 18px;
      height: 18px;
      place-items: center;
      border-radius: 50%;
      color: var(--tray);
      background: var(--meter-fill);
    }

    .reorder-grip {
      position: relative;
      color: var(--tertiary);
      cursor: grab;
      font-size: 16px;
    }

    .reorder-grip::after {
      position: absolute;
      inset: -10px -8px;
      content: '';
    }

    .provider-list-main {
      display: flex;
      min-width: 0;
      flex: 1;
      align-items: center;
      flex-direction: row;
      gap: 10px;
      padding: 4px;
      border: 0;
      color: var(--text);
      background: none;
      text-align: left;
    }

    .provider-list-main > span {
      display: flex;
      min-width: 0;
      flex: 1;
      flex-direction: column;
    }

    .provider-list-main b,
    .provider-list-main small {
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .provider-list-main b {
      font-size: 13px;
    }

    .provider-list-main small {
      color: var(--secondary);
      font-size: 9px;
    }

    .provider-list-row {
      min-height: 42px;
      gap: 10px;
      padding: 9px 12px;
      border-top-color: var(--separator);
    }

    .provider-list-row > .provider-icon {
      color: var(--text);
    }

    .provider-list-main b {
      font-size: 14px;
      font-weight: 600;
    }

    .provider-list-main small {
      font-size: 11px;
    }

    .switch input {
      position: absolute;
    }

    .switch span {
      width: 28px;
      height: 16px;
    }

    .chevron {
      font-size: 18px;
    }
  }
</style>
