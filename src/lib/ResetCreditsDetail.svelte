<script lang="ts">
  import { tick } from 'svelte';
  import { claimClaudeResetCredit, claimCodexResetCredit, refreshProviderUsage } from './backend';
  import { getResetClaim, startResetClaim, finishResetClaim } from './resetClaims';
  import {
    clampFixedPopoverTop,
    computeFixedPopoverTop,
    type VerticalBounds,
  } from './floatingPosition';
  import { formatReset } from './pacing';
  import type { ResetClaimOutcome, ResetCreditDetail, ResetCreditKind } from './types';

  interface Props {
    title: string;
    provider?: 'codex' | 'claude';
    count: number;
    expiries: string[];
    resetCredits?: ResetCreditDetail[];
    now: number;
    timeFormat: 'system' | 'twelveHour' | 'twentyFourHour';
    top: number;
    anchor?: VerticalBounds;
    onEnter: () => void;
    onLeave: () => void;
    onDismiss: (options?: { restoreFocus?: boolean }) => void;
  }

  interface ResetEntry {
    id: string;
    claimKey: string;
    claimAccount: string | null;
    expiry: string | null;
    kind: ResetCreditKind | null;
    kindLabel: string | null;
    redeemable: boolean;
    number: number;
    severity: 'normal' | 'warning' | 'critical';
    exact: string;
    relative: string | null;
  }

  let {
    title,
    provider = 'codex',
    count,
    expiries,
    resetCredits = [],
    now,
    timeFormat,
    top,
    anchor,
    onEnter,
    onLeave,
    onDismiss,
  }: Props = $props();
  let confirmingId = $state<string | null>(null);
  let pendingId = $state<string | null>(null);
  let result = $state<{ context: string; outcome: ResetClaimOutcome } | null>(null);
  let dialogElement = $state<HTMLDivElement>();
  let resolvedTop = $state<number | null>(null);
  let cancelButton = $state<HTMLButtonElement>();
  let claimTriggerIndex = $state<number | null>(null);
  const uncertainMessage = $derived(
    `The reset may have been applied. Check the latest usage in ${provider === 'claude' ? 'Claude' : 'Codex'} before using another reset.`,
  );

  function claimKey(entry: ResetEntry) {
    return `${provider}:${entry.claimKey}`;
  }

  function claimBlocked(entry: ResetEntry) {
    const status = getResetClaim(claimKey(entry))?.status;
    return status === 'pending' || status === 'uncertain';
  }

  function formatKindLabel(kind: ResetCreditKind | null) {
    if (kind === 'fable') return 'Fable';
    if (kind === 'partial') return 'Partial';
    if (kind === 'full') return 'Full';
    return null;
  }

  function confirmMessage(kind: ResetCreditKind | null) {
    if (kind === 'fable') {
      return "Restore Fable weekly usage and refill your session. This can't be undone.";
    }
    if (kind === 'partial') {
      return "Immediately reset the covered usage limits. This can't be undone.";
    }
    return "Immediately reset your usage limits. This can't be undone.";
  }

  function buildEntry(
    expiry: string | null,
    kind: ResetCreditKind | null,
    redeemable: boolean,
    index: number,
    claimAccount: string | null = null,
  ): ResetEntry {
    redeemable = redeemable && (provider !== 'claude' || claimAccount !== null);
    if (!expiry) {
      return {
        id: `${claimAccount ?? ''}:none:${kind ?? 'default'}:${index}`,
        claimKey: `${claimAccount ?? ''}:none:${kind ?? 'default'}`,
        claimAccount,
        expiry: null,
        kind,
        kindLabel: formatKindLabel(kind),
        redeemable,
        number: index + 1,
        severity: 'normal',
        exact: 'No expiry',
        relative: null,
      };
    }
    const timestamp = new Date(expiry).getTime();
    const remaining = timestamp - now;
    const severity =
      remaining <= 48 * 60 * 60 * 1000
        ? 'critical'
        : remaining <= 7 * 24 * 60 * 60 * 1000
          ? 'warning'
          : 'normal';
    const relative = formatReset(expiry, now, 'countdown', timeFormat).replace(
      /^Resets(?: in)?\s*/,
      '',
    );
    const exact = formatReset(expiry, now, 'exact', timeFormat).replace(/^Resets\s*/, '');
    const imminent = relative === 'soon';
    return {
      id: `${claimAccount ?? ''}:${expiry}:${kind ?? 'default'}:${index}`,
      claimKey: `${claimAccount ?? ''}:${expiry}:${kind ?? 'default'}`,
      claimAccount,
      expiry,
      kind,
      kindLabel: formatKindLabel(kind),
      redeemable,
      number: index + 1,
      severity,
      exact: imminent ? 'Expiring soon' : exact,
      relative: imminent ? null : relative,
    };
  }

  const entries = $derived.by(() => {
    if (resetCredits.length > 0) {
      const sorted = [...resetCredits].sort((left, right) => {
        if (!left.expiresAt && !right.expiresAt) return 0;
        if (!left.expiresAt) return 1;
        if (!right.expiresAt) return -1;
        return left.expiresAt.localeCompare(right.expiresAt);
      });
      return sorted.map((credit, index) =>
        buildEntry(
          credit.expiresAt ?? null,
          credit.kind,
          credit.redeemable,
          index,
          credit.claimAccount ?? null,
        ),
      );
    }
    return [...expiries].sort().map((expiry, index) => buildEntry(expiry, null, true, index));
  });

  const entriesContext = $derived(JSON.stringify([provider, entries.map((entry) => entry.id)]));
  const hasUncertainClaim = $derived(
    entries.some((entry) => getResetClaim(claimKey(entry))?.status === 'uncertain'),
  );
  const resultMessage = $derived(
    hasUncertainClaim
      ? uncertainMessage
      : result?.context !== entriesContext
        ? null
        : result?.outcome === 'success'
          ? 'Reset applied.'
          : result?.outcome === 'nothingToReset'
            ? 'No active limit needs resetting.'
            : result?.outcome === 'noCredit'
              ? 'This reset is no longer available.'
              : result?.outcome === 'cooldown'
                ? 'Reset is on cooldown. Try again later.'
                : result?.outcome === 'uncertain'
                  ? uncertainMessage
                  : result?.outcome === 'failed'
                    ? 'Could not use this reset. Try again.'
                    : null,
  );

  $effect(() => {
    if (result && result.context !== entriesContext) result = null;
    if (confirmingId && !entries.some((entry) => entry.id === confirmingId)) confirmingId = null;
  });

  function handleKeydown(event: KeyboardEvent) {
    if (event.key !== 'Escape') return;
    event.preventDefault();
    event.stopPropagation();
    if (confirmingId) {
      void cancelClaim();
      return;
    }
    if (!pendingId) onDismiss({ restoreFocus: true });
  }

  async function beginClaim(entry: ResetEntry, triggerIndex: number) {
    if (!entry.redeemable || claimBlocked(entry)) return;
    confirmingId = entry.id;
    claimTriggerIndex = triggerIndex;
    result = null;
    await tick();
    cancelButton?.focus();
  }

  async function cancelClaim() {
    if (pendingId) return;
    const triggerIndex = claimTriggerIndex;
    confirmingId = null;
    await tick();
    if (triggerIndex !== null) {
      dialogElement
        ?.querySelector<HTMLButtonElement>(`[data-reset-trigger="${triggerIndex}"]`)
        ?.focus();
    }
    claimTriggerIndex = null;
  }

  async function confirmClaim(entry: ResetEntry) {
    if (pendingId || !entry.redeemable) return;
    const key = claimKey(entry);
    const requestId = startResetClaim(key);
    if (!requestId) return;
    const context = entriesContext;
    const claimProvider = provider;
    pendingId = entry.id;
    let outcome: ResetClaimOutcome;
    try {
      outcome =
        claimProvider === 'claude'
          ? await claimClaudeResetCredit(entry.expiry, entry.kind, entry.claimAccount!, requestId)
          : entry.expiry
            ? await claimCodexResetCredit(entry.expiry, requestId)
            : 'failed';
    } catch {
      outcome = claimProvider === 'claude' ? 'uncertain' : 'failed';
      if (claimProvider === 'claude') {
        try {
          await refreshProviderUsage('claude');
        } catch {
          /* Keep the unresolved request. */
        }
      }
    }
    finishResetClaim(key, outcome);
    result = { context, outcome };
    pendingId = null;
    confirmingId = null;
    claimTriggerIndex = null;
    await tick();
    dialogElement?.focus();
  }

  $effect(() => {
    void entries.length;
    void confirmingId;
    void resultMessage;
    const height = dialogElement?.offsetHeight ?? 0;
    if (height <= 0) {
      resolvedTop = top;
      return;
    }
    resolvedTop = anchor
      ? computeFixedPopoverTop(anchor, height, window.innerHeight, { preferred: 'below', gap: 7 })
      : clampFixedPopoverTop(top, height, window.innerHeight);
  });

  $effect(() => {
    const onScroll = (event: Event) => {
      if (confirmingId || pendingId) return;
      if (event.target instanceof Node && dialogElement?.contains(event.target)) return;
      onDismiss({ restoreFocus: false });
    };
    const onResize = () => {
      if (!confirmingId && !pendingId) onDismiss({ restoreFocus: false });
    };
    window.addEventListener('scroll', onScroll, { capture: true, passive: true });
    window.addEventListener('resize', onResize);
    return () => {
      window.removeEventListener('scroll', onScroll, { capture: true });
      window.removeEventListener('resize', onResize);
    };
  });
</script>

<svelte:window onkeydowncapture={handleKeydown} />

<div
  bind:this={dialogElement}
  class="reset-credits-detail"
  style={`top:${resolvedTop ?? top}px`}
  role="dialog"
  tabindex="-1"
  aria-label={`${title} details`}
  onmouseenter={onEnter}
  onfocusin={onEnter}
  onmouseleave={() => {
    if (!confirmingId && !pendingId && !dialogElement?.contains(document.activeElement)) {
      onLeave();
    }
  }}
>
  <div class="reset-detail-body">
    {#if resultMessage}
      <div
        class:reset-result-error={hasUncertainClaim || result?.outcome === 'failed'}
        class="reset-result"
        role="status"
      >
        {resultMessage}
      </div>
    {/if}
    {#if entries.length > 0}
      <div class="reset-timeline">
        {#each entries as entry, index (entry.id)}
          <div class="reset-entry-shell">
            <div class="reset-rail" aria-hidden="true">
              <i class:reset-rail-hidden={index === 0}></i>
              <b class="reset-node reset-node--{entry.severity}">{entry.number}</b>
              <i class:reset-rail-hidden={index === entries.length - 1}></i>
            </div>
            <div class="reset-entry-content">
              {#if confirmingId === entry.id}
                <div
                  class="reset-confirm"
                  role="group"
                  aria-labelledby={`reset-confirm-title-${index}`}
                  aria-describedby={`reset-confirm-message-${index}`}
                >
                  <strong id={`reset-confirm-title-${index}`}>Use this reset?</strong>
                  <span id={`reset-confirm-message-${index}`}>{confirmMessage(entry.kind)}</span>
                  <div>
                    <button
                      class="reset-confirm-primary"
                      type="button"
                      disabled={pendingId !== null}
                      onclick={() => confirmClaim(entry)}
                      >{pendingId === entry.id ? 'Resetting…' : 'Use reset'}</button
                    >
                    <button
                      bind:this={cancelButton}
                      type="button"
                      disabled={pendingId !== null}
                      onclick={() => void cancelClaim()}>Cancel</button
                    >
                  </div>
                </div>
              {:else}
                <div class="reset-entry">
                  <span class="reset-entry-label">
                    <span>{entry.exact}</span>
                    {#if entry.kindLabel}
                      <span
                        class:reset-kind--fable={entry.kind === 'fable'}
                        class:reset-kind--partial={entry.kind === 'partial'}
                        class="reset-kind">{entry.kindLabel}</span
                      >
                    {/if}
                  </span>
                  <div class="reset-trailing">
                    {#if entry.relative}<small>{entry.relative}</small>{/if}
                    <button
                      class="reset-use"
                      type="button"
                      data-reset-trigger={index}
                      aria-label={`Use reset expiring ${entry.exact}`}
                      title={claimBlocked(entry)
                        ? 'Previous reset is unresolved'
                        : entry.redeemable
                          ? undefined
                          : 'Not usable right now'}
                      disabled={!entry.redeemable || pendingId !== null || claimBlocked(entry)}
                      onclick={() => void beginClaim(entry, index)}>Use</button
                    >
                  </div>
                </div>
              {/if}
            </div>
          </div>
        {/each}
      </div>
    {:else if count > 0}
      <div class="reset-empty">
        <strong>{count} available</strong>
        <span>Expiry times unavailable</span>
      </div>
    {:else}
      <div class="reset-empty"><span>No rate limit resets available</span></div>
    {/if}
  </div>
</div>

<style>
  :global {
    .reset-credits-detail {
      position: fixed;
      right: 8px;
      z-index: 100;
      box-sizing: border-box;
      width: 250px;
      max-height: calc(100vh - 16px);
      overflow: hidden;
      border: 1px solid var(--separator);
      border-radius: 11px;
      color: var(--text);
      background: color-mix(in srgb, var(--tray) 98%, transparent);
      box-shadow:
        0 10px 28px rgba(0, 0, 0, 0.22),
        0 2px 7px rgba(0, 0, 0, 0.12);
      backdrop-filter: blur(16px);
      animation: reset-detail-in 120ms ease-out both;
    }

    .reset-detail-body {
      max-height: calc(100vh - 16px);
      padding: 14px;
      overflow-y: auto;
    }

    .reset-timeline {
      display: grid;
    }

    .reset-result {
      margin-bottom: 8px;
      padding: 7px 8px;
      border-radius: 7px;
      color: var(--text);
      background: color-mix(in srgb, var(--meter-fill) 16%, transparent);
      font-size: 10px;
    }

    .reset-result-error {
      color: var(--error);
      background: var(--error-bg);
    }

    .reset-entry-shell {
      display: grid;
      grid-template-columns: 18px minmax(0, 1fr);
      gap: 10px;
    }

    .reset-entry-content {
      min-width: 0;
      padding: 3px 0;
    }

    .reset-entry {
      display: flex;
      height: 30px;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      font-size: 11px;
      line-height: 14px;
    }

    .reset-entry > span {
      overflow: hidden;
      font-weight: 550;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .reset-entry-label {
      display: inline-flex;
      min-width: 0;
      align-items: center;
      gap: 5px;
    }

    .reset-entry-label > span:first-child {
      overflow: hidden;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .reset-kind {
      flex-shrink: 0;
      padding: 1px 5px;
      border-radius: 4px;
      color: var(--secondary);
      background: color-mix(in srgb, var(--text) 7%, transparent);
      font-size: 9px;
      font-weight: 600;
      line-height: 12px;
    }

    .reset-kind--fable,
    .reset-kind--partial {
      color: var(--meter-warning);
      background: color-mix(in srgb, var(--meter-warning) 14%, transparent);
    }

    .reset-trailing {
      position: relative;
      display: flex;
      flex-shrink: 0;
      align-items: center;
      justify-content: flex-end;
    }

    .reset-trailing > small {
      color: var(--secondary);
      font: inherit;
      font-variant-numeric: tabular-nums;
      white-space: nowrap;
      transition: opacity 100ms ease;
    }

    .reset-use,
    .reset-confirm button {
      min-height: 22px;
      padding: 2px 8px;
      border: 1px solid var(--separator);
      border-radius: 6px;
      color: var(--text);
      background: var(--control);
      font: inherit;
      font-size: 10px;
      cursor: default;
    }

    .reset-use {
      position: absolute;
      right: 0;
      opacity: 0;
      pointer-events: none;
      transition: opacity 100ms ease;
    }

    .reset-entry:hover .reset-trailing > small,
    .reset-entry:focus-within .reset-trailing > small {
      opacity: 0;
    }

    .reset-entry:hover .reset-use,
    .reset-entry:focus-within .reset-use {
      opacity: 1;
      pointer-events: auto;
    }

    .reset-entry:hover .reset-use:disabled,
    .reset-entry:focus-within .reset-use:disabled,
    .reset-confirm button:disabled {
      opacity: 0.55;
    }

    .reset-confirm {
      display: grid;
      gap: 8px;
      margin: 1px 0 5px;
      padding: 10px;
      border-radius: 8px;
      background: color-mix(in srgb, var(--text) 4%, transparent);
      font-size: 10px;
      animation: reset-confirm-in 150ms ease-out both;
    }

    .reset-confirm > span {
      color: var(--secondary);
      line-height: 14px;
    }

    .reset-confirm > div {
      display: flex;
      gap: 8px;
    }

    .reset-confirm > div > button {
      flex: 1;
    }

    .reset-confirm .reset-confirm-primary {
      color: white;
      background: var(--meter-fill);
      font-weight: 600;
    }

    .reset-rail {
      display: grid;
      height: 100%;
      grid-template-rows: 8px 18px 1fr;
      justify-items: center;
      align-items: center;
    }

    .reset-rail > i {
      width: 1px;
      height: 100%;
      background: var(--separator);
    }

    .reset-rail > .reset-rail-hidden {
      opacity: 0;
    }

    .reset-node {
      display: grid;
      width: 18px;
      height: 18px;
      place-items: center;
      border-radius: 50%;
      color: white;
      background: var(--meter-fill);
      font-size: 11px;
      font-weight: 600;
      line-height: 1;
    }

    .reset-node--warning {
      color: #171719;
      background: var(--meter-warning);
    }

    .reset-node--critical {
      background: var(--meter-critical);
    }

    .reset-empty {
      display: grid;
      justify-items: center;
      gap: 4px;
      padding: 12px 4px 8px;
      color: var(--secondary);
      font-size: 11px;
      line-height: 15px;
      text-align: center;
    }

    .reset-empty strong {
      color: var(--text);
      font-weight: 650;
    }

    @keyframes reset-detail-in {
      from {
        opacity: 0;
        transform: translateY(-2px) scale(0.98);
      }
      to {
        opacity: 1;
        transform: translateY(0) scale(1);
      }
    }

    @keyframes reset-confirm-in {
      from {
        opacity: 0;
        transform: scale(0.97);
        transform-origin: top;
      }
      to {
        opacity: 1;
        transform: scale(1);
      }
    }

    :root[data-density='compact'] .reset-entry {
      height: 26px;
      font-size: 10px;
    }
  }
</style>
