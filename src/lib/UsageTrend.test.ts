import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import UsageTrend from './UsageTrend.svelte';

function localDate(offset: number) {
  const date = new Date();
  date.setDate(date.getDate() - offset);
  return `${date.getFullYear()}-${String(date.getMonth() + 1).padStart(2, '0')}-${String(date.getDate()).padStart(2, '0')}`;
}

describe('UsageTrend', () => {
  beforeEach(() => vi.useFakeTimers());

  afterEach(() => {
    cleanup();
    vi.useRealTimers();
  });

  it('forgets the hovered day when scrolling closes the detail', async () => {
    render(UsageTrend, {
      label: 'Usage Trend',
      sourceNote: 'Local logs',
      daily: [
        { date: localDate(0), tokens: 1_000, estimatedCostUsd: null, estimateComplete: true },
        { date: localDate(1), tokens: 5_000, estimatedCostUsd: null, estimateComplete: true },
      ],
    });
    const chart = screen.getByRole('group', { name: 'Usage trend chart details' });
    const openDetail = async () => {
      await fireEvent.mouseEnter(chart);
      await vi.advanceTimersByTimeAsync(400);
      return document.querySelector<HTMLElement>('.trend-detail')!;
    };

    let detail = await openDetail();
    expect(detail.querySelector('header span')).toHaveTextContent('peak');
    const bars = detail.querySelectorAll('.trend-detail__bars i');
    await fireEvent.mouseEnter(bars[bars.length - 1]);
    expect(detail.querySelector('header span')).not.toHaveTextContent('peak');

    await fireEvent.scroll(window);
    expect(document.querySelector('.trend-detail')).toBeNull();

    detail = await openDetail();
    expect(detail.querySelector('header span')).toHaveTextContent('peak');
    expect(detail.querySelectorAll('.trend-detail__bars i.muted')).toHaveLength(0);
  });
});
