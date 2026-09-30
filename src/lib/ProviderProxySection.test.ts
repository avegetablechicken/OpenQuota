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

async function setup(existing = '') {
  const settings = structuredClone(settingsState.settings);
  const provider = settings.providers[0];
  settings.providerProxies = { [provider.id]: existing, other: 'http://localhost:8888' };
  const onChange = vi.fn();
  const view = render(ProviderProxySection, { settings, provider, onChange });
  await fireEvent.click(screen.getByRole('button', { name: /^Proxy/ }));
  return {
    get input() {
      return screen.getByLabelText('Proxy URL') as HTMLInputElement;
    },
    get mode() {
      return screen.getByRole('combobox', { name: 'Proxy mode' });
    },
    onChange,
    provider,
    view,
  };
}

it('lets the panel follow the disclosure animation in both directions', async () => {
  const settings = structuredClone(settingsState.settings);
  const provider = settings.providers[0];
  const onContentMorph = vi.fn();
  render(ProviderProxySection, { settings, provider, onChange: vi.fn(), onContentMorph });
  const disclosure = screen.getByRole('button', { name: /^Proxy/ });
  await fireEvent.click(disclosure);
  await fireEvent.click(disclosure);
  expect(onContentMorph).toHaveBeenCalledTimes(2);
});

it('omits the header summary while the system proxy is used', () => {
  const settings = structuredClone(settingsState.settings);
  const provider = settings.providers[0];
  render(ProviderProxySection, { settings, provider, onChange: vi.fn() });
  const disclosure = screen.getByRole('button', { name: /^Proxy/ });
  expect(disclosure).toHaveAttribute('aria-expanded', 'false');
  expect(disclosure).not.toHaveTextContent('System');
});

it('starts collapsed and summarizes the saved mode in the header', async () => {
  const settings = structuredClone(settingsState.settings);
  const provider = settings.providers[0];
  settings.providerProxies = { [provider.id]: 'direct' };
  render(ProviderProxySection, { settings, provider, onChange: vi.fn() });
  const disclosure = screen.getByRole('button', { name: /^Proxy/ });
  expect(disclosure).toHaveAttribute('aria-expanded', 'false');
  expect(disclosure).toHaveTextContent('Direct');
  expect(screen.queryByRole('combobox', { name: 'Proxy mode' })).toBeNull();
  await fireEvent.click(disclosure);
  expect(disclosure).toHaveAttribute('aria-expanded', 'true');
  expect(screen.getByRole('combobox', { name: 'Proxy mode' })).toHaveTextContent('Direct');
});

async function chooseMode(name: 'System' | 'Direct' | 'Custom') {
  await fireEvent.click(screen.getByRole('combobox', { name: 'Proxy mode' }));
  await fireEvent.click(screen.getByRole('option', { name }));
}

it('uses system routing by default and saves direct routing from the mode menu', async () => {
  const { mode, onChange, provider } = await setup();
  expect(mode).toHaveTextContent('System');
  expect(screen.queryByLabelText('Proxy URL')).toBeNull();
  await chooseMode('Direct');
  expect(screen.queryByLabelText('Proxy URL')).toBeNull();
  expect(onChange.mock.calls[0][0].providerProxies).toEqual({
    [provider.id]: 'direct',
    other: 'http://localhost:8888',
  });
});

it('reveals the URL row for custom routing without saving until a URL is committed', async () => {
  const { onChange, provider } = await setup();
  await chooseMode('Custom');
  const input = screen.getByLabelText('Proxy URL') as HTMLInputElement;
  expect(input.value).toBe('');
  expect(onChange).not.toHaveBeenCalled();
  await fireEvent.input(input, { target: { value: 'http://localhost:7890' } });
  await fireEvent.blur(input);
  expect(onChange.mock.calls[0][0].providerProxies).toEqual({
    [provider.id]: 'http://localhost:7890',
    other: 'http://localhost:8888',
  });
});

it('saves only the current configuration and trims the visible URL', async () => {
  const { input, onChange, provider } = await setup('http://localhost:7890');
  expect(input.type).toBe('text');
  await fireEvent.input(input, { target: { value: '  socks5h://localhost:1080  ' } });
  await fireEvent.blur(input);
  expect(onChange.mock.calls[0][0].providerProxies).toEqual({
    [provider.id]: 'socks5h://localhost:1080',
    other: 'http://localhost:8888',
  });
});

it('clearing the URL selects direct routing while System restores system routing', async () => {
  const { input, mode, onChange, provider, view } = await setup('http://localhost:7890');
  await fireEvent.input(input, { target: { value: '' } });
  await fireEvent.blur(input);
  const next = onChange.mock.calls[0][0];
  expect(next.providerProxies[provider.id]).toBe('direct');
  await view.rerender({ settings: next, provider, onChange });
  expect(mode).toHaveTextContent('Direct');
  expect(screen.queryByLabelText('Proxy URL')).toBeNull();
  await chooseMode('System');
  expect(onChange.mock.calls[1][0].providerProxies).toEqual({ other: 'http://localhost:8888' });
});

it('rejects invalid URLs and allows Escape to restore the saved address', async () => {
  const { input, onChange } = await setup('http://localhost:7890');
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
  const { provider } = await setup('http://localhost:7890');
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
  const { input, onChange, provider, view } = await setup('http://localhost:7890');
  await vi.advanceTimersByTimeAsync(400);
  await fireEvent.input(input, { target: { value: 'http://localhost:8889' } });
  await fireEvent.blur(input);
  await view.rerender({ settings: onChange.mock.calls[0][0], provider, onChange });
  await vi.advanceTimersByTimeAsync(400);
  await tick();
  finishOld({ ip: '203.0.113.8', countryCode: 'JP' });
  await tick();
  expect(screen.getByRole('img').textContent).toBe('🇺🇸');
  await chooseMode('System');
  expect(screen.queryByRole('img')).toBeNull();
});

it('shows no flag when geolocation fails and preserves the configured proxy', async () => {
  vi.useFakeTimers();
  vi.mocked(probeProviderProxy).mockRejectedValueOnce(new Error('unavailable'));
  const { input, mode, provider, onChange } = await setup('http://localhost:7890');
  await vi.advanceTimersByTimeAsync(400);
  await tick();
  expect(probeProviderProxy).toHaveBeenCalledWith(provider.id, 'http://localhost:7890');
  expect(screen.queryByRole('img')).toBeNull();
  expect(screen.queryByRole('alert')).toBeNull();
  expect(mode).toHaveTextContent('Custom');
  expect(input.value).toBe('http://localhost:7890');
  expect(onChange).not.toHaveBeenCalled();
});

it.each(['', 'direct'])('does not probe or show a flag without a proxy URL (%s)', async (value) => {
  vi.useFakeTimers();
  await setup(value);
  await vi.advanceTimersByTimeAsync(500);
  expect(probeProviderProxy).not.toHaveBeenCalled();
  expect(screen.queryByRole('img')).toBeNull();
  expect(screen.queryByRole('status')).toBeNull();
});

it('clears the previous flag and stops probing when the URL is cleared', async () => {
  vi.useFakeTimers();
  const { input, onChange, provider, view } = await setup('http://localhost:7890');
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
