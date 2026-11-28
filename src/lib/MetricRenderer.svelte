<script lang="ts">
  import { slide } from 'svelte/transition';
  import { usageSourceNote, type ProviderCatalogIndex } from './metrics';
  import { springMotion } from './motion';
  import QuotaMetric from './QuotaMetric.svelte';
  import QuotaReset from './QuotaReset.svelte';
  import { quotaNow } from './pacing';
  import StatusMetric from './StatusMetric.svelte';
  import UsageMetric from './UsageMetric.svelte';
  import UsageTrend from './UsageTrend.svelte';
  import ValueMetric from './ValueMetric.svelte';
  import { PROVIDER_USAGE_SCOPE_LABELS, usageHistoriesForMode } from './usageScopes';
  import type {
    AppSettings,
    MetricLayout,
    ProviderSnapshot,
    UsageHistory,
    UsageScope,
    UsageViewMode,
  } from './types';

  interface Props {
    layout: MetricLayout;
    snapshot: ProviderSnapshot;
    settings: AppSettings;
    now: number;
    catalog: ProviderCatalogIndex;
    viewMode: UsageViewMode;
    usageScope?: UsageScope;
    tokensOnly?: boolean;
    reducedMotion?: boolean;
    onSettingsChange: (settings: AppSettings) => void;
  }
  let {
    layout,
    snapshot,
    settings,
    now,
    catalog,
    viewMode,
    usageScope,
    tokensOnly = false,
    reducedMotion = false,
    onSettingsChange,
  }: Props = $props();
  const definition = $derived(catalog.metric(layout.id));
  const providerDisplayName = $derived(
    catalog.resolvedDisplayName(snapshot.providerId, settings.providerNames),
  );
  const quota = $derived.by(() => {
    const source = definition?.source;
    if (source?.kind !== 'quota' && source?.kind !== 'quotaOrValue') return undefined;
    return snapshot.quotas.find((item) => item.id === source.sourceId);
  });
  const scopedUsageHistories = $derived(
    usageHistoriesForMode(snapshot.usageHistories, usageScope ?? viewMode),
  );
  function usagePeriod(history: UsageHistory) {
    if (definition?.source.kind !== 'usage') return null;
    if (definition.source.period === 'today') return history.today;
    if (definition.source.period === 'yesterday') return history.yesterday;
    return history.last30Days;
  }
  const valueMetric = $derived.by(() => {
    const source = definition?.source;
    if (source?.kind !== 'value' && source?.kind !== 'quotaOrValue') return null;
    return snapshot.valueMetrics.find((item) => item.id === source.sourceId) ?? null;
  });
  const statusMetric = $derived.by(() => {
    const source = definition?.source;
    if (source?.kind !== 'status') return null;
    return snapshot.statusMetrics.find((item) => item.id === source.sourceId) ?? null;
  });
  function toggleReset() {
    onSettingsChange({
      ...settings,
      resetDisplay: settings.resetDisplay === 'countdown' ? 'exact' : 'countdown',
    });
  }
</script>

{#if (definition?.source.kind === 'quota' || definition?.source.kind === 'quotaOrValue') && quota && quota.format !== 'unlimitedDollars'}
  <QuotaMetric
    {quota}
    {now}
    usageDisplay={settings.usageDisplay}
    resetDisplay={settings.resetDisplay}
    timeFormat={settings.timeFormat}
    alwaysShowPacing={settings.alwaysShowPacing}
    sessionWindow={definition.source.sessionWindow}
    onToggleUsage={() =>
      onSettingsChange({
        ...settings,
        usageDisplay: settings.usageDisplay === 'used' ? 'left' : 'used',
      })}
    onToggleReset={toggleReset}
  />
{:else if definition?.source.kind === 'quotaOrValue' && !quota && valueMetric}
  <ValueMetric
    label={definition.label}
    metric={valueMetric}
    {now}
    resetDisplay={settings.resetDisplay}
    timeFormat={settings.timeFormat}
  />
{:else if definition?.source.kind === 'quota' || definition?.source.kind === 'quotaOrValue'}
  <section class="metric metric--no-data" aria-label={`${definition.label} quota`}>
    <div class="metric__heading"><h2>{definition.label}</h2></div>
    <div class="meter-shell">
      <div
        class="meter"
        role="progressbar"
        aria-label={`${definition.label} used`}
        aria-valuemin="0"
        aria-valuemax="100"
        aria-valuetext="No data"
      ></div>
    </div>
    <div class="metric__reading">
      <span>No data</span>
      {#if quota?.resetsAt}
        <QuotaReset
          resetsAt={quota.resetsAt}
          now={quotaNow(quota, now)}
          resetDisplay={settings.resetDisplay}
          timeFormat={settings.timeFormat}
          onToggleReset={toggleReset}
        />
      {:else}
        <span>Reset unavailable</span>
      {/if}
    </div>
  </section>
{:else if definition?.source.kind === 'trend'}
  {#each scopedUsageHistories as scoped (scoped.scope)}
    <div class="trend-scope-slot" transition:slide={springMotion(reducedMotion)}>
      <UsageTrend
        label={scopedUsageHistories.length > 1
          ? `${PROVIDER_USAGE_SCOPE_LABELS[scoped.scope]} Trend`
          : definition.label}
        daily={scoped.history.daily}
        sourceNote={usageSourceNote(
          catalog,
          snapshot,
          scoped.history,
          scoped.scope,
          providerDisplayName,
        )}
      />
    </div>
  {/each}
{:else if definition?.source.kind === 'status'}
  <StatusMetric label={definition.label} metric={statusMetric} />
{:else if definition?.source.kind === 'usage'}
  {#each scopedUsageHistories as scoped (scoped.scope)}
    <UsageMetric {tokensOnly} label={definition.label} period={usagePeriod(scoped.history)} />
  {/each}
{:else if definition?.source.kind === 'value'}
  <ValueMetric
    label={definition.label}
    metric={valueMetric}
    {now}
    resetDisplay={settings.resetDisplay}
    timeFormat={settings.timeFormat}
  />
{/if}
