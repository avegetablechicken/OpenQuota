import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { get } from 'svelte/store';
import Sub2ApiConfigSection from './Sub2ApiConfigSection.svelte';
import {
  forgetSub2ApiUpstream,
  rememberSub2ApiUpstream,
  sub2ApiUpstreams,
} from './sub2ApiUpstreams';

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));

describe('Sub2ApiConfigSection', () => {
  beforeEach(() => {
    mocks.invoke.mockReset().mockImplementation((command: string) => {
      if (command === 'get_sub2api_config_state') {
        return Promise.resolve({ configured: false, baseUrl: '', email: '', upstream: 'codex' });
      }
      if (command === 'save_sub2api_config') {
        return Promise.resolve({
          configured: true,
          baseUrl: 'https://sub2api.example.com',
          email: 'admin@example.com',
          upstream: 'claude',
        });
      }
      if (command === 'clear_sub2api_config') {
        return Promise.resolve({ configured: false, baseUrl: '', email: '', upstream: 'codex' });
      }
      if (command === 'delete_sub2api_config') {
        return Promise.resolve({ configured: false, baseUrl: '', email: '', upstream: 'codex' });
      }
      return Promise.reject(new Error(`unexpected command ${command}`));
    });
  });

  afterEach(() => {
    cleanup();
    forgetSub2ApiUpstream('sub2api@2');
  });

  it('forgets a stale upstream when the configuration is not configured', async () => {
    rememberSub2ApiUpstream('sub2api@2', 'codex', 'https://sub2api.example.com');
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });

    await screen.findByText('Not configured');
    expect(get(sub2ApiUpstreams)['sub2api@2']).toBeUndefined();
  });

  it('saves a Claude upstream login without retaining the password in the UI', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api' });
    expect(await screen.findByRole('region', { name: 'Sub2API Connection' })).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await fireEvent.input(screen.getByLabelText('Sub2API Base URL'), {
      target: { value: 'https://sub2api.example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator email'), {
      target: { value: 'admin@example.com' },
    });
    const password = screen.getByLabelText('Sub2API administrator password');
    expect(password).toHaveAttribute('type', 'password');
    await fireEvent.input(password, { target: { value: 'secret-password' } });
    await fireEvent.click(screen.getByRole('radio', { name: 'Claude' }));
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('save_sub2api_config', {
        providerId: 'sub2api',
        config: {
          baseUrl: 'https://sub2api.example.com',
          email: 'admin@example.com',
          password: 'secret-password',
          upstream: 'claude',
        },
      }),
    );
    expect(screen.queryByDisplayValue('secret-password')).not.toBeInTheDocument();
    expect(screen.getByText('admin@example.com')).toBeInTheDocument();
    expect(screen.getByText('Claude upstream')).toBeInTheDocument();
  });

  it('clears saved connection fields without deleting the Sub2API item', async () => {
    mocks.invoke.mockImplementation((command: string) => {
      if (command === 'get_sub2api_config_state') {
        return Promise.resolve({
          configured: true,
          baseUrl: 'https://sub2api.example.com',
          email: 'admin@example.com',
          upstream: 'codex',
        });
      }
      if (command === 'clear_sub2api_config') {
        return Promise.resolve({ configured: false, baseUrl: '', email: '', upstream: 'codex' });
      }
      return Promise.reject(new Error(`unexpected command ${command}`));
    });
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('admin@example.com');
    await fireEvent.click(screen.getByRole('button', { name: 'Edit' }));
    await fireEvent.click(screen.getByRole('button', { name: 'Clear Sub2API connection' }));
    expect(
      screen.getByRole('group', { name: 'Clear Sub2API connection?' }),
    ).toHaveAccessibleDescription(
      'The Base URL and saved login will be removed. This Sub2API item will remain.',
    );
    await fireEvent.click(screen.getByRole('button', { name: 'Clear' }));
    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('clear_sub2api_config', {
        providerId: 'sub2api@2',
      }),
    );
    expect(mocks.invoke).not.toHaveBeenCalledWith('delete_sub2api_config', expect.anything());
    expect(screen.getByText('Not configured')).toBeInTheDocument();
  });

  it('deletes an item from the separate bottom action row', async () => {
    const onRemove = vi.fn();
    render(Sub2ApiConfigSection, { providerId: 'sub2api@3', onRemove });
    await screen.findByRole('region', { name: 'Sub2API Connection' });
    await fireEvent.click(screen.getByRole('button', { name: /^Delete Sub2API/ }));

    expect(
      screen.getByRole('group', { name: 'Delete this Sub2API item?' }),
    ).toHaveAccessibleDescription('This empty configuration item will be removed.');
    await fireEvent.click(screen.getByRole('button', { name: 'Delete' }));

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('delete_sub2api_config', {
        providerId: 'sub2api@3',
      }),
    );
    expect(onRemove).toHaveBeenCalledOnce();
  });
});
