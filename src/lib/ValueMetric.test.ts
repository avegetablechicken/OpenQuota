import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import ValueMetric from './ValueMetric.svelte';

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));

describe('ValueMetric', () => {
  beforeEach(() => {
    mocks.invoke.mockReset();
    const stored = new Map<string, string>();
    vi.stubGlobal('localStorage', {
      getItem: (key: string) => stored.get(key) ?? null,
      setItem: (key: string, value: string) => stored.set(key, value),
      removeItem: (key: string) => stored.delete(key),
    });
  });
  afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
  });
  it('renders combined credit values with an exact tooltip for large balances', () => {
    render(ValueMetric, {
      label: 'Extra Usage',
      metric: {
        id: 'credits',
        label: 'Extra Usage',
        values: [
          { number: 1200, kind: 'dollars', estimated: false },
          { number: 30000, kind: 'count', label: 'credits', estimated: false },
        ],
        expiriesAt: [],
      },
      now: Date.parse('2026-02-20T16:00:00Z'),
      resetDisplay: 'countdown',
      timeFormat: 'twentyFourHour',
    });

    expect(screen.getByText('$1.2K · 30K credits')).toHaveAttribute(
      'data-tooltip',
      '$1,200.00 · 30,000 credits',
    );
  });

  it('marks only value rows that contain an estimated value', () => {
    render(ValueMetric, {
      label: 'Extra Usage',
      metric: {
        id: 'credits',
        label: 'Extra Usage',
        values: [
          { number: 4, kind: 'dollars', estimated: true },
          { number: 100, kind: 'count', label: 'credits', estimated: false },
        ],
        expiriesAt: [],
      },
      now: Date.parse('2026-02-20T16:00:00Z'),
      resetDisplay: 'countdown',
      timeFormat: 'twentyFourHour',
    });

    expect(screen.getByLabelText('Estimated value')).toHaveAttribute(
      'data-tooltip',
      'Estimated locally, so it may differ from billed usage.',
    );
  });

  it('opens a sorted reset-expiry timeline and distinguishes count-only fallback', async () => {
    const { rerender } = render(ValueMetric, {
      label: 'Rate Limit Resets',
      metric: {
        id: 'rateLimitResets',
        label: 'Rate Limit Resets',
        values: [{ number: 2, kind: 'count', label: 'available', estimated: false }],
        expiriesAt: ['2026-02-20T19:00:00Z', '2026-02-20T17:30:00Z'],
      },
      now: Date.parse('2026-02-20T16:00:00Z'),
      resetDisplay: 'countdown',
      timeFormat: 'twentyFourHour',
    });

    const trigger = screen.getByRole('button', { name: 'Rate Limit Resets: 2 available' });
    expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
    await fireEvent.click(trigger);
    expect(screen.getByRole('dialog', { name: 'Rate Limit Resets details' })).toBeVisible();
    expect(screen.getByText('1h 30m')).toBeInTheDocument();
    expect(screen.getByText('3h')).toBeInTheDocument();

    rerender({
      label: 'Rate Limit Resets',
      metric: {
        id: 'rateLimitResets',
        label: 'Rate Limit Resets',
        values: [{ number: 3, kind: 'count', label: 'available', estimated: false }],
        expiriesAt: [],
      },
      now: Date.parse('2026-02-20T16:00:00Z'),
      resetDisplay: 'countdown',
      timeFormat: 'twentyFourHour',
    });
    expect(screen.getAllByText('3 available')).toHaveLength(2);
    expect(screen.getByText('Expiry times unavailable')).toBeInTheDocument();
  });

  it('opens a clicked reset-expiry timeline below its trigger', async () => {
    render(ValueMetric, {
      label: 'Rate Limit Resets',
      metric: {
        id: 'rateLimitResets',
        label: 'Rate Limit Resets',
        values: [{ number: 2, kind: 'count', label: 'available', estimated: false }],
        expiriesAt: ['2026-02-20T19:00:00Z', '2026-02-20T17:30:00Z'],
      },
      now: Date.parse('2026-02-20T16:00:00Z'),
      resetDisplay: 'countdown',
      timeFormat: 'twentyFourHour',
    });
    const trigger = screen.getByRole('button', { name: 'Rate Limit Resets: 2 available' });
    vi.spyOn(trigger, 'getBoundingClientRect').mockReturnValue({
      top: 240,
      bottom: 260,
      left: 0,
      right: 100,
      width: 100,
      height: 20,
      x: 0,
      y: 240,
      toJSON: () => ({}),
    });

    await fireEvent.click(trigger);

    expect(screen.getByRole('dialog', { name: 'Rate Limit Resets details' }).style.top).toBe(
      '267px',
    );
  });

  it('requires confirmation and claims one explicitly selected reset credit', async () => {
    mocks.invoke.mockResolvedValue('success');
    render(ValueMetric, {
      label: 'Rate Limit Resets',
      metric: {
        id: 'rateLimitResets',
        label: 'Rate Limit Resets',
        values: [{ number: 1, kind: 'count', label: 'available', estimated: false }],
        expiriesAt: ['2026-02-20T19:00:00Z'],
      },
      now: Date.parse('2026-02-20T16:00:00Z'),
      resetDisplay: 'countdown',
      timeFormat: 'twentyFourHour',
    });

    await fireEvent.click(screen.getByRole('button', { name: 'Rate Limit Resets: 1 available' }));
    await fireEvent.click(screen.getByRole('button', { name: /Use reset expiring/ }));
    expect(mocks.invoke).not.toHaveBeenCalled();
    expect(screen.getByRole('group', { name: 'Use this reset?' })).toHaveAccessibleDescription(
      "Immediately reset your usage limits. This can't be undone.",
    );
    expect(
      screen.getByText("Immediately reset your usage limits. This can't be undone."),
    ).toBeInTheDocument();
    await waitFor(() => expect(screen.getByRole('button', { name: 'Cancel' })).toHaveFocus());
    await fireEvent.click(screen.getByRole('button', { name: 'Use reset' }));

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('claim_codex_reset_credit', {
        expiresAt: '2026-02-20T19:00:00Z',
        redeemRequestId: expect.any(String),
      }),
    );
    expect(await screen.findByText('Reset applied.')).toBeInTheDocument();
  });

  it('keeps reset confirmation open when focus moves into the detail panel', async () => {
    vi.useFakeTimers();
    try {
      render(ValueMetric, {
        label: 'Rate Limit Resets',
        metric: {
          id: 'rateLimitResets',
          label: 'Rate Limit Resets',
          values: [{ number: 1, kind: 'count', label: 'available', estimated: false }],
          expiriesAt: ['2026-02-20T19:00:00Z'],
        },
        now: Date.parse('2026-02-20T16:00:00Z'),
        resetDisplay: 'countdown',
        timeFormat: 'twentyFourHour',
      });

      const trigger = screen.getByRole('button', { name: 'Rate Limit Resets: 1 available' });
      trigger.focus();
      await fireEvent.click(trigger);

      const use = screen.getByRole('button', { name: /Use reset expiring/ });
      use.focus();
      await fireEvent.click(use);
      await vi.advanceTimersByTimeAsync(181);

      expect(screen.getByText('Use this reset?')).toBeInTheDocument();
      expect(screen.getByRole('button', { name: 'Cancel' })).toHaveFocus();
      expect(screen.getByRole('dialog', { name: 'Rate Limit Resets details' })).toBeVisible();
      expect(mocks.invoke).not.toHaveBeenCalled();
    } finally {
      vi.useRealTimers();
    }
  });

  it('behaves as an anchored popover and closes with Escape', async () => {
    vi.useFakeTimers();
    try {
      render(ValueMetric, {
        label: 'Rate Limit Resets',
        metric: {
          id: 'rateLimitResets',
          label: 'Rate Limit Resets',
          values: [{ number: 1, kind: 'count', label: 'available', estimated: false }],
          expiriesAt: ['2026-02-20T19:00:00Z'],
        },
        now: Date.parse('2026-02-20T16:00:00Z'),
        resetDisplay: 'countdown',
        timeFormat: 'twentyFourHour',
      });

      await fireEvent.click(screen.getByRole('button', { name: 'Rate Limit Resets: 1 available' }));
      expect(screen.queryByLabelText('Drag Rate Limit Resets panel')).not.toBeInTheDocument();
      expect(
        screen.queryByRole('button', { name: 'Close Rate Limit Resets' }),
      ).not.toBeInTheDocument();

      const use = screen.getByRole('button', { name: /Use reset expiring/ });
      await fireEvent.click(use);
      let cancel = screen.getByRole('button', { name: 'Cancel' });
      await vi.waitFor(() => expect(cancel).toHaveFocus());
      await fireEvent.click(cancel);
      expect(screen.queryByText('Use this reset?')).not.toBeInTheDocument();
      let restoredUse = screen.getByRole('button', { name: /Use reset expiring/ });
      await vi.waitFor(() => expect(restoredUse).toHaveFocus());

      await fireEvent.click(restoredUse);
      cancel = screen.getByRole('button', { name: 'Cancel' });
      await vi.waitFor(() => expect(cancel).toHaveFocus());
      await fireEvent.keyDown(cancel, { key: 'Escape' });
      expect(screen.queryByText('Use this reset?')).not.toBeInTheDocument();
      const dialog = screen.getByRole('dialog', { name: 'Rate Limit Resets details' });
      expect(dialog).toBeVisible();
      restoredUse = screen.getByRole('button', { name: /Use reset expiring/ });
      await vi.waitFor(() => expect(restoredUse).toHaveFocus());
      await fireEvent.mouseLeave(dialog);
      await vi.advanceTimersByTimeAsync(181);
      expect(dialog).toBeVisible();

      await fireEvent.keyDown(restoredUse, { key: 'Escape' });
      expect(
        screen.queryByRole('dialog', { name: 'Rate Limit Resets details' }),
      ).not.toBeInTheDocument();
      await vi.waitFor(() =>
        expect(
          screen.getByRole('button', { name: 'Rate Limit Resets: 1 available' }),
        ).toHaveFocus(),
      );
      await vi.advanceTimersByTimeAsync(351);
      expect(
        screen.queryByRole('dialog', { name: 'Rate Limit Resets details' }),
      ).not.toBeInTheDocument();
    } finally {
      vi.useRealTimers();
    }
  });

  it('closes on external scroll without stealing focus back to the trigger', async () => {
    vi.useFakeTimers();
    try {
      render(ValueMetric, {
        label: 'Rate Limit Resets',
        now: Date.parse('2026-10-08T12:00:00Z'),
        resetDisplay: 'countdown',
        timeFormat: 'system',
        metric: {
          id: 'rateLimitResets',
          label: 'Rate Limit Resets',
          values: [{ number: 1, kind: 'count', label: 'available', estimated: false }],
          expiriesAt: ['2026-10-09T12:00:00Z'],
        },
      });
      const trigger = screen.getByRole('button', { name: 'Rate Limit Resets: 1 available' });
      await fireEvent.mouseEnter(trigger);
      await vi.advanceTimersByTimeAsync(350);
      expect(screen.getByRole('dialog', { name: 'Rate Limit Resets details' })).toBeVisible();

      window.dispatchEvent(new Event('scroll'));
      await vi.waitFor(() =>
        expect(
          screen.queryByRole('dialog', { name: 'Rate Limit Resets details' }),
        ).not.toBeInTheDocument(),
      );
      expect(trigger).not.toHaveFocus();
    } finally {
      vi.useRealTimers();
    }
  });

  it('shows Claude reset kinds, enforces redeemability, and invokes the Claude claim command', async () => {
    mocks.invoke.mockResolvedValue('success');
    render(ValueMetric, {
      label: 'Rate Limit Resets',
      metric: {
        id: 'claudeRateLimitResets',
        label: 'Rate Limit Resets',
        values: [{ number: 2, kind: 'count', label: 'available', estimated: false }],
        expiriesAt: ['2026-02-20T19:00:00Z', '2026-03-01T19:00:00Z'],
        resetCredits: [
          {
            expiresAt: '2026-02-20T19:00:00Z',
            kind: 'fable',
            redeemable: true,
            claimAccount: 'account-a',
          },
          { expiresAt: '2026-03-01T19:00:00Z', kind: 'full', redeemable: false },
        ],
      },
      now: Date.parse('2026-02-20T16:00:00Z'),
      resetDisplay: 'countdown',
      timeFormat: 'twentyFourHour',
    });

    await fireEvent.click(screen.getByRole('button', { name: 'Rate Limit Resets: 2 available' }));
    expect(screen.getByText('Fable')).toBeInTheDocument();
    expect(screen.getByText('Full')).toBeInTheDocument();

    const buttons = screen.getAllByRole('button', { name: /Use reset expiring/ });
    expect(buttons[0]).toBeEnabled();
    expect(buttons[1]).toBeDisabled();

    await fireEvent.click(buttons[0]);
    expect(
      screen.getByText("Restore Fable weekly usage and refill your session. This can't be undone."),
    ).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: 'Use reset' }));

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('claim_claude_reset_credit', {
        expiresAt: '2026-02-20T19:00:00Z',
        kind: 'fable',
        claimAccount: 'account-a',
        redeemRequestId: expect.any(String),
      }),
    );
    expect(await screen.findByText('Reset applied.')).toBeInTheDocument();
  });

  it('disables Claude claims from older snapshots without an account identity', async () => {
    render(ValueMetric, {
      label: 'Rate Limit Resets',
      metric: {
        id: 'claudeRateLimitResets',
        label: 'Rate Limit Resets',
        values: [{ number: 1, kind: 'count', label: 'available', estimated: false }],
        expiriesAt: ['2026-02-20T19:00:00Z'],
        resetCredits: [{ expiresAt: '2026-02-20T19:00:00Z', kind: 'fable', redeemable: true }],
      },
      now: Date.parse('2026-02-20T16:00:00Z'),
      resetDisplay: 'countdown',
      timeFormat: 'twentyFourHour',
    });
    await fireEvent.click(screen.getByRole('button', { name: 'Rate Limit Resets: 1 available' }));
    expect(screen.getByRole('button', { name: /Use reset expiring/ })).toBeDisabled();
    expect(mocks.invoke).not.toHaveBeenCalled();
  });

  it('renders Sub2API Claude reset credits as read-only with kind and expiry tooltip', () => {
    render(ValueMetric, {
      label: 'Rate Limit Resets',
      metric: {
        id: 'sub2apiRateLimitResets',
        label: 'Rate Limit Resets',
        values: [{ number: 2, kind: 'count', label: 'available', estimated: false }],
        expiriesAt: ['2026-02-20T17:30:00Z', '2026-02-20T19:00:00Z'],
        resetCredits: [
          { expiresAt: '2026-02-20T17:30:00Z', kind: 'fable', redeemable: false },
          { expiresAt: '2026-02-20T19:00:00Z', kind: 'full', redeemable: false },
        ],
      },
      now: Date.parse('2026-02-20T16:00:00Z'),
      resetDisplay: 'countdown',
      timeFormat: 'twentyFourHour',
    });

    expect(screen.queryByRole('button')).not.toBeInTheDocument();
    const reading = screen.getByText('2 available');
    expect(reading).toHaveAttribute(
      'data-tooltip',
      'Resets expire in:\n1. Fable · 1h 30m\n2. Full · 3h',
    );
  });
  function claudeResetProps(account: string) {
    return {
      label: 'Rate Limit Resets',
      metric: {
        id: 'claudeRateLimitResets',
        label: 'Rate Limit Resets',
        values: [{ number: 2, kind: 'count' as const, label: 'available', estimated: false }],
        expiriesAt: ['2026-02-20T19:00:00Z'],
        resetCredits: [
          {
            expiresAt: '2026-02-20T19:00:00Z',
            kind: 'fable' as const,
            redeemable: true,
            claimAccount: account,
          },
        ],
      },
      now: Date.parse('2026-02-20T16:00:00Z'),
      resetDisplay: 'countdown' as const,
      timeFormat: 'twentyFourHour' as const,
    };
  }

  async function openAndClaim() {
    await fireEvent.click(screen.getByRole('button', { name: 'Rate Limit Resets: 2 available' }));
    await fireEvent.click(screen.getByRole('button', { name: /Use reset expiring/ }));
    await fireEvent.click(screen.getByRole('button', { name: 'Use reset' }));
  }

  it('keeps an uncertain debit blocked after the popover and its parent are remounted', async () => {
    mocks.invoke.mockResolvedValue('uncertain');
    const props = claudeResetProps('uncertain-account');
    const rendered = render(ValueMetric, props);
    await openAndClaim();
    expect(await screen.findByText(/The reset may have been applied/)).toBeVisible();
    expect(screen.queryByText('Could not use this reset. Try again.')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: /Use reset expiring/ })).toBeDisabled();
    const first = mocks.invoke.mock.calls[0][1].redeemRequestId;
    await fireEvent.keyDown(screen.getByRole('dialog'), { key: 'Escape' });
    await waitFor(() => expect(screen.queryByRole('dialog')).not.toBeInTheDocument());
    await fireEvent.click(screen.getByRole('button', { name: 'Rate Limit Resets: 2 available' }));
    expect(screen.getByRole('button', { name: /Use reset expiring/ })).toBeDisabled();
    rendered.unmount();
    render(ValueMetric, props);
    await fireEvent.click(screen.getByRole('button', { name: 'Rate Limit Resets: 2 available' }));
    expect(screen.getByRole('button', { name: /Use reset expiring/ })).toBeDisabled();
    expect(mocks.invoke).toHaveBeenCalledTimes(1);
    const saved = localStorage.getItem(
      'openquota.reset-claim.v1:claude:uncertain-account:2026-02-20T19:00:00Z:fable',
    );
    expect(JSON.parse(saved!)).toEqual({ requestId: first, status: 'uncertain' });
  });

  it('keeps the same request ID for a retryable failure after remounting', async () => {
    mocks.invoke.mockResolvedValueOnce('failed').mockResolvedValueOnce('success');
    const props = claudeResetProps('retryable-account');
    const rendered = render(ValueMetric, props);
    await openAndClaim();
    expect(await screen.findByText('Could not use this reset. Try again.')).toBeVisible();
    const first = mocks.invoke.mock.calls[0][1].redeemRequestId;
    rendered.unmount();
    render(ValueMetric, props);
    await openAndClaim();
    expect(await screen.findByText('Reset applied.')).toBeVisible();
    expect(mocks.invoke.mock.calls[1][1].redeemRequestId).toBe(first);
  });

  it('refreshes usage and blocks resending when the claim invocation loses its response', async () => {
    mocks.invoke.mockRejectedValueOnce(new Error('connection lost')).mockResolvedValueOnce({});
    render(ValueMetric, claudeResetProps('lost-response-account'));
    await openAndClaim();
    expect(await screen.findByText(/The reset may have been applied/)).toBeVisible();
    expect(mocks.invoke).toHaveBeenCalledWith('refresh_provider_usage', { providerId: 'claude' });
    expect(screen.getByRole('button', { name: /Use reset expiring/ })).toBeDisabled();
  });

  it('clears the result when the displayed account or reset entries change', async () => {
    mocks.invoke.mockResolvedValue('success');
    const first = claudeResetProps('result-account-a');
    const rendered = render(ValueMetric, first);
    await openAndClaim();
    expect(await screen.findByText('Reset applied.')).toBeVisible();
    await rendered.rerender(claudeResetProps('result-account-b'));
    expect(screen.queryByText('Reset applied.')).not.toBeInTheDocument();
    await rendered.rerender(first);
    expect(screen.queryByText('Reset applied.')).not.toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: /Use reset expiring/ }));
    await fireEvent.click(screen.getByRole('button', { name: 'Use reset' }));
    expect(await screen.findByText('Reset applied.')).toBeVisible();
    await rendered.rerender({
      ...first,
      metric: { ...first.metric, resetCredits: [], expiriesAt: [] },
    });
    expect(screen.queryByText('Reset applied.')).not.toBeInTheDocument();
  });

  it('does not publish a late claim result under a different displayed account', async () => {
    let resolve!: (outcome: string) => void;
    mocks.invoke.mockReturnValue(
      new Promise<string>((done) => {
        resolve = done;
      }),
    );
    const rendered = render(ValueMetric, claudeResetProps('pending-result-account-a'));
    await openAndClaim();
    await rendered.rerender(claudeResetProps('pending-result-account-b'));
    resolve('success');
    await waitFor(() =>
      expect(screen.getByRole('button', { name: /Use reset expiring/ })).toBeEnabled(),
    );
    expect(screen.queryByText('Reset applied.')).not.toBeInTheDocument();
  });
});
