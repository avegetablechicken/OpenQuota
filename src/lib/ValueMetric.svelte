<script lang="ts">
  import { onDestroy, tick } from 'svelte';
  import { computeFixedPopoverTop, type VerticalBounds } from './floatingPosition';
  import { formatMetricValue } from './metricFormat';
  import { formatReset } from './pacing';
  import Icon from './Icon.svelte';
  import ResetCreditsDetail from './ResetCreditsDetail.svelte';
  import type { ResetCreditKind, ValueMetric } from './types';

  interface Props {
    label: string;
    metric: ValueMetric | null;
    now: number;
    resetDisplay: 'countdown' | 'exact';
    timeFormat: 'system' | 'twelveHour' | 'twentyFourHour';
  }

  let { label, metric, now, resetDisplay, timeFormat }: Props = $props();
  let detailOpen = $state(false);
  let detailTop = $state(8);
  let detailAnchor = $state<VerticalBounds | null>(null);
  let showTimer: ReturnType<typeof setTimeout> | undefined;
  let hideTimer: ReturnType<typeof setTimeout> | undefined;
  let detailTrigger = $state<HTMLButtonElement>();
  let restoringTriggerFocus = false;
  const claimProvider = $derived<'codex' | 'claude' | null>(
    metric?.id === 'rateLimitResets'
      ? 'codex'
      : metric?.id === 'claudeRateLimitResets'
        ? 'claude'
        : null,
  );
  const showsResetDetail = $derived(claimProvider !== null);
  const hasEstimatedValue = $derived(metric?.values.some((value) => value.estimated) ?? false);
  const hasNonFullReset = $derived(
    metric?.resetCredits?.some((credit) => credit.kind !== 'full') ?? false,
  );

  function resetKindLabel(kind: ResetCreditKind) {
    if (kind === 'fable') return 'Fable';
    if (kind === 'partial') return 'Partial';
    if (kind === 'full') return 'Full';
    return 'Unknown';
  }

  const reading = $derived(
    metric?.values
      .map((value) => formatMetricValue(value.number, value.kind, 'row', value.label ?? undefined))
      .join(' · ') ?? 'No data',
  );
  const tooltip = $derived.by(() => {
    if (!metric) return undefined;
    if (!showsResetDetail && metric.resetCredits?.length) {
      const sorted = [...metric.resetCredits].sort((left, right) => {
        if (!left.expiresAt && !right.expiresAt) return 0;
        if (!left.expiresAt) return 1;
        if (!right.expiresAt) return -1;
        return left.expiresAt.localeCompare(right.expiresAt);
      });
      const lines = sorted.map((credit, index) => {
        const when = credit.expiresAt
          ? formatReset(credit.expiresAt, now, resetDisplay, timeFormat).replace(
              /^Resets(?: in)?\s*/,
              '',
            )
          : 'No expiry';
        return `${index + 1}. ${resetKindLabel(credit.kind)} · ${when}`;
      });
      return [resetDisplay === 'countdown' ? 'Resets expire in:' : 'Resets expire:', ...lines].join(
        '\n',
      );
    }
    if (metric.expiriesAt.length && !showsResetDetail) {
      const sorted = [...metric.expiriesAt].sort();
      const lines = sorted.map((expiry, index) => {
        const formatted = formatReset(expiry, now, resetDisplay, timeFormat).replace(
          /^Resets(?: in)?\s*/,
          '',
        );
        return `${index + 1}. ${formatted}`;
      });
      return [resetDisplay === 'countdown' ? 'Resets expire in:' : 'Resets expire:', ...lines].join(
        '\n',
      );
    }
    const count = metric.values[0]?.number ?? 0;
    if (
      (metric.id === 'rateLimitResets' ||
        metric.id === 'claudeRateLimitResets' ||
        metric.id === 'sub2apiRateLimitResets') &&
      count > 0
    ) {
      return 'Expiry times unavailable';
    }
    if (metric.values.some((value) => Math.abs(value.number) >= 1000)) {
      return metric.values
        .map((value) =>
          formatMetricValue(value.number, value.kind, 'full', value.label ?? undefined),
        )
        .join(' · ');
    }
    return undefined;
  });
  const expirySeverity = $derived.by(() => {
    if (!metric?.expiriesAt.length) return null;
    const remaining =
      Math.min(...metric.expiriesAt.map((value) => new Date(value).getTime())) - now;
    if (remaining <= 48 * 60 * 60 * 1000) return 'critical';
    if (remaining <= 7 * 24 * 60 * 60 * 1000) return 'warning';
    return 'normal';
  });

  function scheduleShow(event: Event) {
    if (restoringTriggerFocus) {
      restoringTriggerFocus = false;
      return;
    }
    if (!showsResetDetail || detailOpen || showTimer) return;
    if (hideTimer) clearTimeout(hideTimer);
    const target = event.currentTarget as HTMLElement;
    showTimer = setTimeout(() => {
      showTimer = undefined;
      if (!target.isConnected || !showsResetDetail) return;
      // Measure when opening: the page may have scrolled during the delay.
      placeDetail(target);
      detailOpen = true;
    }, 350);
  }
  function placeDetail(target: HTMLElement) {
    const rect = target.getBoundingClientRect();
    const rowCount = metric?.resetCredits?.length || metric?.expiriesAt.length || 0;
    const estimatedHeight = Math.min(72 + rowCount * 34, 340);
    detailAnchor = { top: rect.top, bottom: rect.bottom };
    detailTop = computeFixedPopoverTop(detailAnchor, estimatedHeight, window.innerHeight, {
      preferred: 'below',
      gap: 7,
    });
  }
  function scheduleHide() {
    if (showTimer) clearTimeout(showTimer);
    showTimer = undefined;
    if (hideTimer) clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      detailOpen = false;
      hideTimer = undefined;
    }, 180);
  }
  function keepOpen() {
    if (hideTimer) clearTimeout(hideTimer);
    hideTimer = undefined;
  }
  function toggleDetail(event: Event) {
    if (!showsResetDetail) return;
    if (detailOpen) {
      detailOpen = false;
      return;
    }
    if (showTimer) clearTimeout(showTimer);
    showTimer = undefined;
    if (hideTimer) clearTimeout(hideTimer);
    hideTimer = undefined;
    placeDetail(event.currentTarget as HTMLElement);
    detailOpen = true;
  }
  async function dismissDetail({ restoreFocus = false } = {}) {
    if (showTimer) clearTimeout(showTimer);
    showTimer = undefined;
    if (hideTimer) clearTimeout(hideTimer);
    hideTimer = undefined;
    detailOpen = false;
    if (!restoreFocus) return;
    await tick();
    restoringTriggerFocus = true;
    detailTrigger?.focus();
    restoringTriggerFocus = false;
  }
  onDestroy(() => {
    if (showTimer) clearTimeout(showTimer);
    if (hideTimer) clearTimeout(hideTimer);
  });
</script>

<div class="value-row">
  <span>{label}</span>
  {#if showsResetDetail}
    <button
      bind:this={detailTrigger}
      type="button"
      class="value-reading value-reading--interactive"
      aria-expanded={detailOpen}
      aria-label={`${label}: ${reading}`}
      onmouseenter={scheduleShow}
      onmouseleave={scheduleHide}
      onfocus={scheduleShow}
      onblur={scheduleHide}
      onclick={toggleDetail}
    >
      {#if expirySeverity}<i class="expiry-dot expiry-dot--{expirySeverity}" aria-hidden="true"
        ></i>{/if}
      {reading}
      {#if hasNonFullReset}
        <span class="value-estimate" aria-hidden="true"
          ><Icon name="about" size={11} strokeWidth={1.9} /></span
        >
      {/if}
      {#if hasEstimatedValue}
        <span
          class="value-estimate"
          data-tooltip="Estimated locally, so it may differ from billed usage."
          aria-label="Estimated value"
          role="img"><Icon name="about" size={11} strokeWidth={1.9} /></span
        >
      {/if}
    </button>
  {:else}
    <span class="value-reading" data-tooltip={tooltip}>
      {reading}
      {#if hasNonFullReset}
        <span class="value-estimate" aria-hidden="true"
          ><Icon name="about" size={11} strokeWidth={1.9} /></span
        >
      {/if}
      {#if hasEstimatedValue}
        <span
          class="value-estimate"
          data-tooltip="Estimated locally, so it may differ from billed usage."
          aria-label="Estimated value"
          role="img"><Icon name="about" size={11} strokeWidth={1.9} /></span
        >
      {/if}
    </span>
  {/if}
</div>

{#if detailOpen && metric && claimProvider}
  <ResetCreditsDetail
    title={label}
    provider={claimProvider}
    count={Math.max(0, Math.floor(metric.values[0]?.number ?? 0))}
    expiries={metric.expiriesAt}
    resetCredits={metric.resetCredits ?? []}
    {now}
    {timeFormat}
    top={detailTop}
    anchor={detailAnchor ?? undefined}
    onEnter={keepOpen}
    onLeave={scheduleHide}
    onDismiss={(options) => void dismissDetail(options)}
  />
{/if}

<style>
  :global {
    .value-row {
      display: flex;
      align-items: baseline;
      justify-content: space-between;
      gap: 8px;
      padding: 6px 14px;
      font-size: 12px;
      line-height: 15px;
    }

    .value-row > span:first-child {
      color: color-mix(in srgb, var(--text) 88%, var(--card));
      font-weight: 600;
    }

    .value-reading {
      display: inline-flex;
      align-items: center;
      gap: 5px;
      text-align: right;
    }

    .value-estimate {
      display: inline-flex;
      color: var(--secondary);
    }

    .value-reading--interactive {
      padding: 0;
      border: 0;
      border-radius: 6px;
      color: inherit;
      background: none;
      font: inherit;
      cursor: default;
      outline: none;
      transition:
        background-color 120ms ease,
        box-shadow 120ms ease;
    }

    .value-reading--interactive:hover,
    .value-reading--interactive:focus-visible,
    .value-reading--interactive[aria-expanded='true'] {
      background: var(--button-hover);
      box-shadow: 0 0 0 4px var(--button-hover);
    }

    .expiry-dot {
      width: 6px;
      height: 6px;
      border-radius: 50%;
      background: var(--meter-fill);
    }

    .expiry-dot--warning {
      background: var(--meter-warning);
    }

    .expiry-dot--critical {
      background: var(--meter-critical);
    }

    :root[data-density='compact'] .value-row {
      padding: 3px 14px;
      font-size: 11px;
    }
  }
</style>
