<script lang="ts">
  import Icon from './Icon.svelte';
  import { sub2ApiUpstreams } from './sub2ApiUpstreams';
  import { formatSpendValue, totalSpendRingCenter } from './metricFormat';
  import { popupMenu } from './nativeMenu';
  import SelectMenu from './SelectMenu.svelte';
  import type { ProviderCatalogIndex } from './metrics';
  import { TOTAL_SPEND_GEOMETRY } from './shareCard';
  import { ringSectorPath, spendRingArcs } from './spendRing';
  import {
    emptySpendMessage,
    projectSpend,
    SPEND_SCOPE_LABELS,
    type SpendProjection,
    type SpendSlice,
  } from './totalSpend';
  import {
    OTHERS_SPEND_ID,
    UNPRICED_OTHERS_SPEND_ID,
    providerSpendColorVariable,
  } from './providerIconPaths';
  import type { AppSettings, UsageHistories, UsageViewMode } from './types';

  interface Props {
    providers: Array<{ id: string; usageHistories: UsageHistories }>;
    settings: AppSettings;
    catalog: ProviderCatalogIndex;
    viewMode: UsageViewMode;
    onViewModeChange: (mode: UsageViewMode) => void;
    onChange: (settings: AppSettings) => void;
    onShare: (projections: SpendProjection[]) => boolean | Promise<boolean>;
  }
  let { providers, settings, catalog, viewMode, onViewModeChange, onChange, onShare }: Props =
    $props();
  const providerDisplayName = (id: string) => {
    if (id === OTHERS_SPEND_ID) return 'Others';
    if (id === UNPRICED_OTHERS_SPEND_ID) return 'Others (unpriced)';
    return catalog.resolvedDisplayName(id, settings.providerNames);
  };
  const renderedScopes = $derived(
    viewMode === 'all' ? (['localDevice', 'account'] as const) : [viewMode],
  );
  const projections = $derived(
    renderedScopes.map((scope) =>
      projectSpend(providers, settings.totalSpendPeriod, settings.totalSpendMetric, scope),
    ),
  );
  const inclusionNote = $derived(
    viewMode === 'all'
      ? `${scopeNote('localDevice')} ${scopeNote('account')}`
      : scopeNote(viewMode),
  );
  const periodIndex = $derived(
    settings.totalSpendPeriod === 'today' ? 0 : settings.totalSpendPeriod === 'yesterday' ? 1 : 2,
  );
  function display(value: number | null) {
    if (value === null) return '—';
    return formatSpendValue(value, settings.totalSpendMetric);
  }
  function providerSpendColor(providerId: string) {
    return `var(${providerSpendColorVariable(providerId, $sub2ApiUpstreams[providerId])}, var(--provider))`;
  }
  function ringCenter(value: number | null) {
    if (value === null) return { primary: '—', unit: '' };
    return totalSpendRingCenter(value, settings.totalSpendMetric);
  }
  function centerTooltip(projection: SpendProjection) {
    const value = projection.centerValue;
    if (value === null) return undefined;
    const exact = formatSpendValue(value, settings.totalSpendMetric, 'full');
    if (projection.costEstimated && settings.totalSpendMetric !== 'tokens') {
      return `${exact} · Estimated locally, so it may be off`;
    }
    return exact;
  }
  const billingDollars = new Intl.NumberFormat('en-US', {
    style: 'currency',
    currency: 'USD',
    maximumFractionDigits: 6,
  });
  function billingAmount(value: number) {
    return value > 0 && value < 0.000001 ? '<$0.000001' : billingDollars.format(value);
  }
  function billingLines(slice: SpendSlice) {
    const lines: string[] = [];
    for (const row of slice.otherBilling ?? []) {
      const name = 'model' in row ? row.model : providerDisplayName(row.providerId);
      const amounts = [];
      if (slice.id !== UNPRICED_OTHERS_SPEND_ID && row.estimatedCostUsd !== null) {
        const amount = billingAmount(row.estimatedCostUsd);
        const rate =
          settings.totalSpendMetric === 'costPerMillion' && row.pricedTokens > 0
            ? ` · ${billingAmount((row.estimatedCostUsd * 1_000_000) / row.pricedTokens)}/MTok`
            : '';
        amounts.push(
          `${row.costEstimated ? '≈ ' : ''}${amount} · ${formatSpendValue(row.pricedTokens, 'tokens', 'full')} tokens${rate}`,
        );
      }
      const unpriced = row.tokens - row.pricedTokens;
      if (
        unpriced > 0 &&
        (slice.id === UNPRICED_OTHERS_SPEND_ID || settings.totalSpendMetric === 'tokens')
      ) {
        amounts.push(`Unpriced · ${formatSpendValue(unpriced, 'tokens', 'full')} tokens`);
      }
      if (amounts.length) lines.push([name, ...amounts].join('\n'));
    }
    return lines;
  }
  let hoveredBilling = $state<string | null>(null);
  const chartId = $props.id();
  function patch(patch: Partial<AppSettings>) {
    onChange({ ...settings, ...patch });
  }
  function joinNames(names: string[]) {
    if (names.length < 2) return names[0] ?? '';
    return `${names.slice(0, -1).join(', ')} and ${names.at(-1)}`;
  }
  function scopeNote(scope: SpendProjection['scope']) {
    const names = providers
      .filter((provider) => provider.usageHistories[scope])
      .map((provider) => providerDisplayName(provider.id));
    if (names.length === 0) {
      return scope === 'localDevice'
        ? 'No local-device history available.'
        : 'No account-wide history available.';
    }
    return scope === 'localDevice'
      ? `Local-device history from ${joinNames(names)}.`
      : `Account-wide history from ${joinNames(names)}.`;
  }
  function hasScopeHistory(scope: SpendProjection['scope']) {
    return providers.some((provider) => provider.usageHistories[scope]);
  }
  // Total Spend's actions live in its context menu, drawn by the system where
  // the pointer is.
  function openMenu(event: MouseEvent) {
    event.preventDefault();
    event.stopPropagation();
    void popupMenu([{ text: 'Share Screenshot', action: () => void onShare(projections) }], {
      x: event.clientX,
      y: event.clientY,
    }).catch(() => undefined);
  }
</script>

<section
  class="total-spend-section"
  aria-label="Total Spend"
  data-total-spend
  oncontextmenu={openMenu}
  style={`--total-card-padding-x:${TOTAL_SPEND_GEOMETRY.cardPaddingX}px;--total-card-padding-y:${TOTAL_SPEND_GEOMETRY.cardPaddingY}px;--total-switcher-height:${TOTAL_SPEND_GEOMETRY.switcherHeight}px;--total-period-size:${TOTAL_SPEND_GEOMETRY.periodFontSize}px;--total-body-gap:${TOTAL_SPEND_GEOMETRY.bodyGap}px;--total-legend-gap:${TOTAL_SPEND_GEOMETRY.legendGap}px;--total-ring-size:${TOTAL_SPEND_GEOMETRY.ringDiameter}px;--total-center-size:${TOTAL_SPEND_GEOMETRY.centerFontSize}px;--total-center-unit-size:${TOTAL_SPEND_GEOMETRY.centerUnitFontSize}px;--total-legend-size:${TOTAL_SPEND_GEOMETRY.legendFontSize}px;`}
>
  <div class="total-card__header">
    <div class="total-card__title">
      <SelectMenu
        label="Total Spend Metric"
        value={settings.totalSpendMetric}
        variant="title"
        options={[
          { value: 'cost', label: 'Cost' },
          { value: 'costPerMillion', label: 'Cost/MTok' },
          { value: 'tokens', label: 'Tokens' },
        ]}
        onChange={(value) => patch({ totalSpendMetric: value as AppSettings['totalSpendMetric'] })}
      />
      <span
        class="icon-button icon-button--plain total-card__info"
        data-tooltip={inclusionNote}
        aria-label={inclusionNote}
        role="img"><Icon name="about" size={13} strokeWidth={1.9} /></span
      >
    </div>
    <div class="total-card__actions">
      <SelectMenu
        label="Usage Scope"
        value={viewMode}
        variant="title"
        options={[
          { value: 'all', label: 'All' },
          { value: 'localDevice', label: 'Device' },
          { value: 'account', label: 'Accounts' },
        ]}
        onChange={(value) => onViewModeChange(value as UsageViewMode)}
      />
    </div>
  </div>
  <div class="total-card">
    <div class="period-switcher" aria-label="Total Spend period">
      <span
        class="period-switcher__selection"
        style={`transform: translateX(${periodIndex * 100}%)`}
        aria-hidden="true"
      ></span>
      {#each [['today', 'Today'], ['yesterday', 'Yesterday'], ['last30Days', '30 Days']] as option (option[0])}
        <button
          class:active={settings.totalSpendPeriod === option[0]}
          type="button"
          onclick={() => patch({ totalSpendPeriod: option[0] as AppSettings['totalSpendPeriod'] })}
          >{option[1]}</button
        >
      {/each}
    </div>
    <div class:total-card__scopes--all={viewMode === 'all'}>
      {#each projections as projection (projection.scope)}
        <section
          class="total-card__scope"
          class:total-card__scope--empty-history={projection.slices.length === 0 &&
            !hasScopeHistory(projection.scope)}
          aria-label={`${SPEND_SCOPE_LABELS[projection.scope]} Spend`}
        >
          {#if viewMode === 'all'}
            <h2>{SPEND_SCOPE_LABELS[projection.scope]}</h2>
          {/if}
          {#if projection.slices.length === 0}
            <div class="total-card__empty">
              <span>{emptySpendMessage(settings.totalSpendMetric)}</span>
            </div>
          {:else}
            <div class="total-card__body">
              <div class="spend-ring">
                <svg
                  viewBox={`0 0 ${TOTAL_SPEND_GEOMETRY.ringDiameter} ${TOTAL_SPEND_GEOMETRY.ringDiameter}`}
                  shape-rendering="geometricPrecision"
                  aria-hidden="true"
                >
                  {#each spendRingArcs(projection.slices) as segment (segment.id)}
                    <path
                      role="presentation"
                      class="spend-ring__segment"
                      onpointerenter={() => (hoveredBilling = segment.id)}
                      onpointerleave={() => (hoveredBilling = null)}
                      d={ringSectorPath(segment, TOTAL_SPEND_GEOMETRY)}
                      style={`--segment-color: ${providerSpendColor(segment.id)}`}
                    />
                  {/each}
                </svg>
                <div class="spend-ring__label" data-tooltip={centerTooltip(projection)}>
                  <strong>{ringCenter(projection.centerValue).primary}</strong><span
                    >{ringCenter(projection.centerValue).unit}</span
                  >
                </div>
              </div>
              <div class="spend-legend">
                {#each projection.slices as provider (provider.id)}
                  {@const tooltipId = provider.otherBilling
                    ? `${chartId}-${projection.scope}-${provider.id}`
                    : undefined}
                  <div
                    class="spend-legend__row"
                    class:spend-legend__billing={provider.otherBilling}
                  >
                    <svelte:element
                      this={tooltipId ? 'button' : 'span'}
                      type={tooltipId ? 'button' : undefined}
                      aria-describedby={tooltipId}
                      ><i style={`background: ${providerSpendColor(provider.id)}`}
                      ></i>{providerDisplayName(provider.id)}</svelte:element
                    ><strong
                      class:spend-legend__value--hidden={provider.showValue === false}
                      aria-hidden={provider.showValue === false ? 'true' : undefined}
                      >{display(provider.value)}</strong
                    >
                    {#if provider.otherBilling}
                      <div
                        id={tooltipId}
                        role="tooltip"
                        class="spend-billing-tooltip"
                        class:spend-billing-tooltip--open={hoveredBilling === provider.id}
                      >
                        {#each billingLines(provider) as line, index (index)}
                          <div>{line}</div>
                        {/each}
                      </div>
                    {/if}
                  </div>
                {/each}
              </div>
            </div>
          {/if}
        </section>
      {/each}
    </div>
  </div>
</section>

<style>
  :global {
    .total-card {
      margin-bottom: 12px;
      padding: 10px 11px;
      border-radius: 12px;
      background: var(--card);
    }

    .total-card__header,
    .total-card__body,
    .trend-heading {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
    }

    .period-switcher {
      position: relative;
      display: flex;
      padding: 2px;
      border-radius: 999px;
      background: var(--meter-track);
    }

    .period-switcher button {
      position: relative;
      z-index: 1;
      padding: 3px 6px;
      border: 0;
      border-radius: 5px;
      color: var(--secondary);
      background: transparent;
      font-size: 9px;
      cursor: pointer;
    }

    .period-switcher button.active {
      color: var(--text);
      background: transparent;
      box-shadow: none;
    }

    .period-switcher__selection {
      position: absolute;
      top: 3px;
      bottom: 3px;
      left: 3px;
      width: calc((100% - 6px) / 3);
      border-radius: 999px;
      background: var(--control);
      box-shadow: var(--control-shadow);
      transition: transform var(--motion-spring);
    }

    .total-card__body {
      justify-content: flex-start;
      padding-top: 10px;
    }

    .total-card__scopes--all {
      display: grid;
      gap: 8px;
    }

    .total-card__scopes--all .total-card__scope + .total-card__scope {
      padding-top: 8px;
      border-top: 1px solid var(--separator);
    }

    .total-card__scope h2 {
      margin: 8px 0 0;
      color: var(--secondary);
      font-size: 10px;
      font-weight: 600;
    }

    .spend-ring {
      position: relative;
      display: grid;
      width: 74px;
      height: 74px;
      flex: 0 0 74px;
      place-items: center;
    }

    .spend-ring svg {
      position: absolute;
      width: 100%;
      height: 100%;
      overflow: visible;
    }

    .spend-ring__segment {
      fill: var(--segment-color);
      transition: fill 160ms ease;
    }

    .spend-ring__label {
      position: relative;
      z-index: 1;
      display: flex;
      align-items: center;
      justify-content: center;
      flex-direction: column;
    }

    .spend-ring strong {
      max-width: 48px;
      overflow: hidden;
      font-size: 13px;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .spend-ring span,
    .spend-legend small {
      color: var(--secondary);
      font-size: 8px;
    }

    .spend-legend {
      display: grid;
      flex: 1;
      min-width: 0;
      grid-template-columns: minmax(0, 1fr);
      gap: 2px 8px;
      font-size: 11px;
    }

    .spend-legend__row {
      display: grid;
      grid-template-columns: minmax(0, 1fr) auto;
      align-items: center;
      gap: 8px;
    }

    .spend-legend__billing {
      position: relative;
    }

    .spend-legend__billing button {
      padding: 0;
      border: 0;
      color: inherit;
      background: none;
      font: inherit;
      text-align: left;
      cursor: help;
    }

    .spend-billing-tooltip {
      display: none;
      position: absolute;
      top: 100%;
      right: 0;
      z-index: 50;
      width: max-content;
      max-width: min(260px, calc(100vw - 32px));
      max-height: 35vh;
      overflow: auto;
      padding: 6px 8px;
      border: 1px solid var(--separator);
      border-radius: 6px;
      color: var(--text);
      background: var(--tray);
      box-shadow: 0 5px 18px rgba(0, 0, 0, 0.24);
      font-size: 10px;
      line-height: 1.4;
      white-space: pre-line;
      overflow-wrap: anywhere;
    }

    .spend-billing-tooltip--open,
    .spend-legend__billing:hover .spend-billing-tooltip,
    .spend-legend__billing:focus-within .spend-billing-tooltip {
      display: grid;
      gap: 5px;
    }

    .spend-legend__row i {
      display: inline-block;
      width: 7px;
      height: 7px;
      margin-right: 5px;
      border-radius: 50%;
      background: var(--provider);
    }

    .spend-legend small {
      grid-column: 1 / -1;
    }

    .icon-button {
      display: grid;
      width: 30px;
      height: 30px;
      margin-left: 0;
      padding: 0;
      border: 0;
      border-radius: 50%;
      color: var(--secondary);
      background: transparent;
      cursor: default;
      place-items: center;
    }

    .icon-button:hover,
    .icon-button:focus-visible {
      color: var(--text);
      background: var(--button-hover);
    }

    .icon-button:focus-visible {
      outline: 2px solid var(--meter-fill);
      outline-offset: 2px;
    }

    @media (prefers-reduced-motion: no-preference) {
      .icon-button {
        transition:
          color 120ms ease,
          background-color 120ms ease;
      }
    }

    .total-spend-section {
      margin-bottom: 0;
    }

    .total-card__header {
      min-height: 24px;
      margin-bottom: 4px;
      padding: 0 4px 2px;
    }

    .total-card__title {
      display: flex;
      min-width: 0;
      align-items: center;
      gap: 4px;
    }

    .total-card__actions {
      display: flex;
      min-width: 0;
      align-items: center;
      gap: 4px;
    }

    .total-card__scope-label {
      max-width: 84px;
      overflow: hidden;
      color: var(--secondary);
      font-size: 9px;
      line-height: 20px;
      text-overflow: ellipsis;
      white-space: nowrap;
    }

    .select-menu.select-menu--title {
      font-size: 14px;
    }

    .select-menu--title .select-menu__trigger {
      min-height: 24px;
      gap: 4px;
      font-weight: 600;
      cursor: pointer;
    }

    .select-menu--title .select-menu__trigger:focus-visible {
      outline: 2px solid color-mix(in srgb, var(--meter-fill) 55%, transparent);
      outline-offset: 2px;
    }

    .select-menu--title .select-menu__label {
      color: inherit;
    }

    .select-menu--title .select-menu__stepper {
      width: auto;
      height: auto;
      flex: 0 0 auto;
      color: inherit;
      background: transparent;
    }

    .select-menu.select-menu--title:hover .select-menu__trigger {
      color: var(--meter-fill);
    }

    .select-menu.select-menu--title:hover .select-menu__stepper {
      background: transparent;
    }

    /* The scope pull-down reads as a quieter companion to the metric title. */
    .total-card__actions .select-menu.select-menu--title {
      font-size: 11px;
    }

    .total-card__actions .select-menu--title .select-menu__trigger {
      color: var(--secondary);
      font-weight: 500;
    }

    .total-card__actions .select-menu.select-menu--title:hover .select-menu__trigger {
      color: var(--meter-fill);
    }

    .total-card__header .icon-button--plain {
      width: 20px;
      height: 20px;
      flex: 0 0 20px;
      color: var(--secondary);
      cursor: pointer;
    }

    .total-card__header .total-card__info {
      width: 16px;
      height: 20px;
      flex-basis: 16px;
      cursor: help;
    }

    .total-card {
      margin: 0;
      padding: var(--total-card-padding-y) var(--total-card-padding-x);
    }

    .period-switcher {
      width: 100%;
      height: var(--total-switcher-height);
      padding: 3px;
      background: var(--meter-track);
    }

    .period-switcher button {
      min-height: 0;
      flex: 1;
      padding: 0 12px;
      font-size: var(--total-period-size);
      font-weight: 500;
    }

    .period-switcher button.active {
      font-weight: 600;
    }

    .total-card__body {
      gap: var(--total-legend-gap);
      padding-top: var(--total-body-gap);
    }

    .spend-ring {
      width: var(--total-ring-size);
      height: var(--total-ring-size);
      flex-basis: var(--total-ring-size);
      padding: 0;
    }

    .spend-ring strong {
      max-width: 68px;
      font-size: var(--total-center-size);
    }

    .spend-ring span {
      font-size: var(--total-center-unit-size);
    }

    .spend-legend {
      align-content: center;
      gap: 7px 8px;
      font-size: var(--total-legend-size);
    }

    .spend-legend__row i {
      width: 8px;
      height: 8px;
      margin-right: 7px;
    }

    .spend-legend strong {
      color: var(--secondary);
      font-weight: 500;
      text-align: right;
    }

    .spend-legend__value--hidden {
      visibility: hidden;
    }

    .total-card__empty {
      display: flex;
      min-height: 36px;
      align-items: center;
      justify-content: center;
      flex-direction: column;
      gap: 8px;
      color: var(--secondary);
      font-size: 11px;
      text-align: center;
    }

    :root[data-density='compact'] .total-card {
      padding: 10px 12px;
    }

    :root[data-density='compact'] .period-switcher button {
      min-height: 21px;
      padding: 3px 10px;
    }

    :root[data-density='compact'] .total-card__body {
      gap: 14px;
      padding-top: 8px;
    }

    :root[data-density='compact'] .spend-ring {
      width: 88px;
      height: 88px;
      flex-basis: 88px;
    }

    :root[data-density='compact'] .spend-ring strong {
      max-width: 58px;
    }

    :root[data-density='compact'] .total-card__empty {
      min-height: 32px;
    }
  }
</style>
