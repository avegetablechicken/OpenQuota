import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, expect, it, vi } from 'vitest';
import { tick } from 'svelte';
import { probeProviderProxy } from './backend';
import type { ProxyExitLocation } from './types';
import { settingsState } from '../test/appFixtures';
import ProviderProxySection from './ProviderProxySection.svelte';

vi.mock('./backend', () => ({ probeProviderProxy: vi.fn() }));
beforeEach(() => {
  vi.mocked(probeProviderProxy)
    .mockReset()
    .mockResolvedValue({ ip: '203.0.113.7', countryCode: 'US' });
});
afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

function setup(existing = '') {
  const settings = structuredClone(settingsState.settings);
  const provider = settings.providers[0];
  settings.providerProxies = { [provider.id]: existing, other: 'http://localhost:8888' };
  const onChange = vi.fn();
  const view = render(ProviderProxySection, { settings, provider, onChange });
  return {
    get input() {
      return screen.getByLabelText('Proxy URL') as HTMLInputElement;
    },
    toggle: screen.getByRole('switch', { name: 'Use custom proxy' }) as HTMLInputElement,
    onChange,
    provider,
    view,
  };
}

it('uses system routing with customization off and enables direct routing with an empty URL', async () => {
  const { toggle, onChange, provider } = setup();
  expect(toggle.checked).toBe(false);
  expect(screen.queryByLabelText('Proxy URL')).toBeNull();
  expect(screen.getByText(/Uses the system proxy by default/)).toBeTruthy();
  await fireEvent.click(toggle);
  const input = screen.getByLabelText('Proxy URL') as HTMLInputElement;
  expect(input.disabled).toBe(false);
  expect(input.value).toBe('');
  expect(onChange.mock.calls[0][0].providerProxies).toEqual({
    [provider.id]: 'direct',
    other: 'http://localhost:8888',
  });
});

it('saves only the current configuration and trims the visible URL', async () => {
  const { input, onChange, provider } = setup('direct');
  expect(input.type).toBe('text');
  await fireEvent.input(input, { target: { value: '  socks5h://localhost:1080  ' } });
  await fireEvent.blur(input);
  expect(onChange.mock.calls[0][0].providerProxies).toEqual({
    [provider.id]: 'socks5h://localhost:1080',
    other: 'http://localhost:8888',
  });
});

it('clearing the URL selects direct routing while switching off restores system routing', async () => {
  const { input, toggle, onChange, provider, view } = setup('http://localhost:7890');
  await fireEvent.input(input, { target: { value: '' } });
  await fireEvent.blur(input);
  const next = onChange.mock.calls[0][0];
  expect(next.providerProxies[provider.id]).toBe('direct');
  await view.rerender({ settings: next, provider, onChange });
  expect(toggle.checked).toBe(true);
  expect(input.value).toBe('');
  await fireEvent.click(toggle);
  expect(onChange.mock.calls[1][0].providerProxies).toEqual({ other: 'http://localhost:8888' });
});

it('rejects invalid URLs and allows Escape to restore the saved address', async () => {
  const { input, onChange } = setup('http://localhost:7890');
  await fireEvent.input(input, { target: { value: 'ftp://localhost/path' } });
  await fireEvent.blur(input);
  expect(screen.getByRole('alert')).toBeTruthy();
  expect(onChange).not.toHaveBeenCalled();
  await fireEvent.keyDown(input, { key: 'Escape' });
  expect(input.value).toBe('http://localhost:7890');
  expect(onChange).not.toHaveBeenCalled();
});

it('shows a flag with the detected exit country and IP after checking the configured route', async () => {
  vi.useFakeTimers();
  const { provider } = setup('http://localhost:7890');
  expect(screen.queryByRole('img')).toBeNull();
  await vi.advanceTimersByTimeAsync(400);
  await tick();
  expect(probeProviderProxy).toHaveBeenCalledWith(provider.id, 'http://localhost:7890');
  const flag = screen.getByRole('img', { name: 'United States · 203.0.113.7' });
  expect(flag.textContent).toBe('🇺🇸');
});

it('discards an old lookup after the proxy changes and clears the flag when disabled', async () => {
  vi.useFakeTimers();
  let finishOld!: (value: ProxyExitLocation) => void;
  vi.mocked(probeProviderProxy).mockImplementationOnce(
    () =>
      new Promise((resolve) => {
        finishOld = resolve;
      }),
  );
  const { input, toggle, onChange, provider, view } = setup('http://localhost:7890');
  await vi.advanceTimersByTimeAsync(400);
  await fireEvent.input(input, { target: { value: 'http://localhost:8889' } });
  await fireEvent.blur(input);
  await view.rerender({ settings: onChange.mock.calls[0][0], provider, onChange });
  await vi.advanceTimersByTimeAsync(400);
  await tick();
  finishOld({ ip: '203.0.113.8', countryCode: 'JP' });
  await tick();
  expect(screen.getByRole('img').textContent).toBe('🇺🇸');
  await fireEvent.click(toggle);
  expect(screen.queryByRole('img')).toBeNull();
});

it('shows no flag when geolocation fails and preserves the configured proxy', async () => {
  vi.useFakeTimers();
  vi.mocked(probeProviderProxy).mockRejectedValueOnce(new Error('unavailable'));
  const { input, toggle, provider, onChange } = setup('http://localhost:7890');
  await vi.advanceTimersByTimeAsync(400);
  await tick();
  expect(probeProviderProxy).toHaveBeenCalledWith(provider.id, 'http://localhost:7890');
  expect(screen.queryByRole('img')).toBeNull();
  expect(screen.queryByRole('alert')).toBeNull();
  expect(toggle.checked).toBe(true);
  expect(input.value).toBe('http://localhost:7890');
  expect(onChange).not.toHaveBeenCalled();
});

it.each(['', 'direct'])('does not probe or show a flag without a proxy URL (%s)', async (value) => {
  vi.useFakeTimers();
  setup(value);
  await vi.advanceTimersByTimeAsync(500);
  expect(probeProviderProxy).not.toHaveBeenCalled();
  expect(screen.queryByRole('img')).toBeNull();
  expect(screen.queryByRole('status')).toBeNull();
});

it('clears the previous flag and stops probing when the URL is cleared', async () => {
  vi.useFakeTimers();
  const { input, onChange, provider, view } = setup('http://localhost:7890');
  await vi.advanceTimersByTimeAsync(400);
  await tick();
  expect(screen.getByRole('img')).toBeTruthy();
  await fireEvent.input(input, { target: { value: '' } });
  await fireEvent.blur(input);
  await view.rerender({ settings: onChange.mock.calls[0][0], provider, onChange });
  await vi.advanceTimersByTimeAsync(500);
  expect(probeProviderProxy).toHaveBeenCalledTimes(1);
  expect(screen.queryByRole('img')).toBeNull();
});
