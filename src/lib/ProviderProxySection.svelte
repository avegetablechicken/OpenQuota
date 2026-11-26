<script lang="ts">
  import { slide } from 'svelte/transition';
  import { probeProviderProxy } from './backend';
  import { springMotion } from './motion';
  import SelectMenu from './SelectMenu.svelte';
  import type { AppSettings, ProviderLayout, ProxyExitLocation } from './types';

  interface Props {
    settings: AppSettings;
    provider: ProviderLayout;
    onChange: (settings: AppSettings) => void;
    onContentMorph?: () => void;
    reducedMotion?: boolean;
  }

  type ProxyMode = 'system' | 'direct' | 'custom';

  const modeOptions: { value: ProxyMode; label: string }[] = [
    { value: 'system', label: 'System' },
    { value: 'direct', label: 'Direct' },
    { value: 'custom', label: 'Custom' },
  ];

  let {
    settings,
    provider,
    onChange,
    onContentMorph = () => {},
    reducedMotion = false,
  }: Props = $props();
  let expanded = $state(false);
  let mode = $state<ProxyMode>('system');
  const custom = $derived(mode === 'custom');
  let draft = $state('');
  let error = $state('');
  let location = $state<ProxyExitLocation | null>(null);
  let checking = $state(false);
  let probeRevision = $state(0);
  const flag = $derived(
    location
      ? String.fromCodePoint(
          ...Array.from(location.countryCode, (letter) => 0x1f1e6 + letter.charCodeAt(0) - 65),
        )
      : '',
  );
  const locationLabel = $derived(
    location
      ? `${new Intl.DisplayNames(['en'], { type: 'region' }).of(location.countryCode)} · ${location.ip}`
      : '',
  );
  const savedValue = $derived(settings.providerProxies?.[provider.id] ?? '');

  const savedMode = $derived<ProxyMode>(
    !savedValue ? 'system' : savedValue === 'direct' ? 'direct' : 'custom',
  );
  const savedLabel = $derived(modeOptions.find((option) => option.value === savedMode)?.label);

  function resetEditor() {
    mode = savedMode;
    draft = savedValue === 'direct' ? '' : savedValue;
    error = '';
  }

  $effect(resetEditor);

  $effect(() => {
    const proxy = savedValue;
    const providerId = provider.id;
    void probeRevision;
    location = null;
    checking = false;
    if (!custom || !proxy || proxy === 'direct' || error || draft.trim() !== proxy) return;
    let cancelled = false;
    const timer = setTimeout(() => {
      checking = true;
      void probeProviderProxy(providerId, proxy)
        .then((result) => {
          if (
            !cancelled &&
            /^[A-Z]{2}$/.test(result.countryCode) &&
            !['XX', 'ZZ'].includes(result.countryCode)
          ) {
            location = result;
          }
        })
        .catch(() => {
          // Geolocation is supplemental; a lookup failure must not prevent saving a proxy.
        })
        .finally(() => {
          if (!cancelled) checking = false;
        });
    }, 400);
    return () => {
      cancelled = true;
      clearTimeout(timer);
    };
  });

  function saveValue(value: string) {
    if (value === savedValue) return;
    const providerProxies = { ...settings.providerProxies };
    if (value) providerProxies[provider.id] = value;
    else delete providerProxies[provider.id];
    onChange({ ...settings, providerProxies });
  }

  function selectMode(next: ProxyMode) {
    mode = next;
    error = '';
    if (next === 'system') saveValue('');
    else if (next === 'direct') saveValue('direct');
  }

  function commit() {
    if (!custom) return;
    const value = draft.trim();
    error = '';
    if (value) {
      try {
        const url = new URL(value);
        if (
          !['http:', 'https:', 'socks5:', 'socks5h:'].includes(url.protocol) ||
          !url.hostname ||
          url.port === '0' ||
          !['', '/'].includes(url.pathname) ||
          url.search ||
          url.hash
        ) {
          throw new Error('Invalid proxy');
        }
      } catch {
        error =
          'Enter a valid HTTP, HTTPS, SOCKS5 or SOCKS5H proxy URL without a path, query or fragment.';
        return;
      }
    }
    if (!value) mode = 'direct';
    saveValue(value || 'direct');
    probeRevision += 1;
  }
</script>

<section class="provider-proxy-section" aria-label="Proxy">
  <h2>Proxy</h2>
  <div class="provider-proxy-card">
    <div class="proxy-summary">
      <span class="proxy-mode" data-mode={savedMode}>{savedLabel}</span>
      <button
        type="button"
        aria-label={expanded ? 'Done editing proxy' : 'Edit proxy'}
        aria-expanded={expanded}
        aria-controls={expanded ? `provider-proxy-editor-${provider.id}` : undefined}
        onclick={() => {
          if (expanded) {
            if (error) return;
            onContentMorph();
            expanded = false;
            return;
          }
          resetEditor();
          onContentMorph();
          expanded = true;
        }}>{expanded ? 'Done' : 'Edit'}</button
      >
    </div>
    {#if expanded}
      <div
        class="proxy-editor"
        id={`provider-proxy-editor-${provider.id}`}
        transition:slide={springMotion(reducedMotion)}
      >
        <div class="proxy-row">
          <span>Mode</span>
          <SelectMenu
            label="Proxy mode"
            value={mode}
            options={modeOptions}
            onChange={(value) => selectMode(value as ProxyMode)}
          />
        </div>
        {#if custom}
          <div class="proxy-row">
            <label for={`proxy-url-${provider.id}`}>URL</label>
            <div class="proxy-address">
              <input
                id={`proxy-url-${provider.id}`}
                class="proxy-url"
                type="text"
                bind:value={draft}
                placeholder="http://127.0.0.1:7890"
                aria-label="Proxy URL"
                aria-invalid={!!error}
                aria-describedby={error ? `proxy-error-${provider.id}` : undefined}
                autocomplete="off"
                spellcheck="false"
                oninput={() => (error = '')}
                onblur={commit}
                onkeydown={(event) => {
                  if (event.key === 'Enter') event.currentTarget.blur();
                  else if (event.key === 'Escape') {
                    event.preventDefault();
                    event.stopPropagation();
                    resetEditor();
                    event.currentTarget.blur();
                  }
                }}
              />
              {#if location}
                <span
                  class="proxy-location"
                  role="img"
                  aria-label={locationLabel}
                  title={locationLabel}>{flag}</span
                >
              {:else if checking}
                <span
                  class="proxy-checking"
                  role="status"
                  aria-label="Checking proxy exit location"
                  title="Checking proxy exit location">…</span
                >
              {/if}
            </div>
          </div>
        {/if}
        {#if error}<p class="provider-proxy-error" id={`proxy-error-${provider.id}`} role="alert">
            {error}
          </p>{/if}
      </div>
    {/if}
  </div>
</section>

<style>
  .provider-proxy-section {
    margin-bottom: 14px;
  }

  h2 {
    margin: 0 8px 5px;
    color: var(--secondary);
    font-size: 11px;
    font-weight: 600;
  }

  .provider-proxy-card {
    border-radius: 12px;
    background: var(--card);
  }

  .proxy-summary {
    display: flex;
    min-height: 42px;
    align-items: center;
    gap: 10px;
    padding: 8px 12px;
  }

  .proxy-mode {
    display: flex;
    flex: 1;
    align-items: center;
    justify-content: space-between;
    font-size: 13px;
    font-weight: 600;
  }

  .proxy-mode::after {
    width: 6px;
    height: 6px;
    border-radius: 50%;
    background: #8e8e93;
    content: '';
  }

  .proxy-mode[data-mode='system']::after {
    background: #527eae;
  }

  .proxy-mode[data-mode='custom']::after {
    background: #a88800;
    background: light-dark(#a88800, #ffd60a);
  }

  .proxy-summary button {
    padding: 4px 8px;
    border: 0;
    border-radius: 6px;
    color: var(--text);
    background: var(--button-hover);
    font-size: 10px;
  }

  .proxy-editor {
    padding: 3px 14px 4px 12px;
    border-top: 1px solid var(--separator);
  }

  .proxy-row {
    display: grid;
    grid-template-columns: max-content minmax(0, 1fr);
    min-height: 29px;
    align-items: center;
    column-gap: 10px;
    font-size: 12px;
  }

  .proxy-row > :global(.select-menu) {
    justify-self: end;
  }

  .proxy-row + .proxy-row {
    border-top: 1px solid var(--separator);
  }

  .proxy-address {
    display: flex;
    align-items: center;
    gap: 6px;
  }

  .proxy-url {
    min-width: 0;
    height: 25px;
    flex: 1;
    padding: 4px 8px;
    border: 1px solid var(--separator);
    border-radius: 7px;
    color: var(--text);
    background: var(--control);
    font: inherit;
    font-size: 11px;
    outline: none;
  }

  .proxy-url:focus {
    border-color: var(--meter-fill);
    box-shadow: 0 0 0 2px color-mix(in srgb, var(--meter-fill) 20%, transparent);
  }

  .proxy-location,
  .proxy-checking {
    flex: 0 0 auto;
    font-size: 16px;
    line-height: 20px;
  }

  .proxy-url[aria-invalid='true'] {
    border-color: var(--error);
  }

  .provider-proxy-error {
    margin: 2px 0 1px;
    color: var(--error);
    font-size: 10px;
    line-height: 13px;
    overflow-wrap: anywhere;
  }
</style>
