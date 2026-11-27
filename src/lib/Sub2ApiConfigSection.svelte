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
    editorFor,
    restoreAfterFailure,
    review,
    savedMode as savedModeOf,
    submission,
    withCustomBaseUrl,
    withMode,
    withProviderName,
    withUpstream,
    type EditorChange,
  } from './sub2ApiDraft';
  import {
    forgetSub2ApiEndpoint,
    rememberSub2ApiEndpoint,
    sub2ApiPublicName,
    sub2ApiResolvesProviderNames,
    sub2ApiUpstreamLabel,
    sub2ApiUpstreamOptions,
  } from './sub2ApiUpstreams';
  import type {
    Sub2ApiMode,
    Sub2ApiConfigOutcome,
    Sub2ApiConfigState,
    Sub2ApiUpstream,
  } from './types';

  interface Props {
    providerId: string;
    /** Called once the item is removed, with a warning when provider status could not follow. */
    onRemove?: (warning?: string) => void;
    children?: Snippet;
  }

  let { providerId, onRemove = () => {}, children }: Props = $props();

  const notConfigured: Sub2ApiConfigState = {
    configured: false,
    mode: 'subscription',
    baseUrl: '',
    codexProvider: '',
    customBaseUrl: false,
    email: '',
    upstream: 'codex',
  };
  let connectionState = $state<Sub2ApiConfigState>(notConfigured);
  let open = $state(false);
  let editor = $state(editorFor(notConfigured));
  let saving = $state(false);
  let confirmingClear = $state(false);
  let confirmingItemRemoval = $state(false);
  let error = $state<string | null>(null);
  /** Set when a change was stored but provider status could not follow it. */
  let notice = $state<string | null>(null);

  /** Keeps a change's warning apart from the connection it describes. */
  function accept({ warning, ...state }: Sub2ApiConfigOutcome) {
    notice = warning ?? null;
    return state;
  }
  let providerError = $state<string | null>(null);
  let resolvingProvider = $state(false);
  let providerResolution = 0;
  let providerResolutionTimer: number | undefined;
  let toggleButton = $state<HTMLButtonElement>();
  let clearButton = $state<HTMLButtonElement>();
  let clearCancelButton = $state<HTMLButtonElement>();
  let removeItemButton = $state<HTMLButtonElement>();
  let removeItemCancelButton = $state<HTMLButtonElement>();
  // Codex and Claude also name the configuration an address and key are read from.
  const upstreamOptions = sub2ApiUpstreamOptions;
  // Only Codex and Claude configurations name a Base URL; others are always entered.
  const addressFromProvider = $derived(sub2ApiResolvesProviderNames(editor.draft.providerSource));
  const providerResolutionDelayMs = 300;

  const savedMode = $derived(savedModeOf(connectionState));
  const summarySource = $derived.by(() => {
    if (open) return editor.mode === 'subscription' ? editor.mode : editor.upstream;
    if (!connectionState.configured) return null;
    return savedMode === 'subscription' ? savedMode : connectionState.upstream;
  });
  const summaryDetail = $derived.by(() => {
    if (!open && !connectionState.configured) return 'Provider or custom Base URL';
    if (summarySource === 'subscription') {
      const address = open
        ? editor.draft.providerName.trim() || editor.draft.baseUrl
        : connectionState.codexProvider || connectionState.baseUrl;
      return address || 'Provider or custom Base URL';
    }
    return (
      (open ? editor.draft.email.trim() : connectionState.email) ||
      'Base URL and administrator login'
    );
  });
  const checks = $derived(
    review(editor, connectionState, {
      lookup: { resolving: resolvingProvider, failed: Boolean(providerError) },
      saving,
    }),
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

  /** Stops any provider lookup in flight and forgets its result. */
  function stopProviderLookup() {
    providerError = null;
    resolvingProvider = false;
    cancelProviderResolution();
    providerResolution += 1;
  }

  function resetEditor(next = connectionState) {
    editor = editorFor(next);
    confirmingClear = false;
    error = null;
    stopProviderLookup();
  }

  function syncRememberedEndpoint(next: Sub2ApiConfigState) {
    if (next.configured) rememberSub2ApiEndpoint(providerId, next.baseUrl);
    else forgetSub2ApiEndpoint(providerId);
  }

  function toggleEditor() {
    open = !open;
    resetEditor();
  }

  /** Shows a change to the draft and starts or stops the provider lookup it asks for. */
  function apply({ editor: next, lookup }: EditorChange) {
    editor = next;
    if (lookup.kind === 'keep') return;
    stopProviderLookup();
    if (lookup.kind === 'stop') return;
    const resolution = providerResolution;
    resolvingProvider = true;
    const resolveProvider =
      next.draft.providerSource === 'claude'
        ? resolveSub2ApiClaudeProvider
        : resolveSub2ApiCodexProvider;
    providerResolutionTimer = window.setTimeout(() => {
      providerResolutionTimer = undefined;
      void resolveProvider(lookup.name)
        .then((resolved) => {
          if (resolution === providerResolution) editor.draft.baseUrl = resolved;
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

  function selectMode(next: Sub2ApiMode) {
    error = null;
    apply(withMode(editor, next));
  }

  async function save() {
    if (!checks.canSave) return;
    const previousState = connectionState;
    const { input: submitted, optimistic: optimisticState } = submission(editor, previousState);
    saving = true;
    error = null;
    notice = null;
    connectionState = optimisticState;
    syncRememberedEndpoint(optimisticState);
    resetEditor(optimisticState);
    open = false;
    await tick();
    toggleButton?.focus();
    try {
      connectionState = accept(await saveSub2ApiConfig(providerId, submitted));
      syncRememberedEndpoint(connectionState);
      resetEditor(connectionState);
    } catch (cause) {
      connectionState = previousState;
      syncRememberedEndpoint(previousState);
      resetEditor(previousState);
      editor = restoreAfterFailure(previousState, submitted);
      open = true;
      error = errorMessage(cause, 'The Sub2API connection could not be saved.');
    } finally {
      editor.draft.password = '';
      saving = false;
    }
  }

  async function clearConnection() {
    if (saving) return;
    saving = true;
    error = null;
    notice = null;
    try {
      connectionState = accept(await clearSub2ApiConfig(providerId));
      syncRememberedEndpoint(connectionState);
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
    notice = null;
    try {
      connectionState = accept(await deleteSub2ApiConfig(providerId));
      forgetSub2ApiEndpoint(providerId);
      resetEditor(connectionState);
      confirmingItemRemoval = false;
      open = false;
      // The removed item's section usually closes, so the warning goes to whoever shows the next screen.
      onRemove(notice ?? undefined);
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
    const initialState = connectionState;
    void getSub2ApiConfigState(providerId)
      .then((next) => {
        if (connectionState !== initialState) return;
        connectionState = next;
        syncRememberedEndpoint(next);
        // Add can open before the initial read completes. Keep the in-progress
        // drafts when the backend confirms there was no saved connection.
        if (next.configured || !open) resetEditor(next);
      })
      .catch((cause) => {
        if (connectionState !== initialState) return;
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
        <ProviderIcon {providerId} upstreamProvider={summarySource} size={18} />
        <span>
          <b>{summarySource ? sub2ApiPublicName(summarySource) : 'Not configured'}</b>
          <small>{summaryDetail}</small>
        </span>
        <i class:missing={!connectionState.configured} aria-hidden="true"></i>
        <button bind:this={toggleButton} type="button" aria-expanded={open} onclick={toggleEditor}
          >{open ? 'Done' : connectionState.configured ? 'Edit' : 'Add'}</button
        >
      </div>

      {#if !open && error}
        <div class="config-error summary-error" role="alert">{error}</div>
      {:else if !open && notice}
        <div class="config-note summary-error" role="status">{notice}</div>
      {/if}
      {#if open}
        <div class="sub2api-config-editor">
          <div class="config-group">
            <div class="config-row">
              <span>Track</span>
              <SelectMenu
                label="Track"
                value={editor.mode}
                options={[
                  { value: 'subscription', label: 'User subscription' },
                  { value: 'admin', label: 'Upstream accounts (admin)' },
                ]}
                disabled={saving}
                onChange={(value) => selectMode(value as Sub2ApiMode)}
              />
            </div>
            <div class="config-row">
              <span>Upstream</span>
              <SelectMenu
                label="Upstream"
                value={editor.upstream}
                options={upstreamOptions.map((option) => ({
                  value: option,
                  label: sub2ApiUpstreamLabel(option),
                }))}
                disabled={saving}
                onChange={(value) => apply(withUpstream(editor, value as Sub2ApiUpstream))}
              />
            </div>
            {#if addressFromProvider}
              <div class="config-row address-switch-row">
                <span>Custom Base URL</span>
                <label class="switch connection-switch">
                  <input
                    type="checkbox"
                    role="switch"
                    aria-label="Use custom Base URL"
                    checked={editor.draft.customBaseUrl}
                    disabled={saving}
                    onchange={(event) =>
                      apply(withCustomBaseUrl(editor, event.currentTarget.checked))}
                  />
                  <span></span>
                </label>
              </div>
            {/if}
            {#if editor.draft.customBaseUrl || !addressFromProvider}
              <label class="config-row">
                <span>Base URL</span>
                <input
                  type="url"
                  bind:value={editor.draft.baseUrl}
                  autocomplete="url"
                  spellcheck="false"
                  placeholder="https://sub2api.example.com"
                  aria-label="Base URL"
                  disabled={saving}
                />
              </label>
            {:else}
              <label class="config-row">
                <span>Provider</span>
                <input
                  type="text"
                  value={editor.draft.providerName}
                  autocomplete="off"
                  spellcheck="false"
                  placeholder={editor.draft.providerSource === 'claude'
                    ? 'Settings name'
                    : 'Provider or profile'}
                  aria-label={editor.draft.providerSource === 'claude'
                    ? 'Claude settings provider'
                    : 'Codex provider or profile'}
                  disabled={saving}
                  oninput={(event) => apply(withProviderName(editor, event.currentTarget.value))}
                />
              </label>
              {#if providerError}
                <div class="config-note config-error" role="alert">
                  {providerError}
                </div>
              {:else}
                <p
                  class="config-note config-resolved-url"
                  role="status"
                  aria-label="Resolved Base URL"
                >
                  {resolvingProvider
                    ? 'Looking up the Base URL…'
                    : editor.draft.baseUrl || 'The Base URL is read from this provider.'}
                </p>
              {/if}
            {/if}
          </div>
          {#if editor.mode === 'subscription'}
            <div class="config-row">
              <span>API key</span>
              <div class="password-field">
                <input
                  type="password"
                  bind:value={editor.draft.apiKey}
                  autocomplete="off"
                  spellcheck="false"
                  placeholder={checks.keepSavedKey
                    ? 'Unchanged'
                    : editor.draft.customBaseUrl
                      ? 'Subscription API key'
                      : 'From provider (optional override)'}
                  aria-label="Sub2API subscription API key"
                  disabled={saving}
                />
                {#if !editor.draft.customBaseUrl && checks.keepSavedKey && !editor.draft.apiKey}
                  <button
                    type="button"
                    aria-label="Clear saved API key"
                    title="Clear saved API key"
                    disabled={saving}
                    onclick={() => (editor.draft.clearSavedKey = true)}
                    ><Icon name="clear-filled" size={14} /></button
                  >
                {/if}
              </div>
            </div>
          {:else}
            <div class="config-group">
              <label class="config-row">
                <span>Email</span>
                <input
                  type="email"
                  bind:value={editor.draft.email}
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
                    type={editor.draft.revealPassword ? 'text' : 'password'}
                    bind:value={editor.draft.password}
                    autocomplete="current-password"
                    placeholder={checks.canReuseCredential ? 'Unchanged' : 'Password'}
                    aria-label="Sub2API administrator password"
                    disabled={saving}
                  />
                  <button
                    type="button"
                    aria-label={editor.draft.revealPassword ? 'Hide password' : 'Show password'}
                    onclick={() => (editor.draft.revealPassword = !editor.draft.revealPassword)}
                  >
                    <Icon name={editor.draft.revealPassword ? 'eye-off' : 'eye'} size={15} />
                  </button>
                </div>
              </div>
            </div>
          {/if}
          <div class="sub2api-config-actions">
            {#if connectionState.configured}
              <button
                bind:this={clearButton}
                type="button"
                disabled={saving || confirmingClear}
                aria-label="Clear connection"
                onclick={() => void requestClear()}>Clear…</button
              >
            {/if}
            <button
              class="primary"
              type="button"
              disabled={!checks.canSave}
              use:saveShortcut={checks.canSave}
              onclick={() => void save()}>{saving ? 'Saving…' : 'Save'}</button
            >
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
                >The saved connections and credentials for both tracking modes will be removed. This
                account will remain.</span
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
          {#if error}<div class="config-error" role="alert">{error}</div>
          {:else if notice}<div class="config-note" role="status">{notice}</div>{/if}
        </div>
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
    padding: 3px 14px 8px 12px;
    border-top: 1px solid var(--separator);
  }

  .config-group {
    padding-block: 3px;
    border-bottom: 1px solid var(--separator);
  }

  .config-row {
    display: grid;
    grid-template-columns: minmax(min-content, 6.25rem) minmax(0, 1fr);
    min-height: 29px;
    justify-items: end;
    align-items: center;
    column-gap: 6px;
    font-size: 12px;
  }

  .address-switch-row {
    grid-template-columns: 1fr auto;
  }

  .config-row > span {
    justify-self: start;
  }

  .config-note {
    margin: 0;
    padding-block: 1px 2px;
    overflow-wrap: anywhere;
    color: var(--secondary);
    font-size: 10px;
    line-height: 13px;
    text-align: right;
  }

  .connection-switch {
    position: relative;
    display: block;
    width: 28px;
    height: 16px;
  }

  .connection-switch input {
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

  .connection-switch input:disabled {
    cursor: default;
  }

  .sub2api-config-editor input:not([type='checkbox']) {
    box-sizing: border-box;
    width: 100%;
    min-width: 0;
    height: 25px;
    border: 1px solid var(--separator);
    border-radius: 7px;
    outline: none;
    padding: 4px 8px;
    color: var(--text);
    background: var(--control);
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
  }

  .password-field input:not([type='checkbox']) {
    padding-right: 30px;
  }

  .password-field button {
    position: absolute;
    top: 1px;
    right: 1px;
    display: grid;
    bottom: 1px;
    width: 26px;
    place-items: center;
    border: 0;
    color: var(--secondary);
    background: transparent;
  }

  .sub2api-config-actions {
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 6px;
    padding-top: 3px;
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

  .clear-confirm,
  .sub2api-config-editor > .config-error {
    margin-top: 5px;
  }

  .summary-error {
    padding: 0 12px 10px 40px;
  }
</style>
