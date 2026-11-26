import { cleanup, fireEvent, render } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import FloatingTooltip from './FloatingTooltip.svelte';

function mockRect(element: HTMLElement, rect: Partial<DOMRect>) {
  const full = {
    x: rect.left ?? 0,
    y: rect.top ?? 0,
    left: rect.left ?? 0,
    top: rect.top ?? 0,
    right: rect.right ?? (rect.left ?? 0) + (rect.width ?? 0),
    bottom: rect.bottom ?? (rect.top ?? 0) + (rect.height ?? 0),
    width: rect.width ?? 0,
    height: rect.height ?? 0,
    toJSON: () => ({}),
  } as DOMRect;
  element.getBoundingClientRect = vi.fn(() => full);
}

function mockTooltipSize(width: number, height: number) {
  Object.defineProperty(HTMLElement.prototype, 'offsetWidth', {
    configurable: true,
    get() {
      return this.classList.contains('floating-tooltip') ? width : 0;
    },
  });
  Object.defineProperty(HTMLElement.prototype, 'offsetHeight', {
    configurable: true,
    get() {
      return this.classList.contains('floating-tooltip') ? height : 0;
    },
  });
}

describe('FloatingTooltip', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    Object.defineProperty(window, 'innerWidth', { configurable: true, value: 320 });
    Object.defineProperty(window, 'innerHeight', { configurable: true, value: 360 });
    mockTooltipSize(160, 36);
  });

  afterEach(() => {
    cleanup();
    document.body.innerHTML = '';
    vi.useRealTimers();
    vi.restoreAllMocks();
  });

  it('shows below the trigger when space permits and keeps left alignment for usage warnings', async () => {
    render(FloatingTooltip);
    const trigger = document.createElement('i');
    trigger.className = 'usage-label-warning';
    trigger.dataset.tooltip = 'Unknown model found\n- Unattributed';
    mockRect(trigger, { left: 90, top: 80, width: 10, height: 10 });
    document.body.appendChild(trigger);

    await fireEvent.mouseOver(trigger);
    expect(document.querySelector('.floating-tooltip')).toBeNull();

    await vi.advanceTimersByTimeAsync(400);
    const tooltip = document.querySelector<HTMLElement>('.floating-tooltip')!;
    expect(tooltip).toBeInTheDocument();
    expect(tooltip).toHaveClass('floating-tooltip--visible');
    expect(tooltip).not.toHaveClass('floating-tooltip--top');
    expect(tooltip).toHaveAttribute('aria-hidden', 'true');
    expect(tooltip).not.toHaveAttribute('role');
    expect(tooltip.textContent).toBe('Unknown model found\n- Unattributed');
    expect(tooltip.style.top).toBe('97px');
    expect(tooltip.style.left).toBe('90px');
  });

  it('flips above the trigger when near the bottom edge so the last row tooltip is never clipped', async () => {
    render(FloatingTooltip);
    const trigger = document.createElement('i');
    trigger.className = 'usage-label-warning';
    trigger.dataset.tooltip = 'Unknown model found\n- Unattributed';
    mockRect(trigger, { left: 104, top: 332, width: 10, height: 10 });
    document.body.appendChild(trigger);

    await fireEvent.mouseOver(trigger);
    await vi.advanceTimersByTimeAsync(400);

    const tooltip = document.querySelector<HTMLElement>('.floating-tooltip')!;
    expect(tooltip).toHaveClass('floating-tooltip--visible');
    expect(tooltip).toHaveClass('floating-tooltip--top');
    // 332 - 7 (gap) - 36 (height) = 289
    expect(tooltip.style.top).toBe('289px');
    expect(tooltip.style.left).toBe('104px');
  });

  it('clamps horizontally inside the popover width and hides on scroll or Escape', async () => {
    render(FloatingTooltip);
    const trigger = document.createElement('span');
    trigger.className = 'usage-label-warning';
    trigger.dataset.tooltip = 'Long warning text';
    mockRect(trigger, { left: 260, top: 60, width: 10, height: 10 });
    document.body.appendChild(trigger);

    await fireEvent.mouseOver(trigger);
    await vi.advanceTimersByTimeAsync(400);

    const tooltip = document.querySelector<HTMLElement>('.floating-tooltip')!;
    // 320 - 8 (margin) - 160 (width) = 152
    expect(tooltip.style.left).toBe('152px');

    await fireEvent.keyDown(document, { key: 'Escape' });
    expect(document.querySelector('.floating-tooltip')).toBeNull();
  });

  it('hides after the hover leave delay', async () => {
    render(FloatingTooltip);
    const trigger = document.createElement('button');
    trigger.dataset.tooltip = 'Exact figure';
    mockRect(trigger, { left: 120, top: 60, width: 40, height: 16 });
    document.body.appendChild(trigger);

    await fireEvent.mouseOver(trigger);
    await vi.advanceTimersByTimeAsync(400);
    expect(document.querySelector('.floating-tooltip')).toHaveClass('floating-tooltip--visible');

    await fireEvent.mouseOut(trigger, { relatedTarget: document.body });
    await vi.advanceTimersByTimeAsync(180);
    expect(document.querySelector('.floating-tooltip')).not.toHaveClass(
      'floating-tooltip--visible',
    );

    await vi.advanceTimersByTimeAsync(120);
    expect(document.querySelector('.floating-tooltip')).toBeNull();
  });

  it('restarts the show delay when the pointer returns before a pending hide', async () => {
    render(FloatingTooltip);
    const trigger = document.createElement('span');
    trigger.dataset.tooltip = 'Price unavailable';
    document.body.appendChild(trigger);

    await fireEvent.mouseOver(trigger);
    await vi.advanceTimersByTimeAsync(200);
    await fireEvent.mouseOut(trigger, { relatedTarget: document.body });
    await vi.advanceTimersByTimeAsync(50);
    await fireEvent.mouseOver(trigger);

    await vi.advanceTimersByTimeAsync(399);
    expect(document.querySelector('.floating-tooltip--visible')).toBeNull();
    await vi.advanceTimersByTimeAsync(1);
    expect(document.querySelector('.floating-tooltip')).toHaveClass('floating-tooltip--visible');
  });

  it('keeps the original show delay when moving between children of the same trigger', async () => {
    render(FloatingTooltip);
    const trigger = document.createElement('button');
    trigger.dataset.tooltip = 'Exact figure';
    const child = document.createElement('span');
    trigger.appendChild(child);
    document.body.appendChild(trigger);

    await fireEvent.mouseOver(trigger);
    await vi.advanceTimersByTimeAsync(200);
    await fireEvent.mouseOut(trigger, { relatedTarget: child });
    await fireEvent.mouseOver(child, { relatedTarget: trigger });
    await vi.advanceTimersByTimeAsync(200);
    expect(document.querySelector('.floating-tooltip')).toHaveClass('floating-tooltip--visible');
  });
});
