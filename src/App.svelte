<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { getCurrentWindow } from '@tauri-apps/api/window';
  import {
    beginPanelResize,
    dismissMainWindow,
    getBootstrapState,
    reportFirstPanelFrame,
    getLogPath,
    getPanelHeightMode,
    getPanelResizeEdge,
    lockPanelResizeAxis,
    onOpenScreen,
    onMainWindowHidden,
    onMainWindowShown,
    onMenuBarSession,
    navigateMenuBar,
    onPanelBackdrop,
    onSettingsState,
    onUpdateProgress,
    onUsageState,
    openProviderLink as openProviderLinkCommand,
    openNotificationSettings as openSystemNotificationSettings,
    openLogFolder as openSystemLogFolder,
    quitApplication,
    refreshProviderUsage,
    refreshUsage,
    requestNotificationPermission,
    resetAllSettings as resetAllSettingsCommand,
    resetCustomization as resetCustomizationCommand,
    resetProviderCustomization as resetProviderCustomizationCommand,
    setPanelHeightAutomatic,
    setPanelHeightManual,
    type PanelHeightMode,
    type PanelResizeEdge,
  } from './lib/backend';
  import CustomizeProviderDetail from './lib/CustomizeProviderDetail.svelte';
  import CustomizeProviderList from './lib/CustomizeProviderList.svelte';
  import ConfirmationSheet from './lib/ConfirmationSheet.svelte';
  import { restoreCustomization } from './lib/customizationHistory';
  import Dashboard from './lib/Dashboard.svelte';
  import Icon from './lib/Icon.svelte';
  import { editsText, handleMenuBarKey } from './lib/menuKeys';
  import { dropFocusUntilInteraction } from './lib/initialFocus';
  import { createListenerRegistry } from './lib/listenerRegistry';
  import { emptyProviderCatalog, ProviderCatalogIndex } from './lib/metrics';
  import { springMotion } from './lib/motion';
  import { popupMenu } from './lib/nativeMenu';
  import OpenQuotaMark from './lib/OpenQuotaMark.svelte';
  import { horizontalPageTransition, shouldSlideBetweenScreens } from './lib/pageTransition';
  import { desktopPlatform, shortcutLabels } from './lib/platform';
  import { withProviderName } from './lib/providerNames';
  import { sub2ApiUpstreamAccountName, sub2ApiUpstreams } from './lib/sub2ApiUpstreams';
  import RenameProviderSheet from './lib/RenameProviderSheet.svelte';
  import {
    buildProviderShareRows,
    renderProviderShareCard,
    renderTotalSpendShareCard,
  } from './lib/shareCard';
  import SettingsScreen from './lib/SettingsScreen.svelte';
  import { SettingsController } from './lib/settingsController.svelte';
  import type { SpendProjection } from './lib/totalSpend';
  import type { AppSettings, UsageViewState } from './lib/types';
  import { UpdateController } from './lib/updateController.svelte';
  import { automaticUpdateDelay, UPDATE_CHECK_INTERVAL_MS } from './lib/updateSchedule';
  import { createWindowController, type AppScreen } from './lib/windowController';

  type Screen = AppScreen;
  const appVersion = import.meta.env.APP_VERSION;
  const emptyView: UsageViewState = { providers: {} };

  let viewState = $state<UsageViewState>(emptyView);
  let catalog = $state<ProviderCatalogIndex>(emptyProviderCatalog);
  let screen = $state<Screen>('dashboard');
  let now = $state(Date.now());
  let settingsError = $state<string | null>(null);
  let bootstrapReady = $state(false);
  let systemReducedMotion = $state(false);
  let slideDirection = $state(1);
  let slidePageTransition = $state(false);
  let customizationHistory = $state<AppSettings[]>([]);
  let customizationGestureStart: AppSettings | null = null;
  let reordering = $state(false);
  let confirmationMessage = $state<string | null>(null);
  let resetConfirmationOpen = $state(false);
  let settingsResetConfirmationOpen = $state(false);
  let resettingCustomization = $state(false);
  let resettingAllSettings = $state(false);
  let resettingProviderId = $state<string | null>(null);
  let showAbout = $state(false);
  let aboutTrigger: HTMLElement | null = null;
  let aboutCloseButton = $state<HTMLButtonElement>();
  let shareTimer: ReturnType<typeof setTimeout> | undefined;
  const providerStates = $derived(Object.values(viewState.providers));
  const anyRefreshing = $derived(providerStates.some((state) => state.refreshing));
  const platform = desktopPlatform();
  const shortcuts = shortcutLabels(platform);
  const settingsController = new SettingsController((message) => (settingsError = message));
  const settingsState = $derived(bootstrapReady ? settingsController.state : null);
  const reducedMotion = $derived(
    systemReducedMotion || Boolean(settingsState?.settings.reduceAnimations),
  );
  const floatingWindow = $derived(
    !!settingsState &&
      (!settingsState.trayAvailable || settingsState.settings.windowMode === 'floating'),
  );
  const providerDisplayName = (id: string) =>
    catalog.resolvedDisplayName(id, settingsState?.settings.providerNames);
  const updates = new UpdateController();
  let resizeEdge = $state<PanelResizeEdge>(platform === 'windows' ? 'top' : 'bottom');
  const renderedResizeEdge = $derived(floatingWindow ? 'bottom' : resizeEdge);
  let panelHeightMode = $state<PanelHeightMode>('automatic');
  let panelHeightModeRequest = 0;
  let panelHeightModeMutation: Promise<void> = Promise.resolve();
  let renameCard = $state<{ id: string; initialValue: string } | null>(null);
  let lastResizeGripPointerAt = Number.NEGATIVE_INFINITY;
  let panelResizeOperation: Promise<void> | null = null;
  const windowController = createWindowController({
    screen: () => screen,
    refreshing: () => anyRefreshing,
    reordering: () => reordering,
    automatic: () => panelHeightMode === 'automatic',
    reducedMotion: () => reducedMotion,
    onError: (message) => (settingsError = message),
  });

  $effect(() => {
    const root = document.documentElement;
    root.toggleAttribute('data-reduced-motion', reducedMotion);
    if (!settingsState) return;
    if (settingsState.settings.theme === 'system') delete root.dataset.theme;
    else root.dataset.theme = settingsState.settings.theme;
    root.dataset.density = settingsState.settings.density;
  });

  $effect(() => {
    if (!bootstrapReady || !settingsState?.settings.autoCheckUpdates) return;
    const delay = automaticUpdateDelay(settingsState.settings.lastUpdateCheckAt);
    let interval: ReturnType<typeof setInterval> | undefined;
    const timer = setTimeout(() => {
      void checkForUpdates();
      interval = setInterval(() => void checkForUpdates(), UPDATE_CHECK_INTERVAL_MS);
    }, delay);
    return () => {
      clearTimeout(timer);
      if (interval) clearInterval(interval);
    };
  });

  function scheduleWindowFit() {
    windowController.scheduleFit();
  }

  function beginContentMorph() {
    windowController.beginContentMorph();
  }

  function closeMainWindow() {
    resetTransientUi();
    navigate('dashboard');
    void dismissMainWindow();
  }
  function resetTransientUi() {
    showAbout = false;
    resetConfirmationOpen = false;
    settingsResetConfirmationOpen = false;
    renameCard = null;
    resettingCustomization = false;
    resettingAllSettings = false;
    resettingProviderId = null;
    confirmationMessage = null;
    const content = document.querySelector<HTMLElement>('.content');
    if (content && typeof content.scrollTo === 'function') content.scrollTo({ top: 0 });
    else if (content) content.scrollTop = 0;
  }
  function quitApp() {
    void quitApplication();
  }
  function screenRank(value: Screen) {
    if (value.startsWith('provider:')) return 2;
    return value === 'dashboard' ? 0 : 1;
  }
  function navigate(next: Screen) {
    if (next === screen) return;
    slidePageTransition = shouldSlideBetweenScreens(screen, next);
    slideDirection = screenRank(next) >= screenRank(screen) ? 1 : -1;
    screen = next;
    if (next === 'settings' && bootstrapReady) void settingsController.refreshIfIdle();
  }
  async function openProviderCustomization(providerId: string, focusBack = false) {
    navigate(`provider:${providerId}`);
    if (!focusBack) return;
    await tick();
    document.querySelector<HTMLButtonElement>('.screen-header button[aria-label="Back"]')?.focus();
  }
  function back() {
    if (screen.startsWith('provider:')) navigate('customize');
    else if (screen !== 'dashboard') navigate('dashboard');
    else closeMainWindow();
  }
  function saveSettings(next: AppSettings) {
    settingsError = null;
    const windowModeChanged = settingsState?.settings.windowMode !== next.windowMode;
    if (windowModeChanged) beginContentMorph();
    const save = settingsController.save(next);
    if (windowModeChanged) void save.finally(updatePanelResizeEdge);
  }

  function toggleFloatingWindow() {
    const current = settingsState;
    if (!current?.trayAvailable) return;
    saveSettings({
      ...current.settings,
      windowMode: floatingWindow ? 'popup' : 'floating',
    });
  }

  function cloneSettings(value: AppSettings): AppSettings {
    return JSON.parse(JSON.stringify(value)) as AppSettings;
  }
  function showConfirmation(message: string) {
    confirmationMessage = message;
    if (shareTimer) clearTimeout(shareTimer);
    shareTimer = setTimeout(() => (confirmationMessage = null), 1800);
  }
  function saveCustomization(next: AppSettings) {
    const current = settingsState;
    if (!current) return;
    if (customizationGestureStart) {
      settingsController.setDraftSettings(next);
      settingsError = null;
      return;
    }
    customizationHistory = [...customizationHistory.slice(-19), cloneSettings(current.settings)];
    saveSettings(next);
  }
  function openRenameProvider(providerId: string) {
    const current = settingsState;
    if (!current) return;
    renameCard = {
      id: providerId,
      initialValue: current.settings.providerNames[providerId] ?? '',
    };
  }
  async function closeRenameProvider() {
    const providerId = renameCard?.id;
    renameCard = null;
    if (!providerId) return;
    await tick();
    const provider = [...document.querySelectorAll<HTMLElement>('[data-provider-id]')].find(
      (element) => element.dataset.providerId === providerId,
    );
    provider?.querySelector<HTMLElement>('[data-reorder-touch-handle]')?.focus();
  }
  function renameProvider(name: string) {
    const current = settingsState;
    if (!current || !renameCard) return;
    const changed = withProviderName(current.settings, renameCard.id, name);
    if (changed !== current.settings) saveSettings(changed);
    void closeRenameProvider();
  }
  function beginCustomizationGesture() {
    if (!settingsState) return;
    if (!customizationGestureStart) {
      customizationGestureStart = cloneSettings(settingsState.settings);
      settingsController.beginDraft();
    }
    reordering = true;
    scheduleWindowFit();
  }
  function endCustomizationGesture(moved: boolean, cancelled = false) {
    const current = settingsState;
    if (!current) {
      settingsController.endDraft();
      return;
    }
    const start = customizationGestureStart;
    const final = current.settings;
    customizationGestureStart = null;
    reordering = false;
    if (start && moved && cancelled) settingsController.setDraftSettings(start);
    else if (start && moved) {
      customizationHistory = [...customizationHistory.slice(-19), start];
      saveSettings(final);
    }
    settingsController.endDraft();
    queueMicrotask(scheduleWindowFit);
  }
  function undoCustomization() {
    const current = settingsState;
    const previous = customizationHistory.at(-1);
    if (!current || !previous) return;
    customizationHistory = customizationHistory.slice(0, -1);
    saveSettings(restoreCustomization(current.settings, previous));
  }
  async function refresh() {
    if (anyRefreshing) return;
    viewState = {
      ...viewState,
      providers: Object.fromEntries(
        Object.entries(viewState.providers).map(([id, state]) => [
          id,
          { ...state, refreshing: true },
        ]),
      ),
    };
    try {
      viewState = await refreshUsage();
    } catch {
      viewState = {
        ...viewState,
        providers: Object.fromEntries(
          Object.entries(viewState.providers).map(([id, state]) => [
            id,
            { ...state, refreshing: false },
          ]),
        ),
      };
      settingsError = 'OpenQuota could not start a provider refresh.';
    }
  }
  async function refreshProvider(providerId: string) {
    const current = viewState.providers[providerId];
    if (!current || current.refreshing) return;
    viewState = {
      ...viewState,
      providers: {
        ...viewState.providers,
        [providerId]: { ...current, refreshing: true },
      },
    };
    try {
      viewState = await refreshProviderUsage(providerId);
    } catch {
      const failed = viewState.providers[providerId];
      if (failed) {
        viewState = {
          ...viewState,
          providers: {
            ...viewState.providers,
            [providerId]: { ...failed, refreshing: false },
          },
        };
      }
      settingsError = `${providerDisplayName(providerId)} usage could not be refreshed.`;
    }
  }
  function openProviderLink(providerId: string, linkIndex: number) {
    void openProviderLinkCommand(providerId, linkIndex).catch(() => {});
  }
  function requestCustomizationReset() {
    resetConfirmationOpen = true;
  }
  async function confirmCustomizationReset() {
    const current = settingsState;
    if (!current || resettingCustomization) return;
    resettingCustomization = true;
    const previous = cloneSettings(current.settings);
    try {
      await settingsController.runMutation((expectedSettingsRevision, expectedAccountRevision) =>
        resetCustomizationCommand(expectedSettingsRevision, expectedAccountRevision),
      );
      customizationHistory = [...customizationHistory.slice(-19), previous];
    } catch {
      settingsError = 'Customization could not be reset.';
    } finally {
      resettingCustomization = false;
      resetConfirmationOpen = false;
    }
  }
  async function resetProviderCustomization(providerId: string) {
    const current = settingsState;
    if (!current || resettingProviderId) return;
    const provider = current.settings.providers.find((item) => item.id === providerId);
    if (!provider) return;
    const previous = cloneSettings(current.settings);
    resettingProviderId = providerId;
    try {
      await settingsController.runMutation((expectedSettingsRevision, expectedAccountRevision) =>
        resetProviderCustomizationCommand(
          providerId,
          expectedSettingsRevision,
          expectedAccountRevision,
        ),
      );
      customizationHistory = [...customizationHistory.slice(-19), previous];
    } catch {
      settingsError = `${providerDisplayName(providerId)} customization could not be reset.`;
    } finally {
      resettingProviderId = null;
    }
  }
  async function confirmAllSettingsReset() {
    if (!settingsState || resettingAllSettings) return;
    const windowModeChanged = settingsState.settings.windowMode !== 'popup';
    if (windowModeChanged) beginContentMorph();
    resettingAllSettings = true;
    try {
      await panelHeightModeMutation;
      await settingsController.runMutation((expectedSettingsRevision, expectedAccountRevision) =>
        resetAllSettingsCommand(expectedSettingsRevision, expectedAccountRevision),
      );
      customizationHistory = [];
      updatePanelHeightMode();
      updatePanelResizeEdge();
      settingsError = null;
      showConfirmation('All settings restored');
    } catch {
      settingsError = 'Settings could not be reset.';
      updatePanelHeightMode();
    } finally {
      resettingAllSettings = false;
      settingsResetConfirmationOpen = false;
    }
  }
  async function copyCanvas(canvas: HTMLCanvasElement, fallback: string) {
    const blob = await new Promise<Blob>((resolve, reject) =>
      canvas.toBlob(
        (value) => (value ? resolve(value) : reject(new Error('PNG unavailable'))),
        'image/png',
      ),
    );
    if (typeof ClipboardItem !== 'undefined' && navigator.clipboard.write) {
      await navigator.clipboard.write([new ClipboardItem({ 'image/png': blob })]);
    } else {
      await navigator.clipboard.writeText(fallback);
    }
    showConfirmation('Copied to clipboard');
  }
  async function shareProvider(
    providerId: string,
    viewMode: import('./lib/types').UsageViewMode = 'all',
    accountId?: string,
    accountExpansion: Record<string, boolean> = {},
  ) {
    const current = settingsState;
    if (!current) return;
    const parentSnapshot = viewState.providers[providerId]?.snapshot;
    const card = document.querySelector<HTMLElement>(
      `[data-provider-id="${providerId}"]${accountId ? ` [data-account-id="${accountId}"]` : ''}`,
    );
    if (!card) return;
    const account = parentSnapshot?.accounts?.find((account) => account.id === accountId);
    const provider = account?.snapshot ?? parentSnapshot;
    const layout = current.settings.providers.find((item) => item.id === providerId);
    if (!provider || !layout) return;
    const snapshot = [providerDisplayName(providerId), card.innerText.trim()].join('\n');
    try {
      const accounts =
        !accountId && (provider.accounts?.length ?? 0) > 1 ? provider.accounts : undefined;
      const buildRows = (snapshot: import('./lib/types').ProviderSnapshot, id?: string) =>
        buildProviderShareRows(
          catalog,
          snapshot,
          {
            ...layout,
            expanded:
              id === undefined
                ? layout.expanded
                : (accountExpansion[`${providerId}/${id}`] ?? layout.expanded),
          },
          current.settings,
          now,
          viewMode,
        );
      const rows = accounts
        ? accounts.flatMap((upstream) => [
            {
              kind: 'account' as const,
              label: sub2ApiUpstreamAccountName(upstream.name),
              plan: upstream.snapshot.plan,
            },
            ...buildRows(upstream.snapshot, upstream.id),
          ])
        : buildRows(provider, account?.id ?? provider.accounts?.[0]?.id);

      const canvas = renderProviderShareCard(catalog, {
        providerId,
        providerNames: current.settings.providerNames,
        plan: account
          ? [sub2ApiUpstreamAccountName(account.name), provider.plan].filter(Boolean).join(' · ')
          : accounts
            ? null
            : provider.plan,
        rows,
      });
      await copyCanvas(canvas, snapshot);
    } catch {
      settingsError = 'Provider screenshot could not be copied.';
    }
  }
  async function shareTotalSpend(projections: SpendProjection[]) {
    const current = settingsState;
    if (!current) return false;
    const card = document.querySelector<HTMLElement>('[data-total-spend]');
    if (!card) return false;
    try {
      const canvas = renderTotalSpendShareCard(catalog, {
        projections,
        providerNames: current.settings.providerNames,
        metric: current.settings.totalSpendMetric,
        period: current.settings.totalSpendPeriod,
      });
      await copyCanvas(canvas, card.innerText.trim());
      return true;
    } catch {
      settingsError = 'Total Spend screenshot could not be copied.';
      return false;
    }
  }
  async function copyLogPath() {
    const path = await getLogPath();
    await navigator.clipboard.writeText(path);
    showConfirmation('Log path copied');
  }
  async function openLogFolder() {
    await openSystemLogFolder();
  }
  function topBarTitle() {
    if (screen.startsWith('provider:')) {
      const id = screen.slice(9);
      return catalog.configurationDisplayName(
        id,
        settingsState?.settings.providerNames,
        $sub2ApiUpstreams[id],
      );
    }
    return screen === 'settings' ? 'Settings' : 'Customize';
  }
  async function openAbout() {
    aboutTrigger = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    showAbout = true;
    await tick();
    aboutCloseButton?.focus();
  }
  async function closeAbout() {
    showAbout = false;
    await tick();
    aboutTrigger?.focus();
    aboutTrigger = null;
  }
  function closeAboutFromBackdrop(event: MouseEvent) {
    if (event.target === event.currentTarget) void closeAbout();
  }
  function handleAboutKeydown(event: KeyboardEvent) {
    event.stopPropagation();
    if (event.key === 'Escape') {
      event.preventDefault();
      void closeAbout();
    } else if (event.key === 'Tab') {
      event.preventDefault();
      aboutCloseButton?.focus();
    }
  }
  function ownsEnterKey(target: EventTarget | null) {
    if (!(target instanceof Element)) return false;
    return (
      target.closest(
        'button, a, input, select, textarea, summary, [contenteditable], [role="button"], [role="menuitem"], [role="option"], [role="combobox"]',
      ) !== null
    );
  }
  // Right-clicking the dashboard where nothing has a menu of its own opens
  // Options; elsewhere the page has no context menu.
  function openPageMenu(event: MouseEvent) {
    // A card that opened its own menu has already claimed the event.
    if (event.defaultPrevented) return;
    event.preventDefault();
    if (settingsState && screen === 'dashboard') {
      openOptionsMenu({ x: event.clientX, y: event.clientY });
    }
  }
  // The system draws Options, as it does every menu of the platform: it
  // highlights, keys, dismisses and nests Share Screenshot itself.
  function openOptionsMenu(at: { x: number; y: number }) {
    const providers =
      settingsState?.settings.providers.filter(
        (provider) => provider.enabled && catalog.provider(provider.id),
      ) ?? [];
    void popupMenu(
      [
        ...(settingsState?.trayAvailable
          ? [
              {
                text: 'Keep Window Open',
                checked: floatingWindow,
                action: toggleFloatingWindow,
              },
              'separator' as const,
            ]
          : []),
        {
          text: 'Refresh All',
          accelerator: 'CmdOrCtrl+R',
          enabled: !anyRefreshing,
          action: () => void refresh(),
        },
        'separator',
        { text: 'Customize', action: () => navigate('customize') },
        { text: 'Settings', accelerator: 'CmdOrCtrl+,', action: () => navigate('settings') },
        'separator',
        {
          text: 'Share Screenshot',
          enabled: providers.length > 0,
          items: providers.map((provider) => ({
            text: providerDisplayName(provider.id),
            action: () => void shareProvider(provider.id),
          })),
        },
        { text: 'Check for Updates…', action: () => void checkForUpdates(true) },
        'separator',
        { text: 'About OpenQuota', action: () => void openAbout() },
        { text: 'Quit OpenQuota', accelerator: 'CmdOrCtrl+Q', action: quitApp },
      ],
      at,
    ).catch(() => undefined);
  }
  function handleWindowPointerDown(event: PointerEvent) {
    if (
      event.target instanceof Element &&
      event.target.closest('.floating-chrome__drag') !== null
    ) {
      handleFloatingWindowPointerDown(event);
    }
  }
  function updatePanelResizeEdge() {
    if (!('__TAURI_INTERNALS__' in window)) return;
    void getPanelResizeEdge()
      .then((edge) => (resizeEdge = edge))
      .catch(() => undefined);
  }
  function updatePanelHeightMode() {
    if (!('__TAURI_INTERNALS__' in window)) return;
    const request = ++panelHeightModeRequest;
    void getPanelHeightMode()
      .then((mode) => {
        if (request !== panelHeightModeRequest) return;
        panelHeightMode = mode;
        if (mode === 'automatic') scheduleWindowFit();
      })
      .catch(() => undefined);
  }
  function acceptPanelHeightMode(mode: PanelHeightMode) {
    panelHeightModeRequest += 1;
    panelHeightMode = mode;
  }
  function handlePanelResizePointerDown(event: PointerEvent) {
    if (event.button !== 0) return;
    event.preventDefault();
    event.stopPropagation();
    const pointerAt = event.timeStamp;
    const repeatedPress = event.detail > 1 || pointerAt - lastResizeGripPointerAt <= 400;
    lastResizeGripPointerAt = repeatedPress ? Number.NEGATIVE_INFINITY : pointerAt;
    if (repeatedPress) {
      const activeResize = panelResizeOperation;
      void (async () => {
        if (activeResize) await activeResize;
        await changePanelHeightMode('automatic');
      })();
      return;
    }
    const operation = (async () => {
      try {
        await panelHeightModeMutation;
        const edge = await beginPanelResize();
        resizeEdge = edge;
        // The native begin command has already persisted the current height as manual. Mirroring it
        // here stops any in-flight frontend auto-fit without waiting for the first resize event.
        acceptPanelHeightMode('manual');
        // TODO(macOS): Tao 0.35 reports native resize dragging as unsupported and Tauri currently
        // swallows that runtime error. Re-test after Tauri/Tao upgrades; add an AppKit fallback if
        // upstream support is still unavailable.
        await getCurrentWindow().startResizeDragging(edge === 'top' ? 'North' : 'South');
      } catch {
        settingsError = 'OpenQuota panel resize could not be started.';
      } finally {
        await lockPanelResizeAxis().catch(() => undefined);
        updatePanelHeightMode();
      }
    })();
    panelResizeOperation = operation;
    void operation.finally(() => {
      if (panelResizeOperation === operation) panelResizeOperation = null;
    });
  }
  function handleFloatingWindowPointerDown(event: PointerEvent) {
    if (event.button !== 0 || !('__TAURI_INTERNALS__' in window)) return;
    event.preventDefault();
    void getCurrentWindow()
      .startDragging()
      .catch(() => (settingsError = 'OpenQuota window could not be moved.'));
  }
  async function changePanelHeightMode(mode: PanelHeightMode) {
    if (!('__TAURI_INTERNALS__' in window)) return;
    const request = ++panelHeightModeRequest;
    const operation = panelHeightModeMutation.then(() =>
      mode === 'automatic' ? setPanelHeightAutomatic() : setPanelHeightManual(),
    );
    panelHeightModeMutation = operation.catch(() => undefined);
    try {
      await operation;
      if (request === panelHeightModeRequest) updatePanelHeightMode();
    } catch {
      if (request !== panelHeightModeRequest) return;
      settingsError = 'OpenQuota could not change the panel height mode.';
      updatePanelHeightMode();
    }
  }
  async function requestNotifications() {
    if (!settingsState) return;
    try {
      const permissionState = await requestNotificationPermission();
      settingsController.acceptExternalState(permissionState);
    } catch {
      settingsError = 'Notification permission could not be requested.';
    }
  }
  async function openNotificationSettings() {
    try {
      await openSystemNotificationSettings();
    } catch {
      settingsError = 'Notification settings could not be opened on this system.';
    }
  }
  async function checkForUpdates(manual = false) {
    if (!settingsState) return;
    await updates.check(
      manual,
      (checkedAt) => {
        if (!settingsState) return;
        saveSettings({ ...settingsState.settings, lastUpdateCheckAt: checkedAt });
      },
      showConfirmation,
    );
  }

  onMount(() => {
    const motionQuery = window.matchMedia('(prefers-reduced-motion: reduce)');
    const updateMotionPreference = () => {
      systemReducedMotion = motionQuery.matches;
      scheduleWindowFit();
    };
    updateMotionPreference();
    motionQuery.addEventListener('change', updateMotionPreference);
    const refreshWindowState = () => {
      if (bootstrapReady && screen === 'settings') void settingsController.refreshIfIdle();
      updatePanelResizeEdge();
      updatePanelHeightMode();
      scheduleWindowFit();
    };
    updatePanelResizeEdge();
    updatePanelHeightMode();
    window.addEventListener('focus', refreshWindowState);
    const initialFocus = dropFocusUntilInteraction();

    const popover = document.querySelector<HTMLElement>('.popover');
    const resizeObserver =
      typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(scheduleWindowFit);
    const observePanelParts = () => {
      resizeObserver?.disconnect();
      document
        .querySelectorAll<HTMLElement>('.floating-chrome, .screen-page, .screen-header, .notice')
        .forEach((element) => resizeObserver?.observe(element));
      scheduleWindowFit();
    };
    const mutationObserver = new MutationObserver(observePanelParts);
    if (popover) {
      mutationObserver.observe(popover, { childList: true, subtree: true, characterData: true });
    }
    observePanelParts();
    const handleKeydown = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.isComposing) return;
      if (event.key === 'Escape') {
        event.preventDefault();
        if (showAbout) {
          void closeAbout();
          return;
        }
        back();
      } else if (
        screen !== 'dashboard' &&
        ((platform === 'macos' &&
          event.metaKey &&
          !event.ctrlKey &&
          !event.altKey &&
          !event.shiftKey &&
          (event.key === '[' || event.code === 'BracketLeft')) ||
          (platform !== 'macos' &&
            event.altKey &&
            !event.ctrlKey &&
            !event.metaKey &&
            !event.shiftKey &&
            event.key === 'ArrowLeft'))
      ) {
        event.preventDefault();
        back();
      } else if (event.key === 'Enter' && screen === 'dashboard' && !ownsEnterKey(event.target)) {
        event.preventDefault();
        navigate('customize');
      } else if ((event.ctrlKey || event.metaKey) && event.key === ',') {
        event.preventDefault();
        navigate(screen === 'settings' ? 'dashboard' : 'settings');
      } else if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'r') {
        event.preventDefault();
        void refresh();
      } else if (
        (event.ctrlKey || event.metaKey) &&
        event.key.toLowerCase() === 'z' &&
        !(event.target instanceof HTMLInputElement) &&
        !(event.target instanceof HTMLTextAreaElement)
      ) {
        event.preventDefault();
        undoCustomization();
      } else if (
        (event.ctrlKey || event.metaKey) &&
        event.key.toLowerCase() === 'q' &&
        !(event.target instanceof HTMLInputElement) &&
        !(event.target instanceof HTMLTextAreaElement)
      ) {
        event.preventDefault();
        quitApp();
      }
    };
    document.addEventListener('keydown', handleKeydown);
    // Held open by keyboard navigation of the menu bar (macOS 27), the window
    // passes on the ←/→ it does not use; see menuKeys.
    let navigating = false;
    const handleMenuKeydown = (event: KeyboardEvent) =>
      handleMenuBarKey(event, navigating, (right) => void navigateMenuBar(right).catch(() => {}));
    document.addEventListener('keydown', handleMenuKeydown);
    const clock = window.setInterval(() => (now = Date.now()), 30_000);
    const listeners = createListenerRegistry(() => {
      settingsError ??= 'OpenQuota event bridge is unavailable.';
    });
    if (desktopPlatform() === 'linux') {
      listeners.add(
        onPanelBackdrop((payload) => {
          const root = document.documentElement;
          root.classList.toggle('composited', payload.composited);
          root.style.setProperty('--backdrop-size', `${payload.width}px ${payload.height}px`);
          root.style.setProperty('--backdrop-position', `${-payload.x}px ${-payload.y}px`);
          if (payload.image === null) return;
          root.style.setProperty(
            '--backdrop-image',
            payload.image ? `url("${payload.image}")` : 'none',
          );
          root.classList.toggle('frosted', Boolean(payload.image));
        }),
      );
    }
    listeners.add(onUsageState((state) => (viewState = state)));
    listeners.add(
      onSettingsState((state) => {
        settingsController.acceptExternalState(state);
      }),
    );
    listeners.add(
      onOpenScreen((target) => navigate(target === 'settings' ? 'settings' : 'customize')),
    );
    listeners.add(
      onMenuBarSession(({ active, keyboard }) => {
        navigating = active && keyboard;
        // Keys start on the window, not in a text field left focused, where
        // ←/→ would not move along the menu bar.
        if (navigating && editsText(document.activeElement)) {
          (document.activeElement as HTMLElement).blur();
        }
      }),
    );
    listeners.add(
      onMainWindowHidden(() => {
        windowController.setVisible(false);
        navigating = false;
        initialFocus.reset();
        resetTransientUi();
        slidePageTransition = false;
        screen = 'dashboard';
      }),
    );
    listeners.add(onMainWindowShown(() => windowController.setVisible(true)));
    listeners.add(
      onUpdateProgress((progress) => {
        updates.setProgress(progress);
      }),
    );
    // A hidden WebView can wait a long time for its first visible frame. Keep
    // the IPC duration separate from the age of the document in the local log.
    let frame = 0;
    let disposed = false;
    let bootstrapMs: number | undefined;
    function observeFirstFrame() {
      if (bootstrapMs === undefined || document.visibilityState !== 'visible' || frame) return;
      frame = requestAnimationFrame(() => {
        frame = requestAnimationFrame(() => {
          frame = 0;
          if (document.visibilityState !== 'visible' || bootstrapMs === undefined) return;
          void reportFirstPanelFrame(bootstrapMs, Math.round(performance.now())).catch(() => {});
          document.removeEventListener('visibilitychange', observeFirstFrame);
          bootstrapMs = undefined;
        });
      });
    }
    document.addEventListener('visibilitychange', observeFirstFrame);
    const requestedAt = performance.now();
    void getBootstrapState()
      .then((state) => {
        if (disposed) return;
        bootstrapMs = Math.round(performance.now() - requestedAt);
        catalog = new ProviderCatalogIndex(state.catalog);
        viewState = state.usage;
        settingsController.setState(state.settings);
        bootstrapReady = true;
        observeFirstFrame();
        if (screen === 'settings') void settingsController.refreshIfIdle();
      })
      .catch(() => (settingsError = 'OpenQuota backend is unavailable.'));
    return () => {
      disposed = true;
      document.removeEventListener('visibilitychange', observeFirstFrame);
      cancelAnimationFrame(frame);
      document.removeEventListener('keydown', handleKeydown);
      document.removeEventListener('keydown', handleMenuKeydown);
      window.clearInterval(clock);
      windowController.dispose();
      motionQuery.removeEventListener('change', updateMotionPreference);
      window.removeEventListener('focus', refreshWindowState);
      initialFocus.dispose();
      document.documentElement.removeAttribute('data-reduced-motion');
      mutationObserver.disconnect();
      resizeObserver?.disconnect();
      listeners.dispose();
    };
  });
</script>

<svelte:head><meta name="color-scheme" content="light dark" /></svelte:head>
<svelte:window onpointerdown={handleWindowPointerDown} />

<main
  class="popover"
  class:popover--floating={floatingWindow}
  class:popover--macos={floatingWindow && platform === 'macos'}
  aria-label="OpenQuota usage dashboard"
  oncontextmenu={openPageMenu}
>
  <p id="reorder-instructions" class="sr-only">
    Drag to reorder. With a keyboard, use Alt plus Up Arrow or Alt plus Down Arrow.
  </p>
  {#if renderedResizeEdge === 'top'}
    <div
      class="panel-resize-dragger panel-resize-dragger--top"
      role="separator"
      aria-label="Resize panel height"
      aria-orientation="horizontal"
      onpointerdown={handlePanelResizePointerDown}
    ></div>
  {/if}
  {#if floatingWindow}
    <header class="floating-chrome" aria-label="OpenQuota window controls">
      <div class="floating-chrome__drag">
        <OpenQuotaMark size={14} />
        <span>OpenQuota</span>
      </div>
      <button
        class="floating-chrome__close"
        type="button"
        aria-label={settingsState?.trayAvailable ? 'Hide OpenQuota' : 'Close OpenQuota'}
        onclick={closeMainWindow}
      >
        <Icon name="close" size={12} strokeWidth={2.1} />
      </button>
    </header>
  {/if}
  {#if settingsState}
    {#if screen !== 'dashboard'}
      <header class="screen-header app-top-bar">
        <button
          type="button"
          onclick={back}
          aria-label="Back"
          aria-keyshortcuts={platform === 'macos' ? 'Meta+[' : 'Alt+ArrowLeft'}
          data-tooltip={`Back (${shortcuts.back})`}
        >
          <Icon name="back" size={16} strokeWidth={2.2} />
        </button>
        <h1>{topBarTitle()}</h1>
        {#if screen === 'customize'}
          <button
            class="text-button"
            type="button"
            onclick={requestCustomizationReset}
            aria-label="Reset all customization"
            data-tooltip="Reset All Customization"
            ><Icon name="reset" size={15} strokeWidth={2} /></button
          >
        {:else if screen.startsWith('provider:')}
          <button
            class="text-button"
            type="button"
            disabled={resettingProviderId !== null}
            onclick={() => resetProviderCustomization(screen.slice(9))}
            aria-label={`Reset ${topBarTitle()}`}
            data-tooltip={`Reset ${topBarTitle()}`}
            ><Icon name="reset" size={15} strokeWidth={2} /></button
          >
        {:else}
          <span></span>
        {/if}
      </header>
    {/if}
    <div class="content" class:content--chrome={screen !== 'dashboard'}>
      {#if settingsError}<div class="notice notice--blocking" role="alert">
          {settingsError}
        </div>{/if}
      <div class="screen-stage">
        {#key screen}
          <div
            class="screen-page"
            data-screen={screen}
            in:horizontalPageTransition={{
              direction: slideDirection,
              ...springMotion(reducedMotion || !slidePageTransition),
            }}
            out:horizontalPageTransition={{
              direction: -slideDirection,
              ...springMotion(reducedMotion || !slidePageTransition),
            }}
          >
            {#if screen === 'dashboard'}
              <Dashboard
                {viewState}
                {catalog}
                renamableProviderIds={settingsState.renamableProviderIds}
                settings={settingsState.settings}
                {now}
                onSettingsChange={saveSettings}
                onCustomizationChange={saveCustomization}
                onReorderStart={beginCustomizationGesture}
                onReorderEnd={endCustomizationGesture}
                onCustomize={() => navigate('customize')}
                onOpenProviderCustomize={(id) => void openProviderCustomization(id, true)}
                onRenameProvider={openRenameProvider}
                onShare={shareProvider}
                onShareTotal={shareTotalSpend}
                onRefresh={refreshProvider}
                onOpenProviderLink={openProviderLink}
                onContentMorph={beginContentMorph}
                {reducedMotion}
                updateStatus={updates.status}
                installingUpdate={updates.installing}
                updateProgress={updates.progress}
                updateError={updates.error}
                onInstallUpdate={() => updates.install()}
                onOpenUpdatePage={() => updates.openDownloadPage()}
              />
            {:else if screen === 'settings'}
              <SettingsScreen
                settingsView={settingsState}
                {platform}
                {panelHeightMode}
                onChange={saveSettings}
                onPanelHeightModeChange={(mode) => void changePanelHeightMode(mode)}
                onRequestNotifications={requestNotifications}
                onOpenNotificationSettings={openNotificationSettings}
                updateError={updates.error}
                checkingUpdate={updates.checking}
                onCheckForUpdates={() => void checkForUpdates(true)}
                onCustomize={() => navigate('customize')}
                onCopyLogPath={copyLogPath}
                onOpenLogFolder={openLogFolder}
                onResetAllSettings={() => (settingsResetConfirmationOpen = true)}
              />
            {:else if screen === 'customize'}
              <CustomizeProviderList
                settings={settingsState.settings}
                {catalog}
                onOpen={(id) => void openProviderCustomization(id)}
                onChange={saveCustomization}
                onReorderStart={beginCustomizationGesture}
                onReorderEnd={endCustomizationGesture}
                onSettings={() => navigate('settings')}
                {reducedMotion}
              />
            {:else if screen.startsWith('provider:')}
              <CustomizeProviderDetail
                settings={settingsState.settings}
                providerId={screen.slice(9)}
                {catalog}
                renamableProviderIds={settingsState.renamableProviderIds}
                onChange={saveCustomization}
                onNameChange={saveSettings}
                onReorderStart={beginCustomizationGesture}
                onReorderEnd={endCustomizationGesture}
                onProviderRemoved={() => navigate('customize')}
                onContentMorph={beginContentMorph}
                {reducedMotion}
              />
            {/if}
          </div>
        {/key}
      </div>
    </div>

    {#if confirmationMessage}
      <div class="transient-pill" role="status">
        <Icon name="check" size={15} strokeWidth={2.4} />{confirmationMessage}
      </div>
    {/if}

    {#if resetConfirmationOpen}
      <ConfirmationSheet
        title="Reset All Customization?"
        message="This turns installed providers back on and restores every provider's metric visibility and order."
        confirmLabel="Reset All"
        pending={resettingCustomization}
        onConfirm={() => void confirmCustomizationReset()}
        onCancel={() => (resetConfirmationOpen = false)}
      />
    {/if}

    {#if settingsResetConfirmationOpen}
      <ConfirmationSheet
        title="Reset All Settings?"
        message="This restores appearance, notifications, shortcuts, updates, panel sizing, provider names, and layout. Provider sign-ins, API keys, and usage history stay in place. This cannot be undone."
        confirmLabel="Reset All"
        pending={resettingAllSettings}
        onConfirm={() => void confirmAllSettingsReset()}
        onCancel={() => (settingsResetConfirmationOpen = false)}
      />
    {/if}

    {#if renameCard}
      <RenameProviderSheet
        initialValue={renameCard.initialValue}
        onRename={renameProvider}
        onCancel={() => void closeRenameProvider()}
      />
    {/if}

    {#if showAbout}
      <div
        class="about-backdrop"
        role="presentation"
        onclick={closeAboutFromBackdrop}
        onkeydown={handleAboutKeydown}
      >
        <div
          class="about-card"
          role="dialog"
          tabindex="-1"
          aria-modal="true"
          aria-label="About OpenQuota"
        >
          <button
            bind:this={aboutCloseButton}
            class="about-card__close"
            type="button"
            aria-label="Close About"
            onclick={() => void closeAbout()}
            ><Icon name="close" size={11} strokeWidth={2.3} /></button
          >
          <OpenQuotaMark size={44} />
          <h1>OpenQuota</h1>
          <p>Version {appVersion}</p>
          <small>Private, local usage monitoring for your AI coding tools.</small>
        </div>
      </div>
    {/if}
  {:else}
    <div class="content">
      {#if settingsError}
        <div class="notice notice--blocking" role="alert">{settingsError}</div>
      {:else}
        <p class="empty-row">Loading OpenQuota…</p>
      {/if}
    </div>
  {/if}
  {#if renderedResizeEdge === 'bottom'}
    <div
      class="panel-resize-dragger panel-resize-dragger--bottom"
      role="separator"
      aria-label="Resize panel height"
      aria-orientation="horizontal"
      onpointerdown={handlePanelResizePointerDown}
    ></div>
  {/if}
</main>

<style>
  :global {
    .popover {
      position: relative;
      display: flex;
      width: 100%;
      height: 100%;
      flex-direction: column;
      overflow: hidden;
      color: var(--text);
      background: var(--surface);
      isolation: isolate;
      user-select: none;
    }

    /* Copool's blurred desktop snapshot and material tint. */
    :root.frosted .popover {
      background:
        linear-gradient(var(--glass-tint), var(--glass-tint)),
        var(--backdrop-image) var(--backdrop-position) / var(--backdrop-size) no-repeat,
        var(--tray);
      box-shadow: inset 0 0 0 0.5px var(--glass-rim);
    }

    .floating-chrome {
      position: relative;
      z-index: 30;
      display: grid;
      width: 100%;
      height: 32px;
      flex: 0 0 32px;
      grid-template-columns: 32px 1fr 32px;
      align-items: center;
      border-bottom: 1px solid var(--separator);
      background: color-mix(in srgb, var(--text) 3%, var(--surface));
    }

    .floating-chrome__drag {
      grid-row: 1;
      grid-column: 1 / -1;
      display: flex;
      height: 100%;
      align-items: center;
      justify-content: center;
      gap: 6px;
      color: var(--secondary);
      cursor: grab;
      font-size: 11px;
      font-weight: 600;
      letter-spacing: 0.01em;
      touch-action: none;
    }

    .floating-chrome__drag:active {
      cursor: grabbing;
    }

    .floating-chrome__drag > * {
      pointer-events: none;
    }

    .floating-chrome__close {
      position: relative;
      z-index: 1;
      display: grid;
      width: 24px;
      height: 24px;
      grid-row: 1;
      grid-column: 3;
      align-items: center;
      justify-self: center;
      padding: 0;
      border: 0;
      border-radius: 7px;
      color: var(--secondary);
      background: transparent;
      cursor: default;
      place-items: center;
      transition:
        color 120ms ease,
        background-color 120ms ease,
        transform 80ms ease;
    }

    :root[data-platform='macos'] .popover {
      border-radius: 12px;
    }

    .popover--macos .floating-chrome__close {
      grid-column: 1;
    }

    .floating-chrome__close:hover {
      color: var(--text);
      background: color-mix(in srgb, var(--text) 9%, transparent);
    }

    .floating-chrome__close:active {
      transform: scale(0.92);
    }

    .floating-chrome__close:focus-visible {
      outline: 2px solid color-mix(in srgb, var(--meter-fill) 55%, transparent);
      outline-offset: -1px;
    }

    .panel-resize-dragger {
      position: relative;
      z-index: 20;
      display: grid;
      width: 100%;
      height: 10px;
      flex: 0 0 10px;
      cursor: ns-resize;
      touch-action: none;
      place-items: center;
    }

    .panel-resize-dragger::after {
      width: 36px;
      height: 4px;
      border-radius: 999px;
      background: var(--separator);
      content: '';
      transition:
        width 120ms ease,
        background-color 120ms ease;
    }

    .panel-resize-dragger:hover::after {
      width: 42px;
      background: var(--tertiary);
    }

    .panel-resize-dragger--top {
      box-shadow: 0 10px 18px -20px rgba(0, 0, 0, 0.7);
    }

    .panel-resize-dragger--bottom {
      box-shadow: 0 -10px 18px -20px rgba(0, 0, 0, 0.7);
    }

    .content {
      flex: 1;
      min-height: 0;
      padding: 14px 14px 12px;
      overflow-y: auto;
      scrollbar-width: none;
      overflow-x: hidden;
    }

    .content::-webkit-scrollbar {
      width: 0;
      height: 0;
    }

    .screen-header {
      display: grid;
      min-height: 30px;
      align-items: center;
      grid-template-columns: 54px 1fr 54px;
      margin-bottom: 8px;
    }

    .screen-header h1 {
      margin: 0;
      font-size: 14px;
      text-align: center;
    }

    .screen-header button {
      width: fit-content;
      padding: 3px 7px;
      border: 0;
      border-radius: 6px;
      color: var(--secondary);
      background: transparent;
      cursor: pointer;
    }

    .screen-header > button:first-child {
      font-size: 23px;
      line-height: 20px;
    }

    .screen-header .text-button {
      justify-self: end;
      color: var(--meter-fill);
      font-size: 10px;
    }

    .screen-header button:hover {
      background: var(--button-hover);
    }

    :root[data-density='compact'] .content {
      padding: 9px 11px 7px;
    }

    .content {
      padding: 14px 14px 12px;
      scrollbar-width: none;
    }

    .content--chrome {
      padding-top: 12px;
    }

    .screen-stage {
      position: relative;
      display: grid;
      width: 100%;
      min-width: 0;
      min-height: 0;
      overflow: clip;
      background: var(--surface);
    }

    .screen-page {
      width: 100%;
      min-width: 0;
      min-height: 0;
      grid-area: 1 / 1;
      align-self: start;
      transform-origin: 50% 45%;
    }

    /* Svelte marks the outgoing page inert during its slide. Only the incoming
       page should determine how far the content can scroll. */
    .screen-page[inert] {
      position: absolute;
      inset: 0 0 auto;
    }

    .screen-header {
      position: sticky;
      top: 0;
      z-index: 5;
      min-height: 44px;
      grid-template-columns: 44px 1fr 44px;
      margin: 0 -14px 12px;
      padding: 0 14px;
      background: var(--surface-veil);
      box-shadow: 0 10px 18px -20px rgba(0, 0, 0, 0.8);
      backdrop-filter: blur(18px);
    }

    .app-top-bar {
      position: relative;
      top: auto;
      z-index: 10;
      width: 100%;
      min-height: 44px;
      flex: 0 0 44px;
      margin: 0;
      padding: 0 14px;
      background: var(--surface);
    }

    .screen-header h1 {
      font-size: 13px;
      font-weight: 600;
    }

    .screen-header button:first-child {
      display: grid;
      width: 28px;
      height: 28px;
      padding: 0;
      border-radius: 50%;
      background: var(--button-hover);
      place-items: center;
    }

    .screen-header .text-button {
      width: 28px;
      height: 28px;
      overflow: hidden;
      color: var(--secondary);
      font-size: inherit;
    }

    .screen-header .text-button::after {
      content: none;
    }

    .transient-pill {
      position: absolute;
      right: 14px;
      bottom: 62px;
      z-index: 90;
      display: flex;
      align-items: center;
      gap: 6px;
      padding: 7px 10px;
      border: 1px solid var(--separator);
      border-radius: 999px;
      color: var(--text);
      background: color-mix(in srgb, var(--tray) 96%, transparent);
      box-shadow: 0 8px 24px rgba(0, 0, 0, 0.22);
      font-size: 10px;
      animation: detail-in var(--motion-spring) both;
    }

    .transient-pill .symbol-icon {
      color: #34c759;
    }

    .about-backdrop {
      position: absolute;
      z-index: 100;
      display: grid;
      border: 0;
      background: rgba(0, 0, 0, 0.28);
      inset: 0;
      place-items: center;
      backdrop-filter: blur(6px);
    }

    .about-card {
      position: relative;
      display: flex;
      width: 230px;
      align-items: center;
      padding: 24px 20px 20px;
      border: 1px solid var(--separator);
      border-radius: 16px;
      color: var(--text);
      background: var(--tray);
      box-shadow: 0 18px 55px rgba(0, 0, 0, 0.35);
      flex-direction: column;
      animation: detail-in var(--motion-spring) both;
    }

    .about-card h1 {
      margin: 10px 0 2px;
      font-size: 17px;
    }

    .about-card p,
    .about-card small {
      margin: 0;
      color: var(--secondary);
      font-size: 10px;
      text-align: center;
    }

    .about-card__close {
      position: absolute;
      top: 8px;
      right: 8px;
      display: grid;
      width: 24px;
      height: 24px;
      padding: 0;
      border: 0;
      border-radius: 50%;
      color: var(--secondary);
      background: var(--button-hover);
      cursor: pointer;
      place-items: center;
      transition:
        color var(--motion-switch),
        background var(--motion-switch),
        transform var(--motion-switch);
    }

    .about-card__close:hover {
      color: var(--text);
      background: color-mix(in srgb, var(--text) 14%, transparent);
    }

    .about-card__close:active {
      transform: scale(0.92);
    }

    .about-card__close:focus-visible {
      outline: 2px solid color-mix(in srgb, var(--meter-fill) 55%, transparent);
      outline-offset: 1px;
    }

    :root[data-density='compact'] .content {
      padding: 10px 14px 8px;
    }

    :root[data-density='compact'] .content--chrome {
      padding-top: 12px;
    }

    .notice--blocking {
      color: var(--error);
      background: var(--error-bg);
    }

    .popover {
      width: 100%;
      min-width: 0;
      max-width: 320px;
    }
  }
</style>
