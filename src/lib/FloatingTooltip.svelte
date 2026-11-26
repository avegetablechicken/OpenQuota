<script lang="ts">
  import { onMount, tick } from 'svelte';

  const SHOW_DELAY_MS = 400;
  const HIDE_DELAY_MS = 180;
  const FADE_OUT_MS = 120;
  const VIEWPORT_MARGIN = 8;
  const ANCHOR_GAP = 7;

  let text = $state<string | null>(null);
  let visible = $state(false);
  let top = $state(0);
  let left = $state(0);
  let placement = $state<'bottom' | 'top'>('bottom');
  let tooltipEl = $state<HTMLDivElement | null>(null);

  let activeTarget: HTMLElement | null = null;
  let showTimer: ReturnType<typeof setTimeout> | undefined;
  let hideTimer: ReturnType<typeof setTimeout> | undefined;
  let unmountTimer: ReturnType<typeof setTimeout> | undefined;
  let renderToken = 0;

  function findTooltipTarget(node: EventTarget | null): HTMLElement | null {
    let current: Element | null = node instanceof Element ? node : null;
    while (current) {
      if (current instanceof HTMLElement) {
        const value = current.getAttribute('data-tooltip');
        if (value && value.trim().length > 0) return current;
      }
      current = current.parentElement;
    }
    return null;
  }

  function isFocusVisible(element: HTMLElement): boolean {
    try {
      return element.matches(':focus-visible');
    } catch {
      return false;
    }
  }

  function clearShowTimer() {
    if (showTimer !== undefined) {
      clearTimeout(showTimer);
      showTimer = undefined;
    }
  }

  function clearHideTimer() {
    if (hideTimer !== undefined) {
      clearTimeout(hideTimer);
      hideTimer = undefined;
    }
  }

  function clearUnmountTimer() {
    if (unmountTimer !== undefined) {
      clearTimeout(unmountTimer);
      unmountTimer = undefined;
    }
  }

  function hideNow() {
    renderToken += 1;
    clearShowTimer();
    clearHideTimer();
    clearUnmountTimer();
    activeTarget = null;
    visible = false;
    text = null;
  }

  function scheduleHide() {
    clearShowTimer();
    clearHideTimer();
    hideTimer = setTimeout(() => {
      hideTimer = undefined;
      renderToken += 1;
      activeTarget = null;
      visible = false;
      clearUnmountTimer();
      unmountTimer = setTimeout(() => {
        unmountTimer = undefined;
        if (!visible) text = null;
      }, FADE_OUT_MS);
    }, HIDE_DELAY_MS);
  }

  function positionFor(target: HTMLElement, element: HTMLDivElement) {
    const anchor = target.getBoundingClientRect();
    const tipWidth = element.offsetWidth;
    const tipHeight = element.offsetHeight;
    const viewportWidth = window.innerWidth;
    const viewportHeight = window.innerHeight;

    let rawLeft = anchor.left + anchor.width / 2 - tipWidth / 2;
    if (target.matches('.provider-warning, .screen-header > :last-child')) {
      rawLeft = anchor.right - tipWidth;
    } else if (
      target.matches('.total-card__info, .usage-label-warning, .screen-header > :first-child')
    ) {
      rawLeft = anchor.left;
    }
    const maxLeft = Math.max(VIEWPORT_MARGIN, viewportWidth - VIEWPORT_MARGIN - tipWidth);
    left = Math.round(Math.max(VIEWPORT_MARGIN, Math.min(rawLeft, maxLeft)));

    const belowTop = anchor.bottom + ANCHOR_GAP;
    const aboveTop = anchor.top - ANCHOR_GAP - tipHeight;
    const fitsBelow = belowTop + tipHeight <= viewportHeight - VIEWPORT_MARGIN;
    const fitsAbove = aboveTop >= VIEWPORT_MARGIN;
    const maxTop = Math.max(VIEWPORT_MARGIN, viewportHeight - VIEWPORT_MARGIN - tipHeight);

    if (fitsBelow || (!fitsAbove && viewportHeight - anchor.bottom >= anchor.top)) {
      placement = 'bottom';
      top = Math.round(Math.max(VIEWPORT_MARGIN, Math.min(belowTop, maxTop)));
    } else {
      placement = 'top';
      top = Math.round(Math.max(VIEWPORT_MARGIN, Math.min(aboveTop, maxTop)));
    }
  }

  async function reveal(target: HTMLElement) {
    if (activeTarget !== target || !target.isConnected) return;
    const value = target.getAttribute('data-tooltip')?.trim();
    if (!value) {
      hideNow();
      return;
    }
    const token = ++renderToken;
    clearUnmountTimer();
    text = value;
    await tick();
    if (token !== renderToken || activeTarget !== target || !target.isConnected || !tooltipEl) {
      return;
    }
    positionFor(target, tooltipEl);
    visible = true;
  }

  function scheduleShow(target: HTMLElement) {
    if (document.documentElement.classList.contains('is-reordering')) return;
    clearHideTimer();
    clearUnmountTimer();
    if (activeTarget === target && (visible || showTimer !== undefined)) {
      if (visible && tooltipEl) positionFor(target, tooltipEl);
      return;
    }
    renderToken += 1;
    clearShowTimer();
    visible = false;
    activeTarget = target;
    showTimer = setTimeout(() => {
      showTimer = undefined;
      void reveal(target);
    }, SHOW_DELAY_MS);
  }

  onMount(() => {
    const onMouseOver = (event: MouseEvent) => {
      const target = findTooltipTarget(event.target);
      if (!target) {
        if (activeTarget) scheduleHide();
        return;
      }
      scheduleShow(target);
    };

    const onMouseOut = (event: MouseEvent) => {
      if (!activeTarget) return;
      if (event.relatedTarget instanceof Node && activeTarget.contains(event.relatedTarget)) {
        return;
      }
      if (findTooltipTarget(event.relatedTarget) === activeTarget) {
        return;
      }
      scheduleHide();
    };

    const onFocusIn = (event: FocusEvent) => {
      const target = findTooltipTarget(event.target);
      if (!target || !isFocusVisible(target)) return;
      scheduleShow(target);
    };

    const onFocusOut = (event: FocusEvent) => {
      if (!activeTarget) return;
      if (event.relatedTarget instanceof Node && activeTarget.contains(event.relatedTarget)) {
        return;
      }
      hideNow();
    };

    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') hideNow();
    };

    document.addEventListener('mouseover', onMouseOver);
    document.addEventListener('mouseout', onMouseOut);
    document.addEventListener('focusin', onFocusIn);
    document.addEventListener('focusout', onFocusOut);
    document.addEventListener('pointerdown', hideNow);
    document.addEventListener('keydown', onKeyDown);
    window.addEventListener('scroll', hideNow, { capture: true, passive: true });
    window.addEventListener('resize', hideNow);
    window.addEventListener('blur', hideNow);

    return () => {
      hideNow();
      document.removeEventListener('mouseover', onMouseOver);
      document.removeEventListener('mouseout', onMouseOut);
      document.removeEventListener('focusin', onFocusIn);
      document.removeEventListener('focusout', onFocusOut);
      document.removeEventListener('pointerdown', hideNow);
      document.removeEventListener('keydown', onKeyDown);
      window.removeEventListener('scroll', hideNow, { capture: true });
      window.removeEventListener('resize', hideNow);
      window.removeEventListener('blur', hideNow);
    };
  });
</script>

{#if text !== null}
  <div
    bind:this={tooltipEl}
    class="floating-tooltip"
    class:floating-tooltip--top={placement === 'top'}
    class:floating-tooltip--visible={visible}
    style={`top:${top}px;left:${left}px`}
    aria-hidden="true"
  >
    {text}
  </div>
{/if}

<style>
  :global {
    .floating-tooltip {
      position: fixed;
      z-index: 110;
      width: max-content;
      max-width: min(190px, calc(100vw - 24px));
      padding: 5px 7px;
      border: 1px solid var(--separator);
      border-radius: 6px;
      color: var(--text);
      background: var(--tray);
      box-shadow: 0 5px 18px rgba(0, 0, 0, 0.24);
      font-size: 10px;
      font-weight: 400;
      line-height: 13px;
      white-space: pre-line;
      overflow-wrap: anywhere;
      opacity: 0;
      pointer-events: none;
      transform: translateY(-2px) scale(0.97);
      transition:
        opacity 120ms ease,
        transform 120ms ease;
    }

    .floating-tooltip--top {
      transform: translateY(2px) scale(0.97);
    }

    .floating-tooltip--visible {
      opacity: 1;
      transform: translateY(0) scale(1);
    }
  }
</style>
