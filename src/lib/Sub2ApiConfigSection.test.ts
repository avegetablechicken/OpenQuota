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
    expect(screen.getByText('Account')).toBeInTheDocument();
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
    expect(screen.getByText('Account')).toBeInTheDocument();
  });

  it('confirms a save before backend persistence finishes', async () => {
    let resolveSave!: (value: {
      configured: boolean;
      baseUrl: string;
      email: string;
      upstream: string;
    }) => void;
    const pendingSave = new Promise<{
      configured: boolean;
      baseUrl: string;
      email: string;
      upstream: string;
    }>((resolve) => {
      resolveSave = resolve;
    });
    mocks.invoke.mockImplementation((command: string) => {
      if (command === 'get_sub2api_config_state') {
        return Promise.resolve({ configured: false, baseUrl: '', email: '', upstream: 'codex' });
      }
      if (command === 'save_sub2api_config') return pendingSave;
      return Promise.reject(new Error(`unexpected command ${command}`));
    });
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await fireEvent.input(screen.getByLabelText('Sub2API Base URL'), {
      target: { value: 'https://pending.example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator email'), {
      target: { value: 'pending@example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator password'), {
      target: { value: 'pending-password' },
    });

    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('save_sub2api_config', expect.anything()),
    );
    expect(screen.getByText('pending@example.com')).toBeInTheDocument();
    expect(screen.getByRole('button', { name: 'Edit' })).toBeInTheDocument();
    expect(screen.queryByLabelText('Sub2API administrator password')).not.toBeInTheDocument();

    resolveSave({
      configured: true,
      baseUrl: 'https://pending.example.com',
      email: 'pending@example.com',
      upstream: 'codex',
    });
    await waitFor(() => expect(screen.getByText('pending@example.com')).toBeInTheDocument());
  });

  it('restores the editor when optimistic persistence fails', async () => {
    let rejectSave!: (reason: Error) => void;
    const pendingSave = new Promise<never>((_resolve, reject) => {
      rejectSave = reject;
    });
    mocks.invoke.mockImplementation((command: string) => {
      if (command === 'get_sub2api_config_state') {
        return Promise.resolve({ configured: false, baseUrl: '', email: '', upstream: 'codex' });
      }
      if (command === 'save_sub2api_config') return pendingSave;
      return Promise.reject(new Error(`unexpected command ${command}`));
    });
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await fireEvent.input(screen.getByLabelText('Sub2API Base URL'), {
      target: { value: 'https://failed.example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator email'), {
      target: { value: 'failed@example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator password'), {
      target: { value: 'failed-password' },
    });

    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await screen.findByRole('button', { name: 'Edit' });
    rejectSave(new Error('Credential store unavailable'));

    expect(await screen.findByRole('alert')).toHaveTextContent('Credential store unavailable');
    expect(screen.getByLabelText('Sub2API Base URL')).toHaveValue('https://failed.example.com');
    expect(screen.getByLabelText('Sub2API administrator email')).toHaveValue('failed@example.com');
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveValue('');
    expect(screen.getByText('Not configured')).toBeInTheDocument();
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
    expect(screen.getByText('Account')).toBeInTheDocument();
  });

  it('deletes an item from the separate bottom action row', async () => {
    const onRemove = vi.fn();
    render(Sub2ApiConfigSection, { providerId: 'sub2api@3', onRemove });
    await screen.findByRole('region', { name: 'Sub2API Connection' });
    await fireEvent.click(screen.getByRole('button', { name: /^Delete Remove/ }));

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
