<script lang="ts">
  import { probeProviderProxy } from './backend';
  import Icon from './Icon.svelte';
  import type { AppSettings, ProviderLayout, ProxyExitLocation } from './types';

  interface Props {
    settings: AppSettings;
    provider: ProviderLayout;
    onChange: (settings: AppSettings) => void;
  }

  let { settings, provider, onChange }: Props = $props();
  let custom = $state(false);
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
    custom = Boolean(savedValue);
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

  function toggleCustom(enabled: boolean) {
    custom = enabled;
    error = '';
    if (enabled) commit();
    else saveValue('');
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
    saveValue(value || 'direct');
    probeRevision += 1;
  }
</script>

<section class="provider-proxy-section" aria-labelledby={`provider-proxy-title-${provider.id}`}>
  <h2 id={`provider-proxy-title-${provider.id}`}>Proxy <span>Optional</span></h2>
  <div class="provider-proxy-card">
    <div class="provider-proxy-summary">
      <Icon name="sliders" size={18} />
      {#if custom}
        <input
          class="proxy-url"
          type="text"
          bind:value={draft}
          placeholder="Leave empty to connect directly"
          aria-label="Proxy URL"
          aria-invalid={!!error}
          autocomplete="off"
          spellcheck="false"
          oninput={() => (error = '')}
          onblur={(event) => {
            if ((event.relatedTarget as HTMLElement | null)?.getAttribute('role') !== 'switch')
              commit();
          }}
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
      {:else}
        <span class="proxy-default">Uses the system proxy by default.</span>
      {/if}
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
      <label class="switch custom-proxy-switch" title="Use custom proxy">
        <input
          type="checkbox"
          role="switch"
          aria-label="Use custom proxy"
          checked={custom}
          onchange={(event) => toggleCustom(event.currentTarget.checked)}
        />
        <span></span>
      </label>
    </div>
    {#if error}<p class="provider-proxy-error" role="alert">{error}</p>{/if}
  </div>
</section>

<style>
  .provider-proxy-section {
    margin-bottom: 14px;
  }

  h2 {
    display: flex;
    align-items: baseline;
    gap: 6px;
    margin: 0 8px 5px;
    color: var(--secondary);
    font-size: 11px;
    font-weight: 600;
  }

  h2 span {
    color: var(--tertiary);
    font-size: 10px;
    font-weight: 400;
  }

  .provider-proxy-card {
    overflow: hidden;
    border-radius: 12px;
    background: var(--card);
  }

  .provider-proxy-summary {
    display: flex;
    min-height: 42px;
    align-items: center;
    gap: 10px;
    padding: 8px 12px;
    color: var(--secondary);
  }

  .proxy-url {
    min-width: 0;
    height: 28px;
    flex: 1;
    padding: 4px 8px;
    border: 1px solid var(--separator);
    border-radius: 7px;
    color: var(--text);
    background: var(--tray);
    font: inherit;
    font-size: 11px;
    outline: none;
  }

  .proxy-default {
    min-width: 0;
    flex: 1;
    font-size: 11px;
    line-height: 16px;
    color: var(--secondary);
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

  .custom-proxy-switch {
    position: relative;
    display: block;
    width: 28px;
    height: 16px;
    flex: 0 0 28px;
  }

  .custom-proxy-switch input {
    position: absolute;
    z-index: 1;
    inset: 0;
    width: 100%;
    height: 100%;
    margin: 0;
    border: 0;
    padding: 0;
    opacity: 0;
    cursor: pointer;
  }

  p {
    margin: 0 12px 10px;
    color: var(--secondary);
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
