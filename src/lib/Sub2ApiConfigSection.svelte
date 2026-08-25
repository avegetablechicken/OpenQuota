<script lang="ts">
  import { onDestroy, onMount, tick } from 'svelte';
  import {
    clearSub2ApiConfig,
    deleteSub2ApiConfig,
    getSub2ApiConfigState,
    resolveSub2ApiClaudeBaseUrl,
    resolveSub2ApiCodexProvider,
    saveSub2ApiConfig,
  } from './backend';
  import Icon from './Icon.svelte';
  import ProviderIcon from './ProviderIcon.svelte';
  import { saveShortcut } from './saveShortcut';
  import { forgetSub2ApiUpstream, rememberSub2ApiUpstream } from './sub2ApiUpstreams';
  import type { Sub2ApiConfigState, Sub2ApiUpstream } from './types';

  interface Props {
    providerId: string;
    onRemove?: () => void;
  }

  let { providerId, onRemove = () => {} }: Props = $props();

  let connectionState = $state<Sub2ApiConfigState>({
    configured: false,
    baseUrl: '',
    codexProvider: '',
    customBaseUrl: false,
    email: '',
    upstream: 'codex',
  });
  let open = $state(false);
  let baseUrl = $state('');
  let codexProvider = $state('');
  let customBaseUrl = $state(false);
  let email = $state('');
  let password = $state('');
  let upstream = $state<Sub2ApiUpstream>('codex');
  let revealPassword = $state(false);
  let saving = $state(false);
  let confirmingClear = $state(false);
  let confirmingItemRemoval = $state(false);
  let error = $state<string | null>(null);
  let providerError = $state<string | null>(null);
  let resolvingProvider = $state(false);
  let providerResolution = 0;
  let providerResolutionTimer: number | undefined;
  let claudeBaseUrlError = $state<string | null>(null);
  let resolvingClaudeBaseUrl = $state(false);
  let claudeBaseUrlResolution = 0;
  let toggleButton = $state<HTMLButtonElement>();
  let clearButton = $state<HTMLButtonElement>();
  let clearCancelButton = $state<HTMLButtonElement>();
  let removeItemButton = $state<HTMLButtonElement>();
  let removeItemCancelButton = $state<HTMLButtonElement>();
  const upstreamOptions: Sub2ApiUpstream[] = ['codex', 'claude'];
  const providerResolutionDelayMs = 300;

  const endpointReady = $derived(
    !customBaseUrl
      ? upstream === 'codex'
        ? Boolean(codexProvider.trim() && baseUrl.trim() && !providerError && !resolvingProvider)
        : Boolean(baseUrl.trim() && !claudeBaseUrlError && !resolvingClaudeBaseUrl)
      : Boolean(baseUrl.trim()),
  );
  const canSave = $derived(
    Boolean(
      endpointReady && email.trim() && (connectionState.configured || password.trim()) && !saving,
    ),
  );

  function errorMessage(cause: unknown, fallback: string) {
    if (typeof cause === 'string') return cause;
    if (cause instanceof Error && cause.message) return cause.message;
    return fallback;
  }

  function cancelProviderResolution() {
    if (providerResolutionTimer !== undefined) window.clearTimeout(providerResolutionTimer);
    providerResolutionTimer = undefined;
  }

  function resetEditor(next = connectionState) {
    baseUrl = next.baseUrl;
    codexProvider = next.codexProvider ?? '';
    customBaseUrl = Boolean(next.customBaseUrl);
    email = next.email;
    upstream = next.upstream;
    password = '';
    revealPassword = false;
    confirmingClear = false;
    error = null;
    providerError = null;
    resolvingProvider = false;
    cancelProviderResolution();
    providerResolution += 1;
    claudeBaseUrlError = null;
    resolvingClaudeBaseUrl = false;
    claudeBaseUrlResolution += 1;
  }

  function syncRememberedUpstream(next: Sub2ApiConfigState) {
    if (next.configured) rememberSub2ApiUpstream(providerId, next.upstream, next.baseUrl);
    else forgetSub2ApiUpstream(providerId);
  }

  function toggleEditor() {
    open = !open;
    if (open) {
      resetEditor();
      if (upstream === 'claude' && !customBaseUrl) resolveClaudeBaseUrl();
    } else {
      cancelProviderResolution();
      resolvingProvider = false;
      providerResolution += 1;
      resolvingClaudeBaseUrl = false;
      claudeBaseUrlResolution += 1;
    }
  }

  function selectUpstream(next: Sub2ApiUpstream) {
    if (upstream === next) return;
    upstream = next;
    baseUrl = '';
    codexProvider = '';
    customBaseUrl = false;
    providerError = null;
    resolvingProvider = false;
    cancelProviderResolution();
    providerResolution += 1;
    claudeBaseUrlError = null;
    resolvingClaudeBaseUrl = false;
    claudeBaseUrlResolution += 1;
    if (next === 'claude') resolveClaudeBaseUrl();
  }

  function setCustomBaseUrl(next: boolean) {
    customBaseUrl = next;
    baseUrl = '';
    codexProvider = '';
    providerError = null;
    resolvingProvider = false;
    cancelProviderResolution();
    providerResolution += 1;
    claudeBaseUrlError = null;
    resolvingClaudeBaseUrl = false;
    claudeBaseUrlResolution += 1;
    if (!next && upstream === 'claude') resolveClaudeBaseUrl();
  }

  function updateCodexProvider(value: string) {
    codexProvider = value;
    baseUrl = '';
    providerError = null;
    cancelProviderResolution();
    const candidate = value.trim();
    const resolution = ++providerResolution;
    if (!candidate || upstream !== 'codex' || customBaseUrl) {
      resolvingProvider = false;
      return;
    }
    resolvingProvider = true;
    providerResolutionTimer = window.setTimeout(() => {
      providerResolutionTimer = undefined;
      void resolveSub2ApiCodexProvider(candidate)
        .then((resolved) => {
          if (resolution === providerResolution) baseUrl = resolved;
        })
        .catch((cause) => {
          if (resolution === providerResolution) {
            providerError = errorMessage(cause, 'The Codex provider could not be resolved.');
          }
        })
        .finally(() => {
          if (resolution === providerResolution) resolvingProvider = false;
        });
    }, providerResolutionDelayMs);
  }

  function resolveClaudeBaseUrl() {
    const resolution = ++claudeBaseUrlResolution;
    baseUrl = '';
    claudeBaseUrlError = null;
    resolvingClaudeBaseUrl = true;
    void resolveSub2ApiClaudeBaseUrl()
      .then((resolved) => {
        if (resolution === claudeBaseUrlResolution) baseUrl = resolved;
      })
      .catch((cause) => {
        if (resolution === claudeBaseUrlResolution) {
          claudeBaseUrlError = errorMessage(cause, 'The Claude Base URL could not be resolved.');
        }
      })
      .finally(() => {
        if (resolution === claudeBaseUrlResolution) resolvingClaudeBaseUrl = false;
      });
  }

  async function save() {
    if (!canSave) return;
    const previousState = connectionState;
    const submitted = {
      baseUrl: baseUrl.trim(),
      codexProvider: codexProvider.trim(),
      customBaseUrl,
      email: email.trim(),
      password,
      upstream,
    };
    const optimisticState: Sub2ApiConfigState = {
      configured: true,
      baseUrl: submitted.baseUrl,
      codexProvider: submitted.codexProvider,
      customBaseUrl: submitted.customBaseUrl,
      email: submitted.email,
      upstream: submitted.upstream,
    };
    saving = true;
    error = null;
    connectionState = optimisticState;
    syncRememberedUpstream(optimisticState);
    resetEditor(optimisticState);
    open = false;
    await tick();
    toggleButton?.focus();
    try {
      connectionState = await saveSub2ApiConfig(providerId, submitted);
      syncRememberedUpstream(connectionState);
    } catch (cause) {
      connectionState = previousState;
      syncRememberedUpstream(previousState);
      baseUrl = submitted.baseUrl;
      codexProvider = submitted.codexProvider;
      customBaseUrl = submitted.customBaseUrl;
      email = submitted.email;
      upstream = submitted.upstream;
      revealPassword = false;
      confirmingClear = false;
      open = true;
      error = errorMessage(cause, 'The Sub2API connection could not be saved.');
      await tick();
    } finally {
      password = '';
      saving = false;
    }
  }

  async function clearConnection() {
    if (saving) return;
    saving = true;
    error = null;
    try {
      connectionState = await clearSub2ApiConfig(providerId);
      syncRememberedUpstream(connectionState);
      resetEditor(connectionState);
      await tick();
      clearButton?.focus();
    } catch (cause) {
      error = errorMessage(cause, 'The Sub2API connection could not be cleared.');
    } finally {
      saving = false;
    }
  }

  async function removeItem() {
    if (saving) return;
    saving = true;
    error = null;
    try {
      connectionState = await deleteSub2ApiConfig(providerId);
      forgetSub2ApiUpstream(providerId);
      resetEditor(connectionState);
      confirmingItemRemoval = false;
      open = false;
      onRemove();
    } catch (cause) {
      error = errorMessage(cause, 'The Sub2API item could not be removed.');
    } finally {
      saving = false;
    }
  }

  async function requestClear() {
    confirmingClear = true;
    error = null;
    await tick();
    clearCancelButton?.focus();
  }

  async function cancelClear() {
    confirmingClear = false;
    await tick();
    clearButton?.focus();
  }

  async function requestItemRemoval() {
    confirmingItemRemoval = true;
    error = null;
    await tick();
    removeItemCancelButton?.focus();
  }

  async function cancelItemRemoval() {
    confirmingItemRemoval = false;
    await tick();
    removeItemButton?.focus();
  }

  function handleClearKeydown(event: KeyboardEvent) {
    if (event.key !== 'Escape' || saving) return;
    event.preventDefault();
    event.stopPropagation();
    void cancelClear();
  }

  function handleItemRemovalKeydown(event: KeyboardEvent) {
    if (event.key !== 'Escape' || saving) return;
    event.preventDefault();
    event.stopPropagation();
    void cancelItemRemoval();
  }

  onMount(() => {
    void getSub2ApiConfigState(providerId)
      .then((next) => {
        connectionState = next;
        syncRememberedUpstream(next);
        resetEditor(next);
        if (next.upstream === 'claude' && !next.customBaseUrl) resolveClaudeBaseUrl();
      })
      .catch((cause) => {
        error = errorMessage(cause, 'The Sub2API connection could not be read.');
        open = true;
      });
  });

  onDestroy(() => {
    cancelProviderResolution();
    providerResolution += 1;
    claudeBaseUrlResolution += 1;
  });
</script>

<section class="sub2api-config-section" aria-label="Sub2API Connection">
  <h2>Connection</h2>
  <div class="sub2api-config-card">
    <div class="sub2api-config-summary">
      <ProviderIcon
        {providerId}
        upstreamProvider={connectionState.configured ? connectionState.upstream : null}
        size={20}
      />
      <span>
        <b>Account</b>
        <small>{connectionState.configured ? connectionState.email : 'Not configured'}</small>
      </span>
      <i class:missing={!connectionState.configured} aria-hidden="true"></i>
      <button bind:this={toggleButton} type="button" onclick={toggleEditor}
        >{open ? 'Done' : connectionState.configured ? 'Edit' : 'Add'}</button
      >
    </div>

    {#if open}
      <div class="sub2api-config-editor">
        <fieldset class="upstream-field">
          <legend>Upstream</legend>
          <div class="upstream-options">
            {#each upstreamOptions as option (option)}
              <label class:active={upstream === option}>
                <input
                  type="radio"
                  name={`${providerId}-upstream`}
                  value={option}
                  checked={upstream === option}
                  disabled={saving}
                  onchange={() => selectUpstream(option)}
                />
                <ProviderIcon providerId={option} size={15} />
                <span>{option === 'claude' ? 'Claude' : 'Codex'}</span>
              </label>
            {/each}
          </div>
        </fieldset>
        {#if upstream === 'codex'}
          <label>
            <span>Provider</span>
            <input
              type="text"
              value={codexProvider}
              autocomplete="off"
              spellcheck="false"
              placeholder="Provider or profile"
              aria-label="Codex provider or profile"
              disabled={saving || customBaseUrl}
              oninput={(event) => updateCodexProvider(event.currentTarget.value)}
            />
          </label>
          {#if providerError}<div class="config-error" role="alert">{providerError}</div>{/if}
        {/if}
        <div class="custom-base-url-row">
          <span>Custom Base URL</span>
          <label class="switch">
            <input
              type="checkbox"
              role="switch"
              aria-label="Use custom Base URL"
              checked={customBaseUrl}
              disabled={saving}
              onchange={(event) => setCustomBaseUrl(event.currentTarget.checked)}
            />
            <span></span>
          </label>
        </div>
        {#if upstream === 'claude' && claudeBaseUrlError}
          <div class="config-error" role="alert">{claudeBaseUrlError}</div>
        {/if}
        <label>
          <span>Base URL</span>
          <input
            type="url"
            bind:value={baseUrl}
            autocomplete="url"
            spellcheck="false"
            placeholder="https://sub2api.example.com"
            aria-label="Base URL"
            disabled={saving || !customBaseUrl}
          />
        </label>
        <label>
          <span>Email</span>
          <input
            type="email"
            bind:value={email}
            autocomplete="username"
            spellcheck="false"
            placeholder="admin@example.com"
            aria-label="Sub2API administrator email"
            disabled={saving}
          />
        </label>
        <label>
          <span>Password</span>
          <div class="password-field">
            <input
              type={revealPassword ? 'text' : 'password'}
              bind:value={password}
              autocomplete="current-password"
              placeholder={connectionState.configured
                ? 'Leave blank to keep saved password'
                : 'Password'}
              aria-label="Sub2API administrator password"
              disabled={saving}
            />
            <button
              type="button"
              aria-label={revealPassword ? 'Hide password' : 'Show password'}
              onclick={() => (revealPassword = !revealPassword)}
            >
              <Icon name={revealPassword ? 'eye-off' : 'eye'} size={15} />
            </button>
          </div>
        </label>

        <div class="sub2api-config-actions">
          <button
            class="primary"
            type="button"
            disabled={!canSave}
            use:saveShortcut={canSave}
            onclick={() => void save()}>{saving ? 'Saving…' : 'Save'}</button
          >
          {#if connectionState.configured}
            <button
              bind:this={clearButton}
              class="clear"
              type="button"
              disabled={saving || confirmingClear}
              aria-label="Clear Sub2API connection"
              title="Clear Sub2API connection"
              onclick={() => void requestClear()}
            >
              <Icon name="clear-filled" size={15} />
            </button>
          {/if}
        </div>

        {#if confirmingClear}
          <div
            class="clear-confirm"
            role="group"
            aria-labelledby="clear-sub2api-title"
            aria-describedby="clear-sub2api-message"
          >
            <strong id="clear-sub2api-title">Clear Sub2API connection?</strong>
            <span id="clear-sub2api-message"
              >The Base URL and saved login will be removed. This Sub2API item will remain.</span
            >
            <div>
              <button
                bind:this={clearCancelButton}
                type="button"
                disabled={saving}
                onkeydown={handleClearKeydown}
                onclick={() => void cancelClear()}>Cancel</button
              >
              <button
                class="destructive"
                type="button"
                disabled={saving}
                onkeydown={handleClearKeydown}
                onclick={() => void clearConnection()}>{saving ? 'Clearing…' : 'Clear'}</button
              >
            </div>
          </div>
        {/if}
        {#if error}<div class="config-error" role="alert">{error}</div>{/if}
      </div>
    {:else if error}
      <div class="config-error summary-error" role="alert">{error}</div>
    {/if}
  </div>
</section>

<section class="sub2api-item-actions" aria-label="Sub2API Item">
  <button
    bind:this={removeItemButton}
    class="remove-item-row"
    type="button"
    disabled={saving || confirmingItemRemoval}
    onclick={() => void requestItemRemoval()}
  >
    <Icon name="clear-filled" size={17} />
    <span>
      <b>Delete</b>
      <small>Remove this provider and its connection</small>
    </span>
    <Icon name="chevron-right" size={13} strokeWidth={2.2} />
  </button>

  {#if confirmingItemRemoval}
    <div
      class="remove-item-confirm"
      role="group"
      aria-labelledby="remove-sub2api-item-title"
      aria-describedby="remove-sub2api-item-message"
    >
      <strong id="remove-sub2api-item-title">Delete this Sub2API item?</strong>
      <span id="remove-sub2api-item-message"
        >{connectionState.configured
          ? 'The provider and its saved login will be removed.'
          : 'This empty configuration item will be removed.'}</span
      >
      <div>
        <button
          bind:this={removeItemCancelButton}
          type="button"
          disabled={saving}
          onkeydown={handleItemRemovalKeydown}
          onclick={() => void cancelItemRemoval()}>Cancel</button
        >
        <button
          class="destructive"
          type="button"
          disabled={saving}
          onkeydown={handleItemRemovalKeydown}
          onclick={() => void removeItem()}>{saving ? 'Deleting…' : 'Delete'}</button
        >
      </div>
    </div>
  {/if}
</section>

<style>
  .sub2api-config-section {
    margin-bottom: 14px;
  }

  .sub2api-config-section h2 {
    margin: 0 8px 5px;
    color: var(--secondary);
    font-size: 11px;
    font-weight: 600;
  }

  .sub2api-config-card {
    overflow: hidden;
    border-radius: 12px;
    background: var(--card);
  }

  .sub2api-config-summary {
    display: flex;
    min-height: 42px;
    align-items: center;
    gap: 10px;
    padding: 8px 12px;
  }

  .sub2api-config-summary > span {
    display: flex;
    min-width: 0;
    flex: 1;
    flex-direction: column;
  }

  .sub2api-config-summary b {
    overflow: hidden;
    font-size: 13px;
    font-weight: 600;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .sub2api-config-summary small {
    overflow: hidden;
    color: var(--secondary);
    font-size: 10px;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .sub2api-config-summary i {
    width: 7px;
    height: 7px;
    border-radius: 50%;
    background: #34c759;
  }

  .sub2api-config-summary i.missing {
    background: var(--meter-critical);
  }

  .sub2api-config-summary > button,
  .sub2api-config-actions button,
  .clear-confirm button,
  .remove-item-confirm button {
    min-height: 26px;
    border: 0;
    border-radius: 6px;
    color: var(--text);
    background: var(--button-hover);
    font-size: 11px;
    font-weight: 600;
  }

  .sub2api-config-summary > button {
    padding: 0 10px;
  }

  .sub2api-config-editor {
    display: grid;
    gap: 10px;
    padding: 11px 12px 12px;
    border-top: 1px solid var(--separator);
  }

  .sub2api-config-editor > label {
    display: grid;
    gap: 4px;
  }

  .upstream-field {
    display: grid;
    gap: 4px;
    min-width: 0;
    margin: 0;
    border: 0;
    padding: 0;
  }

  .upstream-field legend {
    padding: 0;
    color: var(--secondary);
    font-size: 10px;
    font-weight: 600;
  }

  .upstream-options {
    display: grid;
    grid-template-columns: repeat(2, minmax(0, 1fr));
    gap: 2px;
    border-radius: 7px;
    padding: 2px;
    background: var(--tray);
  }

  .upstream-options label {
    position: relative;
    display: flex;
    min-width: 0;
    height: 28px;
    align-items: center;
    justify-content: center;
    gap: 6px;
    border-radius: 5px;
    color: var(--secondary);
    font-size: 11px;
    font-weight: 600;
  }

  .upstream-options label.active {
    color: var(--text);
    background: var(--button-hover);
  }

  .upstream-options input {
    position: absolute;
    width: 1px;
    height: 1px;
    opacity: 0;
  }

  .upstream-options label:has(input:focus-visible) {
    outline: 2px solid color-mix(in srgb, var(--meter-fill) 35%, transparent);
  }

  .sub2api-config-editor > label > span {
    color: var(--secondary);
    font-size: 10px;
    font-weight: 600;
  }

  .custom-base-url-row {
    display: flex;
    min-height: 24px;
    align-items: center;
    justify-content: space-between;
    color: var(--secondary);
    font-size: 10px;
    font-weight: 600;
  }

  .custom-base-url-row .switch input {
    position: absolute;
    width: 1px;
    height: 1px;
    opacity: 0;
  }

  .sub2api-config-editor input {
    box-sizing: border-box;
    width: 100%;
    height: 32px;
    border: 1px solid var(--separator);
    border-radius: 6px;
    outline: none;
    padding: 0 9px;
    color: var(--text);
    background: var(--tray);
    font: inherit;
    font-size: 12px;
  }

  .sub2api-config-editor input:focus {
    border-color: var(--meter-fill);
    box-shadow: 0 0 0 2px color-mix(in srgb, var(--meter-fill) 20%, transparent);
  }

  .password-field {
    position: relative;
  }

  .password-field input {
    padding-right: 34px;
  }

  .password-field button {
    display: grid;
    width: 28px;
    height: 28px;
    place-items: center;
    border: 0;
    color: var(--secondary);
    background: transparent;
  }

  .password-field button {
    position: absolute;
    top: 2px;
    right: 2px;
  }

  .sub2api-config-actions {
    display: flex;
    justify-content: flex-end;
    gap: 6px;
  }

  .sub2api-config-actions .primary {
    min-width: 62px;
    padding: 0 12px;
    color: white;
    background: var(--meter-fill);
  }

  .sub2api-config-actions .clear {
    display: grid;
    width: 28px;
    height: 28px;
    padding: 0;
    place-items: center;
    color: var(--secondary);
    background: transparent;
  }

  .sub2api-config-actions button:disabled,
  .clear-confirm button:disabled,
  .remove-item-confirm button:disabled,
  .remove-item-row:disabled {
    opacity: 0.5;
  }

  .clear-confirm,
  .remove-item-confirm {
    display: grid;
    gap: 7px;
    border-radius: 7px;
    padding: 9px;
    background: color-mix(in srgb, var(--meter-critical) 8%, transparent);
    font-size: 11px;
  }

  .clear-confirm strong,
  .remove-item-confirm strong {
    font-size: 11px;
  }

  .clear-confirm span,
  .remove-item-confirm span {
    color: var(--secondary);
    line-height: 15px;
  }

  .clear-confirm > div,
  .remove-item-confirm > div {
    display: flex;
    justify-content: flex-end;
    gap: 6px;
  }

  .clear-confirm button,
  .remove-item-confirm button {
    padding: 0 10px;
  }

  .clear-confirm .destructive,
  .remove-item-confirm .destructive {
    color: var(--error);
  }

  .sub2api-item-actions {
    margin-bottom: 14px;
  }

  .remove-item-row {
    display: flex;
    width: 100%;
    min-height: 48px;
    align-items: center;
    gap: 10px;
    padding: 9px 12px;
    border: 0;
    border-radius: 12px;
    color: var(--error);
    background: var(--card);
    text-align: left;
  }

  .remove-item-row > span {
    display: flex;
    min-width: 0;
    flex: 1;
    flex-direction: column;
  }

  .remove-item-row b {
    font-size: 13px;
    font-weight: 600;
  }

  .remove-item-row small {
    color: var(--secondary);
    font-size: 10px;
  }

  .remove-item-confirm {
    margin-top: 6px;
  }

  .config-error {
    color: var(--error);
    font-size: 10px;
    line-height: 14px;
  }

  .summary-error {
    padding: 0 12px 10px 42px;
  }
</style>
