<script lang="ts">
  import { onDestroy } from 'svelte';
  import Icon from './Icon.svelte';
  import { formatSpendValue, totalSpendRingCenter } from './metricFormat';
  import SelectMenu from './SelectMenu.svelte';
  import type { ProviderCatalogIndex } from './metrics';
  import { TOTAL_SPEND_GEOMETRY } from './shareCard';
  import { ringSectorPath, spendRingArcs } from './spendRing';
  import {
    emptySpendMessage,
    projectSpend,
    SPEND_SCOPE_LABELS,
    type SpendProjection,
  } from './totalSpend';
  import {
    OTHERS_SPEND_ID,
    UNPRICED_OTHERS_SPEND_ID,
    providerSpendColorVariable,
  } from './providerIconPaths';
  import { sub2ApiDisplayName, sub2ApiUpstreams } from './sub2ApiUpstreams';
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
    return (
      sub2ApiDisplayName(id, $sub2ApiUpstreams[id], settings.providerNames[id]) ??
      catalog.displayName(id, settings.providerNames)
    );
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
  let shareCopied = $state(false);
  let shareTimer: ReturnType<typeof setTimeout> | undefined;

  onDestroy(() => {
    if (shareTimer) clearTimeout(shareTimer);
  });

  function display(value: number | null) {
    if (value === null) return '—';
    return formatSpendValue(value, settings.totalSpendMetric);
  }
  function providerSpendColor(providerId: string) {
    return `var(${providerSpendColorVariable(providerId)}, var(--provider))`;
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
  function metricTitle() {
    if (settings.totalSpendMetric === 'tokens') return 'Tokens';
    if (settings.totalSpendMetric === 'costPerMillion') return 'Cost/MTok';
    return 'Cost';
  }
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
  async function share() {
    if (!(await onShare(projections))) return;
    shareCopied = true;
    if (shareTimer) clearTimeout(shareTimer);
    shareTimer = setTimeout(() => (shareCopied = false), 1400);
  }
</script>

<section
  class="total-spend-section"
  aria-label="Total Spend"
  data-total-spend
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
      <div class="spend-scope-switcher" role="group" aria-label="Usage Scope">
        {#each [['all', 'All'], ['localDevice', 'Device'], ['account', 'Accounts']] as option (option[0])}
          <button
            class:active={viewMode === option[0]}
            type="button"
            aria-pressed={viewMode === option[0]}
            onclick={() => onViewModeChange(option[0] as UsageViewMode)}>{option[1]}</button
          >
        {/each}
      </div>
      <button
        class="icon-button icon-button--plain total-card__share"
        type="button"
        aria-label={`Share ${metricTitle()} Screenshot`}
        data-tooltip="Share Screenshot"
        onclick={share}
        ><Icon name={shareCopied ? 'check' : 'share'} size={14} strokeWidth={1.8} /></button
      >
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
          class:total-card__scope--empty-history={projection.centerValue === null &&
            !hasScopeHistory(projection.scope)}
          aria-label={`${SPEND_SCOPE_LABELS[projection.scope]} Spend`}
        >
          {#if viewMode === 'all'}
            <h2>{SPEND_SCOPE_LABELS[projection.scope]}</h2>
          {/if}
          {#if projection.centerValue === null}
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
                      class="spend-ring__segment"
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
                  <span
                    ><i style={`background: ${providerSpendColor(provider.id)}`}
                    ></i>{providerDisplayName(provider.id)}</span
                  ><strong
                    class:spend-legend__value--hidden={provider.showValue === false}
                    aria-hidden={provider.showValue === false ? 'true' : undefined}
                    >{display(provider.value)}</strong
                  >
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
      background: var(--tray);
      box-shadow: 0 1px 2px rgba(0, 0, 0, 0.12);
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
      grid-template-columns: 1fr auto;
      gap: 2px 8px;
      font-size: 11px;
    }

    .spend-legend span i {
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

    .spend-scope-switcher {
      display: grid;
      width: 144px;
      height: 22px;
      flex: 0 0 144px;
      grid-template-columns: repeat(3, minmax(0, 1fr));
      gap: 1px;
      padding: 2px;
      border-radius: 6px;
      background: var(--meter-track);
    }

    .spend-scope-switcher button {
      min-width: 0;
      padding: 0 4px;
      overflow: hidden;
      border: 0;
      border-radius: 4px;
      color: var(--secondary);
      background: transparent;
      font-size: 9px;
      font-weight: 500;
      line-height: 18px;
      text-overflow: ellipsis;
      white-space: nowrap;
      cursor: pointer;
    }

    .spend-scope-switcher button.active {
      color: var(--text);
      background: var(--tray);
      box-shadow: 0 1px 2px rgba(0, 0, 0, 0.12);
      font-weight: 600;
    }

    .spend-scope-switcher button:focus-visible {
      outline: 2px solid color-mix(in srgb, var(--meter-fill) 55%, transparent);
      outline-offset: 1px;
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

    .select-menu--title {
      min-width: 0;
    }

    .select-menu--title .select-menu__trigger {
      min-height: 24px;
      gap: 4px;
      padding: 0;
      border: 0;
      background: transparent;
      font-size: 14px;
      font-weight: 600;
    }

    .select-menu--title .select-menu__trigger:hover,
    .select-menu--title .select-menu__trigger[aria-expanded='true'] {
      color: var(--meter-fill);
      background: transparent;
    }

    .select-menu__list--title {
      min-width: 138px;
      transform-origin: top left;
    }

    .select-menu__list--title.select-menu__list--above {
      transform-origin: bottom left;
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

    .spend-legend span i {
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
      min-height: 76px;
      align-items: center;
      justify-content: center;
      flex-direction: column;
      gap: 8px;
      color: var(--secondary);
      font-size: 11px;
      text-align: center;
    }

    .total-card__scope--empty-history .total-card__empty {
      min-height: 36px;
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
      min-height: 64px;
    }
  }
</style>
