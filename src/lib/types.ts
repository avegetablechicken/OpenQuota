export interface QuotaWindow {
  clockOffsetSeconds?: number | null;
  id: string;
  label: string;
  usedPercent: number;
  resetsAt: string | null;
  periodSeconds: number;
  format: 'percent' | 'dollars' | 'unlimitedDollars' | 'count';
  usedValue: number | null;
  limitValue: number | null;
  unit?: string | null;
  estimated: boolean;
  sourceNote?: string | null;
}

export interface MetricValue {
  number: number;
  kind: 'count' | 'dollars';
  label?: string | null;
  estimated: boolean;
}

export type ResetCreditKind = 'full' | 'fable' | 'partial' | 'unknown';

export interface ResetCreditDetail {
  claimAccount?: string | null;
  expiresAt?: string | null;
  kind: ResetCreditKind;
  redeemable: boolean;
}

export interface ValueMetric {
  id: string;
  label: string;
  values: MetricValue[];
  expiriesAt: string[];
  resetCredits?: ResetCreditDetail[];
}

export interface StatusMetric {
  id: string;
  label: string;
  text: string;
  tone: 'neutral' | 'positive' | 'warning' | 'danger';
  subtitle?: string | null;
}

export type ResetClaimOutcome =
  'success' | 'nothingToReset' | 'noCredit' | 'cooldown' | 'uncertain' | 'failed';

export interface ProviderNotice {
  id: string;
  title: string;
  message: string;
  tone: 'info' | 'warning';
}

export interface UsagePeriod {
  tokens: number;
  estimatedCostUsd: number | null;
  costEstimated: boolean;
  estimateComplete: boolean;
  modelBreakdown?: ModelUsageBreakdown | null;
  unknownModels?: string[];
}

export interface ModelUsageEntry {
  model: string;
  totalTokens: number;
  costUsd: number | null;
  variants?: ModelUsageVariant[] | null;
}

export interface ModelUsageVariant {
  model: string;
  totalTokens: number;
  costUsd: number | null;
}

export interface ModelUsageBreakdown {
  models: ModelUsageEntry[];
  sourceNote: string;
}

export interface DailyUsage {
  date: string;
  tokens: number;
  estimatedCostUsd: number | null;
  estimateComplete: boolean;
}

export interface OtherModelUsage {
  tokens: number;
  pricedTokens: number;
  estimatedCostUsd: number | null;
  costEstimated: boolean;
}

export interface OtherUsagePeriod extends OtherModelUsage {
  models?: Record<string, OtherModelUsage>;
  estimateComplete: boolean;
}

export interface OtherUsageHistory {
  today: OtherUsagePeriod | null;
  yesterday: OtherUsagePeriod | null;
  last30Days: OtherUsagePeriod | null;
}

export type UsageScope = 'localDevice' | 'account';
export type UsageViewMode = 'all' | UsageScope;

export interface UsageHistory {
  today: UsagePeriod | null;
  yesterday: UsagePeriod | null;
  last30Days: UsagePeriod | null;
  daily: DailyUsage[];
  unknownModels: string[];
  otherUsage?: OtherUsageHistory | null;
}

export interface UsageHistories {
  localDevice?: UsageHistory;
  account?: UsageHistory;
}

export interface ProviderSnapshot {
  upstreamProviderId?: string | null;
  accounts?: Array<{ id: string; name: string; snapshot: ProviderSnapshot }>;
  providerId: string;
  plan: string | null;
  quotas: QuotaWindow[];
  valueMetrics: ValueMetric[];
  statusMetrics: StatusMetric[];
  notices: ProviderNotice[];
  usageHistories: UsageHistories;
  warnings: string[];
  refreshedAt: string;
}

export type ProviderErrorKind =
  | 'authentication'
  | 'permission'
  | 'rateLimited'
  | 'network'
  | 'invalidResponse'
  | 'credentialStorage'
  | 'localData'
  | 'storage'
  | 'internal';

export interface ProviderViewState {
  snapshot: ProviderSnapshot | null;
  source: 'none' | 'cache' | 'live';
  refreshing: boolean;
  stale: boolean;
  error: string | null;
  errorKind: ProviderErrorKind | null;
  lastAttemptAt: string | null;
}

export interface UsageViewState {
  providers: Record<string, ProviderViewState>;
  lastFullRefreshAt?: string | null;
  refreshIntervalSeconds?: number;
}

export type MetricSection = 'alwaysVisible' | 'onDemand';

export type MetricSource =
  | { kind: 'quota'; sourceId: string; sessionWindow: boolean }
  | { kind: 'quotaOrValue'; sourceId: string; sessionWindow: boolean }
  | { kind: 'value'; sourceId: string }
  | { kind: 'status'; sourceId: string }
  | { kind: 'usage'; period: 'today' | 'yesterday' | 'last30Days' }
  | { kind: 'trend' };

export interface TrayMetricDefinition {
  shortLabel: string;
  suffix: string | null;
}

export interface MetricDefinition {
  id: string;
  label: string;
  source: MetricSource;
  usageScope?: UsageScope;
  pinnable: boolean;
  defaultEnabled: boolean;
  defaultSection: MetricSection;
  defaultPinned: boolean;
  tray: TrayMetricDefinition | null;
}

export interface ProviderLink {
  label: string;
  url: string;
}

export type ApiKeyStatus = 'notSet' | 'fromEnvironment' | 'fromConfig' | 'saved' | 'overrideActive';

export interface ProviderApiKeyState {
  providerId: string;
  status: ApiKeyStatus;
}

export interface ApiKeyMutationOutcome extends ProviderApiKeyState {
  warning?: string;
}

export type Sub2ApiMode = 'subscription' | 'admin';
export type Sub2ApiSource = 'subscription' | Sub2ApiUpstream;

export interface Sub2ApiConfigState {
  useProviderKey?: boolean;
  otherMode?: Sub2ApiConfigState;
  mode: Sub2ApiMode;
  configured: boolean;
  baseUrl: string;
  codexProvider: string;
  customBaseUrl: boolean;
  email: string;
  upstream: Sub2ApiUpstream;
  providerSource?: Sub2ApiUpstream | null;
  /** Present when a save can keep the saved secret instead of asking for it again. */
  reusableCredential?: ReusableCredential;
}

/** What a save must match to keep a saved secret. A field left out matches anything. */
export interface ReusableCredential {
  baseUrl: string;
  customBaseUrl?: boolean;
  providerSource?: Sub2ApiUpstream;
  codexProvider?: string;
}

/** A saved, cleared or removed connection; the warning says provider status could not follow. */
export interface Sub2ApiConfigOutcome extends Sub2ApiConfigState {
  warning?: string;
}

export type Sub2ApiUpstream = 'codex' | 'claude' | 'antigravity' | 'grok';

export interface Sub2ApiConfigInput {
  useProviderKey?: boolean;
  mode: Sub2ApiMode;
  apiKey?: string;
  baseUrl: string;
  codexProvider: string;
  customBaseUrl: boolean;
  email: string;
  password: string;
  upstream: Sub2ApiUpstream;
  providerSource?: Sub2ApiUpstream | null;
}

export interface ProviderDefinition {
  id: string;
  displayName: string;
  shortName: string;
  fallbackEnabled: boolean;
  localUsageSourceNote: string | null;
  links: ProviderLink[];
  metrics: MetricDefinition[];
}

export interface ProviderCatalog {
  providers: ProviderDefinition[];
  apiKeyProviderIds?: string[];
  connectionConfigProviderIds?: string[];
}

export interface MetricLayout {
  id: string;
  enabled: boolean;
  section: MetricSection;
  pinned: boolean;
}

export interface ProviderLayout {
  id: string;
  enabled: boolean;
  detected: boolean;
  expanded: boolean;
  metrics: MetricLayout[];
}

export interface NotificationPreferences {
  fullyUsed: boolean;
  usageReset: boolean;
  almostOut: boolean;
  cuttingItClose: boolean;
  willRunOut: boolean;
}

export interface AppSettings {
  schemaVersion: number;
  providers: ProviderLayout[];
  knownProviderIds: string[];
  providerNames: Record<string, string>;
  /** Proxy URL or "direct"; absent entries use system routing. */
  providerProxies?: Record<string, string>;
  showTotalSpend: boolean;
  theme: 'system' | 'light' | 'dark';
  density: 'default' | 'compact';
  reduceAnimations: boolean;
  windowMode: 'popup' | 'floating';
  menuBarStyle: 'icon' | 'text' | 'bars';
  usageDisplay: 'used' | 'left';
  updateFrequency: 'oneMinute' | 'fiveMinutes' | 'adaptive';
  resetDisplay: 'countdown' | 'exact';
  timeFormat: 'system' | 'twelveHour' | 'twentyFourHour';
  alwaysShowPacing: boolean;
  launchAtLogin: boolean;
  autoCheckUpdates: boolean;
  dismissedUpdateVersion: string | null;
  lastUpdateCheckAt: string | null;
  globalShortcut: string | null;
  logLevel: 'error' | 'warn' | 'info' | 'debug';
  notifications: NotificationPreferences;
  totalSpendMetric: 'cost' | 'costPerMillion' | 'tokens';
  totalSpendPeriod: 'today' | 'yesterday' | 'last30Days';
  detectionNoticeDismissed: boolean;
}

export interface UpdateStatus {
  available: boolean;
  currentVersion: string;
  version: string | null;
  body: string | null;
  installable: boolean;
  releaseUrl: string;
}

export interface UpdateProgress {
  phase: 'downloading' | 'retrying' | 'installing';
  downloaded: number;
  total: number | null;
  percent: number | null;
}

export interface UpdateFailure {
  code: string;
  message: string;
  action: string;
  retryable: boolean;
}

/** What a Card shows and can Star, as the backend resolved it from the Card's Track. */
export interface MetricPolicy {
  /** The connection's Track; null for plain providers and Tracks not yet read. */
  track: Sub2ApiSource | null;
  visibleMetricIds: string[];
  /** Metrics that can be given a new Star. A Star already given on a visible metric can be removed. */
  starrableMetricIds: string[];
  maxStars: number;
}

export interface SettingsViewState {
  settings: AppSettings;
  settingsRevision: number;
  accountRevision: number;
  renamableProviderIds: string[];
  metricPolicies: Record<string, MetricPolicy>;
  notificationPermission: 'granted' | 'denied' | 'prompt' | 'unavailable';
  integrationError: string | null;
  trayAvailable: boolean;
  platformSummary: string | null;
}

export interface BootstrapState {
  usage: UsageViewState;
  settings: SettingsViewState;
  catalog: ProviderCatalog;
}

export interface ProxyExitLocation {
  ip: string;
  countryCode: string;
}
