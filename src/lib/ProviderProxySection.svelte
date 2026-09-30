<script lang="ts">
  import { probeProviderProxy } from './backend';
  import SelectMenu from './SelectMenu.svelte';
  import type { AppSettings, ProviderLayout, ProxyExitLocation } from './types';

  interface Props {
    settings: AppSettings;
    provider: ProviderLayout;
    onChange: (settings: AppSettings) => void;
  }

  type ProxyMode = 'system' | 'direct' | 'custom';

  const modeOptions: { value: ProxyMode; label: string }[] = [
    { value: 'system', label: 'System' },
    { value: 'direct', label: 'Direct' },
    { value: 'custom', label: 'Custom' },
  ];

  let { settings, provider, onChange }: Props = $props();
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

  $effect(() => {
    mode = !savedValue ? 'system' : savedValue === 'direct' ? 'direct' : 'custom';
    draft = savedValue === 'direct' ? '' : savedValue;
    error = '';
  });

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
        <span>URL</span>
        <input
          class="proxy-url"
          type="text"
          bind:value={draft}
          placeholder="http://127.0.0.1:7890"
          aria-label="Proxy URL"
          aria-invalid={!!error}
          autocomplete="off"
          spellcheck="false"
          oninput={() => (error = '')}
          onblur={commit}
          onkeydown={(event) => {
            if (event.key === 'Enter') event.currentTarget.blur();
            else if (event.key === 'Escape') {
              event.preventDefault();
              event.stopPropagation();
              draft = savedValue === 'direct' ? '' : savedValue;
              error = '';
              event.currentTarget.blur();
            }
          }}
        />
        {#if location}
          <span class="proxy-location" role="img" aria-label={locationLabel} title={locationLabel}
            >{flag}</span
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
    {/if}
    {#if error}<p class="provider-proxy-error" role="alert">{error}</p>{/if}
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

  .proxy-row {
    display: flex;
    min-height: 42px;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    padding: 7px 12px;
    font-size: 13px;
  }

  .proxy-row + .proxy-row {
    border-top: 1px solid var(--separator);
  }

  .proxy-row > span:first-child {
    flex: 0 0 auto;
  }

  .proxy-url {
    min-width: 0;
    height: 28px;
    flex: 1 1 auto;
    padding: 4px 8px;
    border: 1px solid var(--separator);
    border-radius: 7px;
    color: var(--text);
    background: var(--tray);
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

  p {
    margin: 0 12px 10px;
    font-size: 10px;
    line-height: 14px;
  }

  .provider-proxy-error {
    padding: 6px 7px;
    border-radius: 7px;
    color: var(--error);
    background: var(--error-bg);
  }
</style>
