<script lang="ts">
  import { onDestroy, onMount, tick, type Snippet } from 'svelte';
  import {
    clearSub2ApiConfig,
    deleteSub2ApiConfig,
    getSub2ApiConfigState,
    resolveSub2ApiCodexProvider,
    resolveSub2ApiClaudeProvider,
    saveSub2ApiConfig,
  } from './backend';
  import Icon from './Icon.svelte';
  import ProviderIcon from './ProviderIcon.svelte';
  import SelectMenu from './SelectMenu.svelte';
  import { saveShortcut } from './saveShortcut';
  import {
    forgetSub2ApiUpstream,
    rememberSub2ApiUpstream,
    sub2ApiPublicName,
    sub2ApiUpstreamLabel,
  } from './sub2ApiUpstreams';
  import type { Sub2ApiConfigState, Sub2ApiUpstream } from './types';

  interface Props {
    providerId: string;
    onRemove?: () => void;
    children?: Snippet;
  }

  interface ConnectionDraft {
    baseUrl: string;
    providerName: string;
    customBaseUrl: boolean;
    email: string;
    password: string;
    revealPassword: boolean;
  }

  function emptyConnectionDraft(): ConnectionDraft {
    return {
      baseUrl: '',
      providerName: '',
      customBaseUrl: false,
      email: '',
      password: '',
      revealPassword: false,
    };
  }

  let { providerId, onRemove = () => {}, children }: Props = $props();

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
  let providerName = $state('');
  let customBaseUrl = $state(false);
  let email = $state('');
  let password = $state('');
  let upstream = $state<Sub2ApiUpstream>('codex');
  let revealPassword = $state(false);
  let upstreamDrafts = $state<Record<Sub2ApiUpstream, ConnectionDraft>>({
    codex: emptyConnectionDraft(),
    claude: emptyConnectionDraft(),
  });
  let saving = $state(false);
  let confirmingClear = $state(false);
  let confirmingItemRemoval = $state(false);
  let error = $state<string | null>(null);
  let providerError = $state<string | null>(null);
  let resolvingProvider = $state(false);
  let providerResolution = 0;
  let providerResolutionTimer: number | undefined;
  let toggleButton = $state<HTMLButtonElement>();
  let clearButton = $state<HTMLButtonElement>();
  let clearCancelButton = $state<HTMLButtonElement>();
  let removeItemButton = $state<HTMLButtonElement>();
  let removeItemCancelButton = $state<HTMLButtonElement>();
  const upstreamOptions: Sub2ApiUpstream[] = ['codex', 'claude'];
  const providerResolutionDelayMs = 300;

  const endpointReady = $derived(
    !customBaseUrl
      ? Boolean(providerName.trim() && baseUrl.trim() && !providerError && !resolvingProvider)
      : Boolean(baseUrl.trim()),
  );
  const canReuseSavedPassword = $derived(
    connectionState.configured && upstream === connectionState.upstream,
  );
  const canSave = $derived(
    Boolean(endpointReady && email.trim() && (canReuseSavedPassword || password.trim()) && !saving),
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

  function currentDraft(): ConnectionDraft {
    return { baseUrl, providerName, customBaseUrl, email, password, revealPassword };
  }

  function applyDraft(draft: ConnectionDraft) {
    baseUrl = draft.baseUrl;
    providerName = draft.providerName;
    customBaseUrl = draft.customBaseUrl;
    email = draft.email;
    password = draft.password;
    revealPassword = draft.revealPassword;
  }

  function resetEditor(next = connectionState) {
    const drafts: Record<Sub2ApiUpstream, ConnectionDraft> = {
      codex: emptyConnectionDraft(),
      claude: emptyConnectionDraft(),
    };
    drafts[next.upstream] = {
      baseUrl: next.baseUrl,
      providerName: next.codexProvider ?? '',
      customBaseUrl: next.customBaseUrl,
      email: next.email,
      password: '',
      revealPassword: false,
    };
    upstreamDrafts = drafts;
    upstream = next.upstream;
    applyDraft(drafts[next.upstream]);
    confirmingClear = false;
    error = null;
    providerError = null;
    resolvingProvider = false;
    cancelProviderResolution();
    providerResolution += 1;
  }

  function syncRememberedUpstream(next: Sub2ApiConfigState) {
    if (next.configured) rememberSub2ApiUpstream(providerId, next.upstream, next.baseUrl);
    else forgetSub2ApiUpstream(providerId);
  }

  function toggleEditor() {
    open = !open;
    if (open) {
      resetEditor();
    } else {
      cancelProviderResolution();
      resolvingProvider = false;
      providerResolution += 1;
    }
  }

  function selectUpstream(next: Sub2ApiUpstream) {
    if (upstream === next) return;
    upstreamDrafts[upstream] = currentDraft();
    upstream = next;
    applyDraft(upstreamDrafts[next]);
    providerError = null;
    resolvingProvider = false;
    cancelProviderResolution();
    providerResolution += 1;
    if (!customBaseUrl && providerName.trim()) {
      updateProvider(providerName);
    }
  }

  function setCustomBaseUrl(next: boolean) {
    customBaseUrl = next;
    baseUrl = '';
    providerName = '';
    providerError = null;
    resolvingProvider = false;
    cancelProviderResolution();
    providerResolution += 1;
  }

  function updateProvider(value: string) {
    providerName = value;
    baseUrl = '';
    providerError = null;
    cancelProviderResolution();
    const candidate = value.trim();
    const resolution = ++providerResolution;
    if (!candidate || customBaseUrl) {
      resolvingProvider = false;
      return;
    }
    resolvingProvider = true;
    const resolveProvider =
      upstream === 'claude' ? resolveSub2ApiClaudeProvider : resolveSub2ApiCodexProvider;
    providerResolutionTimer = window.setTimeout(() => {
      providerResolutionTimer = undefined;
      void resolveProvider(candidate)
        .then((resolved) => {
          if (resolution === providerResolution) baseUrl = resolved;
        })
        .catch((cause) => {
          if (resolution === providerResolution) {
            providerError = errorMessage(cause, 'The provider could not be resolved.');
          }
        })
        .finally(() => {
          if (resolution === providerResolution) resolvingProvider = false;
        });
    }, providerResolutionDelayMs);
  }

  async function save() {
    if (!canSave) return;
    const previousState = connectionState;
    const submitted = {
      baseUrl: baseUrl.trim(),
      codexProvider: providerName.trim(),
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
      providerName = submitted.codexProvider;
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
      })
      .catch((cause) => {
        error = errorMessage(cause, 'The Sub2API connection could not be read.');
        open = true;
      });
  });

  onDestroy(() => {
    cancelProviderResolution();
    providerResolution += 1;
  });
</script>

<section class="sub2api-config-section" aria-label="Connection">
  <h2>Connection</h2>
  <div class="sub2api-config-card" role="list" aria-label="Connection configurations">
    <div class="sub2api-config-item" role="listitem" aria-label="Connection">
      <div class="sub2api-config-summary">
        <ProviderIcon
          {providerId}
          upstreamProvider={connectionState.configured ? connectionState.upstream : null}
          size={18}
        />
        <span>
          <b
            >{connectionState.configured
              ? sub2ApiPublicName(connectionState.upstream)
              : 'Not configured'}</b
          >
          <small
            >{connectionState.configured
              ? connectionState.email
              : 'Base URL and administrator login'}</small
          >
        </span>
        <i class:missing={!connectionState.configured} aria-hidden="true"></i>
        <button bind:this={toggleButton} type="button" onclick={toggleEditor}
          >{open ? 'Done' : connectionState.configured ? 'Edit' : 'Add'}</button
        >
      </div>

      {#if open}
        <div class="sub2api-config-editor">
          <div class="config-row">
            <span>Upstream</span>
            <SelectMenu
              label="Upstream"
              value={upstream}
              options={upstreamOptions.map((option) => ({
                value: option,
                label: sub2ApiUpstreamLabel(option),
              }))}
              onChange={(value) => selectUpstream(value as Sub2ApiUpstream)}
            />
          </div>
          <label class="config-row">
            <span>Provider</span>
            <input
              type="text"
              value={providerName}
              autocomplete="off"
              spellcheck="false"
              placeholder={upstream === 'claude' ? 'Settings name' : 'Provider or profile'}
              aria-label={upstream === 'claude'
                ? 'Claude settings provider'
                : 'Codex provider or profile'}
              disabled={saving || customBaseUrl}
              oninput={(event) => updateProvider(event.currentTarget.value)}
            />
          </label>
          {#if providerError}<div class="config-error" role="alert">{providerError}</div>{/if}
          <div class="config-row">
            <span>Custom Base URL</span>
            <label class="switch custom-base-url-switch">
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
          <label class="config-row">
            <span>Base URL</span>
            <input
              type="url"
              bind:value={baseUrl}
              autocomplete="url"
              spellcheck="false"
              placeholder={customBaseUrl ? 'https://sub2api.example.com' : 'Resolved automatically'}
              aria-label="Base URL"
              disabled={saving || !customBaseUrl}
            />
          </label>
          <label class="config-row">
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
          <div class="config-row">
            <span>Password</span>
            <div class="password-field">
              <input
                type={revealPassword ? 'text' : 'password'}
                bind:value={password}
                autocomplete="current-password"
                placeholder={canReuseSavedPassword ? 'Unchanged' : 'Password'}
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
          </div>

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
                type="button"
                disabled={saving || confirmingClear}
                aria-label="Clear connection"
                onclick={() => void requestClear()}>Clear…</button
              >
            {/if}
          </div>

          {#if confirmingClear}
            <div
              class="clear-confirm"
              role="group"
              aria-labelledby="clear-sub2api-title"
              aria-describedby="clear-sub2api-message"
            >
              <strong id="clear-sub2api-title">Clear connection?</strong>
              <span id="clear-sub2api-message"
                >The Base URL and saved login will be removed. This account will remain.</span
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
  </div>
</section>

{@render children?.()}

<section class="sub2api-item-actions" aria-label="Sub2API Item">
  <button
    bind:this={removeItemButton}
    class="remove-item-row"
    type="button"
    disabled={saving || confirmingItemRemoval}
    onclick={() => void requestItemRemoval()}
  >
    Delete Account
  </button>

  {#if confirmingItemRemoval}
    <div
      class="remove-item-confirm"
      role="group"
      aria-labelledby="remove-sub2api-item-title"
      aria-describedby="remove-sub2api-item-message"
    >
      <strong id="remove-sub2api-item-title">Delete this account?</strong>
      <span id="remove-sub2api-item-message"
        >{connectionState.configured
          ? 'The account and its saved login will be removed.'
          : 'This empty account will be removed.'}</span
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

  :global(.sub2api-config-item + .sub2api-config-item) {
    border-top: 1px solid var(--separator);
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
    width: 6px;
    height: 6px;
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
    padding: 4px 8px;
    border: 0;
    border-radius: 6px;
    color: var(--text);
    background: var(--button-hover);
    font-size: 10px;
  }

  .sub2api-config-actions button,
  .clear-confirm button,
  .remove-item-confirm button {
    min-height: 26px;
    padding: 5px 10px;
  }

  .sub2api-config-editor {
    display: grid;
    border-top: 1px solid var(--separator);
  }

  .config-row {
    display: flex;
    min-height: 40px;
    align-items: center;
    justify-content: space-between;
    gap: 10px;
    padding: 6px 12px;
    font-size: 12px;
  }

  .config-row + .config-row {
    border-top: 1px solid var(--separator);
  }

  .config-row > span {
    flex: 0 0 auto;
  }

  .custom-base-url-switch {
    position: relative;
    display: block;
    width: 28px;
    height: 16px;
  }

  .custom-base-url-switch input {
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

  .custom-base-url-switch input:disabled {
    cursor: default;
  }

  .sub2api-config-editor input:not([type='checkbox']) {
    box-sizing: border-box;
    width: 100%;
    min-width: 0;
    max-width: 190px;
    height: 28px;
    border: 1px solid var(--separator);
    border-radius: 7px;
    outline: none;
    padding: 4px 8px;
    color: var(--text);
    background: var(--tray);
    font: inherit;
    font-size: 11px;
  }

  .sub2api-config-editor input:not([type='checkbox']):disabled {
    opacity: 1;
    border-color: transparent;
    color: var(--secondary);
    -webkit-text-fill-color: var(--secondary);
    background: transparent;
    text-align: right;
  }

  .sub2api-config-editor input:focus {
    border-color: var(--meter-fill);
    box-shadow: 0 0 0 2px color-mix(in srgb, var(--meter-fill) 20%, transparent);
  }

  .password-field {
    position: relative;
    width: 100%;
    max-width: 190px;
  }

  .password-field input {
    padding-right: 30px;
  }

  .password-field button {
    position: absolute;
    top: 1px;
    right: 1px;
    display: grid;
    width: 26px;
    height: 26px;
    place-items: center;
    border: 0;
    color: var(--secondary);
    background: transparent;
  }

  .sub2api-config-actions {
    display: flex;
    align-items: center;
    gap: 7px;
    padding: 8px 12px 12px;
    border-top: 1px solid var(--separator);
  }

  .sub2api-config-actions .primary {
    min-width: 56px;
    padding-inline: 12px;
    color: white;
    background: var(--meter-fill);
    font-weight: 600;
  }

  .sub2api-config-actions button:disabled,
  .clear-confirm button:disabled,
  .remove-item-confirm button:disabled,
  .remove-item-row:disabled {
    opacity: 0.5;
  }

  .clear-confirm {
    margin: 0 12px 12px;
  }

  .clear-confirm,
  .remove-item-confirm {
    display: grid;
    gap: 6px;
    border-radius: 8px;
    padding: 9px;
    background: color-mix(in srgb, var(--meter-critical) 8%, transparent);
    font-size: 10px;
  }

  .clear-confirm span,
  .remove-item-confirm span {
    color: var(--secondary);
    line-height: 14px;
  }

  .clear-confirm > div,
  .remove-item-confirm > div {
    display: flex;
    gap: 7px;
  }

  .clear-confirm .destructive,
  .remove-item-confirm .destructive {
    color: var(--tray);
    background: var(--error);
    font-weight: 600;
  }

  .sub2api-item-actions {
    margin-bottom: 14px;
  }

  .remove-item-row {
    display: block;
    width: 100%;
    min-height: 42px;
    padding: 8px 12px;
    border: 0;
    border-radius: 12px;
    color: var(--error);
    background: var(--card);
    font-size: 13px;
    font-weight: 600;
    text-align: center;
  }

  .remove-item-row:hover:not(:disabled) {
    background: var(--card-hover);
  }

  .remove-item-confirm {
    margin-top: 6px;
  }

  .config-error {
    color: var(--error);
    font-size: 10px;
    line-height: 14px;
  }

  .sub2api-config-editor > .config-error {
    padding: 0 12px 8px;
  }

  .summary-error {
    padding: 0 12px 10px 40px;
  }
</style>
