import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import QuotaMetric from './QuotaMetric.svelte';
import type { QuotaWindow } from './types';

const now = Date.parse('2026-07-10T12:00:00Z');
const periodSeconds = 10_000;

afterEach(cleanup);

function quota(usedPercent: number, elapsedFraction = 0.5): QuotaWindow {
  return {
    id: 'weekly',
    label: 'Weekly',
    usedPercent,
    format: 'percent',
    usedValue: null,
    limitValue: null,
    estimated: false,
    periodSeconds,
    resetsAt: new Date(now + (1 - elapsedFraction) * periodSeconds * 1000).toISOString(),
  };
}

function show(value: QuotaWindow, onToggleReset = vi.fn(), usageDisplay: 'used' | 'left' = 'left') {
  return {
    onToggleReset,
    ...render(QuotaMetric, {
      quota: value,
      now,
      usageDisplay,
      resetDisplay: 'countdown',
      timeFormat: 'system',
      alwaysShowPacing: false,
      onToggleUsage: vi.fn(),
      onToggleReset,
    }),
  };
}

function showAlways(value: QuotaWindow) {
  return render(QuotaMetric, {
    quota: value,
    now,
    usageDisplay: 'left',
    resetDisplay: 'countdown',
    timeFormat: 'system',
    alwaysShowPacing: true,
    onToggleUsage: vi.fn(),
    onToggleReset: vi.fn(),
  });
}

describe('quota pacing presentation', () => {
  it('shows the flame, run-out time, and projection tooltip', async () => {
    const onToggleReset = vi.fn();
    const { container } = show(quota(60), onToggleReset);
    const warning = screen.getByRole('button', { name: 'Limit in 56m' });
    expect(container.querySelector('.pace-warning__icon')).toBeInTheDocument();
    expect(warning).toHaveAttribute('data-tooltip', '~20% over limit at reset');
    expect(container.querySelector('.meter-shell')).toHaveAttribute(
      'data-tooltip',
      '~20% over limit at reset',
    );
    expect(container.querySelector('.meter__fill')).toHaveStyle('--fill-percent: 40%');
    expect(screen.getByRole('button', { name: '40% left' })).toHaveAttribute(
      'data-tooltip',
      '60% used',
    );
    expect(screen.getByRole('button', { name: /Resets in/ })).toHaveAttribute(
      'data-tooltip',
      expect.stringContaining('Resets today at'),
    );
    await fireEvent.click(warning);
    expect(onToggleReset).toHaveBeenCalledOnce();
  });

  it('shows a flame without a misleading time at the exact-limit edge', () => {
    const { container } = show(quota(50));
    expect(screen.getByLabelText('Will reach limit')).toBeInTheDocument();
    expect(screen.queryByText(/Limit in/)).not.toBeInTheDocument();
    expect(container.querySelector('.pace-warning')).toHaveAttribute(
      'data-tooltip',
      '~100% used at reset',
    );
  });

  it('shows the rounded spare copy and projected-use tooltip when close', () => {
    show(quota(46));
    expect(screen.getByText('~8% spare')).toHaveAttribute('data-tooltip', '~92% used at reset');
  });

  it('frames the even-pace tick in the selected remaining mode', () => {
    const { container } = show(quota(30, 0.25));
    expect(container.querySelector('.meter__pace')).toHaveStyle('--pace-percent: 75%');
  });

  it.each(['used', 'left'] as const)('renders available zero usage in %s mode', (mode) => {
    const { container } = show(quota(0), vi.fn(), mode);
    expect(
      screen.getByRole('button', { name: mode === 'used' ? '0% used' : '100% left' }),
    ).toBeInTheDocument();
    expect(screen.queryByText('No data')).not.toBeInTheDocument();
    expect(container.querySelector('.pace-warning')).not.toBeInTheDocument();
    expect(container.querySelector('.meter-shell')).not.toHaveAttribute('data-tooltip');
  });

  it('keeps zero usage visible for a non-session window', () => {
    show(quota(0), vi.fn(), 'used');
    expect(screen.getByRole('button', { name: '0% used' })).toBeInTheDocument();
    expect(screen.queryByText('No data')).not.toBeInTheDocument();
  });

  it('does not decorate unused non-session quotas as healthy pacing', () => {
    const { container } = showAlways(quota(0));
    expect(screen.queryByText(/left at reset/)).not.toBeInTheDocument();
    expect(container.querySelector('.meter__pace')).not.toBeInTheDocument();
    expect(container.querySelector('.meter-shell')).not.toHaveAttribute('data-tooltip');
  });

  it('renders provider-supplied count units instead of a hardcoded request label', () => {
    show({
      ...quota(24),
      id: 'requests',
      label: 'Requests',
      format: 'count',
      usedValue: 120,
      limitValue: 500,
      unit: 'searches',
    });
    expect(screen.getByRole('button', { name: '380 searches left' })).toHaveAttribute(
      'data-tooltip',
      '120 searches used',
    );
  });

  it('marks inferred quotas with their source note', () => {
    show({
      ...quota(24),
      estimated: true,
      sourceNote: 'Estimated from local records.',
    });

    expect(screen.getByLabelText('Estimated quota')).toHaveAttribute(
      'data-tooltip',
      'Estimated from local records.',
    );
  });
});
