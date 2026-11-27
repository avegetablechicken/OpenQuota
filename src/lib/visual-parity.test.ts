import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import ProviderIcon from './ProviderIcon.svelte';
import { sub2ApiSnapshotContext } from './sub2ApiUpstreams';
import { clearSub2ApiTrack, setSub2ApiTrack } from '../test/sub2ApiTracks';
import UsageTrend from './UsageTrend.svelte';

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe('native visual contract', () => {
  it.each([
    'claude',
    'codex',
    'cursor',
    'antigravity',
    'copilot',
    'devin',
    'grok',
    'opencode',
    'openrouter',
    'sub2api',
    'zai',
  ])('packages the exact %s provider icon', (providerId) => {
    const { container } = render(ProviderIcon, { providerId });
    const icon = container.querySelector('.provider-icon');
    expect(icon).not.toBeNull();
    const path = icon?.querySelector('path')?.getAttribute('d');
    expect(path?.length).toBeGreaterThan(10);
  });

  it('renders provider marks with their intended brand treatment', () => {
    const claude = render(ProviderIcon, { providerId: 'claude' });
    expect(claude.container.querySelector('path')).toHaveAttribute('fill', '#DE7356');
    cleanup();
    const codex = render(ProviderIcon, { providerId: 'codex' });
    expect(codex.container.querySelector('path')).toHaveAttribute('fill', 'currentColor');
    cleanup();
    const copilot = render(ProviderIcon, { providerId: 'copilot' });
    expect(copilot.container.querySelector('path')).toHaveAttribute('fill', 'currentColor');
    cleanup();
    const cursor = render(ProviderIcon, { providerId: 'cursor' });
    expect(cursor.container.querySelector('path')).toHaveAttribute('fill', 'currentColor');
    cleanup();
    const devin = render(ProviderIcon, { providerId: 'devin' });
    expect(devin.container.querySelector('path')).toHaveAttribute('fill', 'currentColor');
    cleanup();
    const antigravity = render(ProviderIcon, { providerId: 'antigravity' });
    expect(antigravity.container.querySelector('path')).toHaveAttribute('fill', '#4285F4');
    cleanup();
    const grok = render(ProviderIcon, { providerId: 'grok' });
    expect(grok.container.querySelector('path')).toHaveAttribute('fill', 'currentColor');
    cleanup();
    const opencode = render(ProviderIcon, { providerId: 'opencode' });
    expect(opencode.container.querySelector('path')).toHaveAttribute('fill', 'currentColor');
    cleanup();
    const openrouter = render(ProviderIcon, { providerId: 'openrouter' });
    expect(openrouter.container.querySelector('path')).toHaveAttribute('fill', 'currentColor');
    cleanup();
    const zai = render(ProviderIcon, { providerId: 'zai' });
    expect(zai.container.querySelector('path')).toHaveAttribute('fill', 'currentColor');
  });

  it('reuses the Claude mark for Claude account cards', () => {
    const claude = render(ProviderIcon, { providerId: 'claude' });
    const account = render(ProviderIcon, { providerId: 'claude@1234abcd' });

    expect(account.container.innerHTML).toBe(claude.container.innerHTML);
  });

  it('shows the original Sub2API mark before an upstream is configured', () => {
    const { container } = render(ProviderIcon, { providerId: 'sub2api' });
    expect(container.querySelectorAll('path')).toHaveLength(1);
    expect(container.querySelector('circle')).toBeNull();
    expect(container.querySelector('[data-icon-layer="upstream"]')).toBeNull();
    expect(container.querySelector('path')).toHaveAttribute('stroke', '#39D9E7');
  });

  it('can compose the Sub2API mark with its Claude upstream mark', () => {
    const { container } = render(ProviderIcon, {
      providerId: 'sub2api',
      upstreamProvider: 'claude',
    });
    expect(container.querySelectorAll('path')).toHaveLength(2);
    expect(container.querySelector('path[fill="#DE7356"]')).not.toBeNull();
    expect(container.querySelector('[data-icon-layer="sub2api"] path')).toHaveAttribute(
      'stroke',
      '#39D9E7',
    );
  });

  it.each(['codex', 'claude', 'grok'])(
    'draws the detected %s mark for a user subscription',
    (upstreamProviderId) => {
      const icon = render(ProviderIcon, {
        props: { providerId: 'sub2api@2', upstreamProvider: 'subscription' },
        context: new Map([[sub2ApiSnapshotContext, () => ({ upstreamProviderId })]]),
      });
      const reference = render(ProviderIcon, { providerId: upstreamProviderId });
      expect(icon.container.querySelector('[data-icon-layer="sub2api"] path')).toBeInTheDocument();
      expect(icon.container.querySelector('[data-icon-layer="upstream"] path')).toHaveAttribute(
        'd',
        reference.container.querySelector('path')!.getAttribute('d'),
      );
      expect(icon.container.querySelector('[data-icon-layer="upstream"]')).toHaveAttribute(
        'viewBox',
        reference.container.querySelector('svg')!.getAttribute('viewBox'),
      );
    },
  );

  it('updates a Sub2API item icon from its remembered upstream', () => {
    setSub2ApiTrack('sub2api@2', 'claude');
    const { container } = render(ProviderIcon, { providerId: 'sub2api@2' });
    expect(container.querySelector('path[fill="#DE7356"]')).not.toBeNull();
    clearSub2ApiTrack('sub2api@2');
  });

  it('uses the shared hover dwell and grace timing for Usage Trend details', async () => {
    vi.useFakeTimers();
    const today = new Date();
    const date = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, '0')}-${String(today.getDate()).padStart(2, '0')}`;
    render(UsageTrend, {
      label: 'Usage Trend',
      daily: [{ date, tokens: 42_000, estimatedCostUsd: 0.21, estimateComplete: true }],
      sourceNote: 'From your Codex logs (estimated)',
    });
    const chart = screen.getByRole('group', { name: 'Usage trend chart details' });

    await fireEvent.mouseEnter(chart);
    await vi.advanceTimersByTimeAsync(399);
    expect(screen.queryByText('peak 42K tokens')).not.toBeInTheDocument();
    await vi.advanceTimersByTimeAsync(1);
    expect(screen.getByText('peak 42K tokens')).toBeInTheDocument();
    expect(screen.getByText('From your Codex logs (estimated)')).toBeInTheDocument();

    await fireEvent.mouseLeave(chart);
    await vi.advanceTimersByTimeAsync(179);
    expect(screen.getByText('peak 42K tokens')).toBeInTheDocument();
    await vi.advanceTimersByTimeAsync(1);
    expect(screen.queryByText('peak 42K tokens')).not.toBeInTheDocument();
  });

  it('reveals an exact day value when a detail bar is hovered', async () => {
    vi.useFakeTimers();
    const today = new Date();
    const date = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, '0')}-${String(today.getDate()).padStart(2, '0')}`;
    const { container } = render(UsageTrend, {
      label: 'Usage Trend',
      daily: [{ date, tokens: 42_000, estimatedCostUsd: 0.21, estimateComplete: true }],
      sourceNote: 'From your Codex logs (estimated)',
    });
    await fireEvent.mouseEnter(screen.getByRole('group', { name: 'Usage trend chart details' }));
    await vi.advanceTimersByTimeAsync(400);
    const bars = container.querySelectorAll<HTMLElement>('.trend-detail__bars i');
    await fireEvent.mouseEnter(bars[bars.length - 1]);
    expect(screen.getByText(/· 42K tokens$/)).toBeInTheDocument();
    expect(container.querySelectorAll('.trend-detail__bars i.muted')).toHaveLength(30);
  });

  it('flips Usage Trend details below the row when there is not enough room above', async () => {
    vi.useFakeTimers();
    Object.defineProperty(window, 'innerHeight', { configurable: true, value: 400 });
    const today = new Date();
    const date = `${today.getFullYear()}-${String(today.getMonth() + 1).padStart(2, '0')}-${String(today.getDate()).padStart(2, '0')}`;
    const { container } = render(UsageTrend, {
      label: 'Usage Trend',
      daily: [{ date, tokens: 42_000, estimatedCostUsd: 0.21, estimateComplete: true }],
      sourceNote: 'From your Codex logs (estimated)',
    });
    const chart = screen.getByRole('group', { name: 'Usage trend chart details' });
    chart.getBoundingClientRect = vi.fn(
      () =>
        ({
          top: 40,
          bottom: 58,
          left: 140,
          right: 290,
          width: 150,
          height: 18,
          x: 140,
          y: 40,
          toJSON: () => ({}),
        }) as DOMRect,
    );

    await fireEvent.mouseEnter(chart);
    await vi.advanceTimersByTimeAsync(400);
    const detail = container.querySelector<HTMLElement>('.trend-detail')!;
    expect(detail).toBeInTheDocument();
    // 58 (bottom) + 12 (gap) = 70
    expect(detail.style.top).toBe('70px');
  });
});
