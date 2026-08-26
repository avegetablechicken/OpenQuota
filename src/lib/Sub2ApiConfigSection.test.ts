import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/svelte';
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
        return Promise.resolve({
          configured: false,
          baseUrl: '',
          codexProvider: '',
          customBaseUrl: false,
          email: '',
          upstream: 'codex',
        });
      }
      if (command === 'resolve_sub2api_codex_provider') {
        return Promise.resolve('https://resolved.example.com');
      }
      if (command === 'resolve_sub2api_claude_base_url') {
        return Promise.resolve('https://resolved-claude.example.com');
      }
      if (command === 'save_sub2api_config') {
        return Promise.resolve({
          configured: true,
          baseUrl: 'https://resolved-claude.example.com',
          codexProvider: '',
          customBaseUrl: false,
          email: 'admin@example.com',
          upstream: 'claude',
        });
      }
      if (command === 'clear_sub2api_config') {
        return Promise.resolve({
          configured: false,
          baseUrl: '',
          codexProvider: '',
          customBaseUrl: false,
          email: '',
          upstream: 'codex',
        });
      }
      if (command === 'delete_sub2api_config') {
        return Promise.resolve({
          configured: false,
          baseUrl: '',
          codexProvider: '',
          customBaseUrl: false,
          email: '',
          upstream: 'codex',
        });
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
    const connections = screen.getByRole('list', { name: 'Connection configurations' });
    expect(within(connections).getAllByRole('listitem')).toHaveLength(1);
    expect(within(connections).getByText('Sub2API')).toBeInTheDocument();
    expect(get(sub2ApiUpstreams)['sub2api@2']).toBeUndefined();
  });

  it('saves a Claude upstream login without retaining the password in the UI', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api' });
    expect(await screen.findByRole('region', { name: 'Connection' })).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await fireEvent.click(screen.getByRole('radio', { name: 'Claude' }));
    const baseUrl = screen.getByLabelText('Base URL');
    await waitFor(() => expect(baseUrl).toHaveValue('https://resolved-claude.example.com'));
    expect(baseUrl).toBeDisabled();
    expect(screen.getByRole('switch', { name: 'Use custom Base URL' })).not.toBeChecked();
    await fireEvent.input(screen.getByLabelText('Sub2API administrator email'), {
      target: { value: 'admin@example.com' },
    });
    const password = screen.getByLabelText('Sub2API administrator password');
    expect(password).toHaveAttribute('type', 'password');
    await fireEvent.input(password, { target: { value: 'secret-password' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('save_sub2api_config', {
        providerId: 'sub2api',
        config: {
          baseUrl: 'https://resolved-claude.example.com',
          codexProvider: '',
          customBaseUrl: false,
          email: 'admin@example.com',
          password: 'secret-password',
          upstream: 'claude',
        },
      }),
    );
    expect(screen.queryByDisplayValue('secret-password')).not.toBeInTheDocument();
    expect(screen.getByText('admin@example.com')).toBeInTheDocument();
    expect(screen.getByText('Sub2API')).toBeInTheDocument();
  });

  it('allows a custom Claude Base URL only after its switch is enabled', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await fireEvent.click(screen.getByRole('radio', { name: 'Claude' }));
    const baseUrl = screen.getByLabelText('Base URL');
    await waitFor(() => expect(baseUrl).toHaveValue('https://resolved-claude.example.com'));

    await fireEvent.click(screen.getByRole('switch', { name: 'Use custom Base URL' }));

    expect(baseUrl).toBeEnabled();
    expect(baseUrl).toHaveValue('');
    await fireEvent.input(baseUrl, { target: { value: 'https://custom-claude.example.com' } });
    expect(baseUrl).toHaveValue('https://custom-claude.example.com');
  });

  it('resolves a saved non-custom Claude Base URL during initialization', async () => {
    mocks.invoke.mockImplementation((command: string) => {
      if (command === 'get_sub2api_config_state') {
        return Promise.resolve({
          configured: true,
          baseUrl: 'https://previous.example.com',
          codexProvider: '',
          customBaseUrl: false,
          email: 'admin@example.com',
          upstream: 'claude',
        });
      }
      if (command === 'resolve_sub2api_claude_base_url') {
        return Promise.resolve('https://resolved-claude.example.com');
      }
      return Promise.reject(new Error(`unexpected command ${command}`));
    });
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('resolve_sub2api_claude_base_url'),
    );
    await fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));

    expect(screen.getByRole('switch', { name: 'Use custom Base URL' })).not.toBeChecked();
    expect(screen.getByLabelText('Base URL')).toBeDisabled();
    await waitFor(() =>
      expect(screen.getByLabelText('Base URL')).toHaveValue('https://resolved-claude.example.com'),
    );
  });

  it('restores a saved account while keeping new upstream drafts separate', async () => {
    mocks.invoke.mockImplementation((command: string) => {
      if (command === 'get_sub2api_config_state') {
        return Promise.resolve({
          configured: true,
          baseUrl: 'https://claude.example.com',
          codexProvider: '',
          customBaseUrl: true,
          email: 'old@example.com',
          upstream: 'claude',
        });
      }
      if (command === 'resolve_sub2api_codex_provider') {
        return Promise.resolve('https://resolved.example.com');
      }
      if (command === 'resolve_sub2api_claude_base_url') {
        return Promise.resolve('https://resolved-claude.example.com');
      }
      if (command === 'save_sub2api_config') {
        return Promise.resolve({
          configured: true,
          baseUrl: 'https://new-codex.example.com',
          codexProvider: '',
          customBaseUrl: true,
          email: 'new@example.com',
          upstream: 'codex',
        });
      }
      return Promise.reject(new Error(`unexpected command ${command}`));
    });
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('old@example.com');
    await fireEvent.click(screen.getByRole('button', { name: 'Edit' }));
    expect(screen.getByLabelText('Sub2API administrator email')).toHaveValue('old@example.com');
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveAttribute(
      'placeholder',
      'Leave blank to keep saved password',
    );

    await fireEvent.click(screen.getByRole('radio', { name: 'Codex' }));

    expect(screen.getByLabelText('Sub2API administrator email')).toHaveValue('');
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveValue('');
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveAttribute(
      'placeholder',
      'Password',
    );
    expect(screen.getByRole('switch', { name: 'Use custom Base URL' })).not.toBeChecked();
    expect(screen.getByLabelText('Base URL')).toHaveValue('');
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();

    await fireEvent.click(screen.getByRole('switch', { name: 'Use custom Base URL' }));
    await fireEvent.input(screen.getByLabelText('Base URL'), {
      target: { value: 'https://new-codex.example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator email'), {
      target: { value: 'new@example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator password'), {
      target: { value: 'new-password' },
    });
    await fireEvent.click(screen.getByRole('radio', { name: 'Claude' }));

    expect(screen.getByLabelText('Sub2API administrator email')).toHaveValue('old@example.com');
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveValue('');
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveAttribute(
      'placeholder',
      'Leave blank to keep saved password',
    );
    expect(screen.getByRole('switch', { name: 'Use custom Base URL' })).toBeChecked();
    expect(screen.getByLabelText('Base URL')).toHaveValue('https://claude.example.com');
    expect(screen.getByRole('button', { name: 'Save' })).toBeEnabled();

    await fireEvent.click(screen.getByRole('radio', { name: 'Codex' }));

    expect(screen.getByRole('switch', { name: 'Use custom Base URL' })).toBeChecked();
    expect(screen.getByLabelText('Base URL')).toHaveValue('https://new-codex.example.com');
    expect(screen.getByLabelText('Sub2API administrator email')).toHaveValue('new@example.com');
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveValue('new-password');
    expect(screen.getByRole('button', { name: 'Save' })).toBeEnabled();
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('save_sub2api_config', {
        providerId: 'sub2api@2',
        config: {
          baseUrl: 'https://new-codex.example.com',
          codexProvider: '',
          customBaseUrl: true,
          email: 'new@example.com',
          password: 'new-password',
          upstream: 'codex',
        },
      }),
    );
  });

  it('keeps unsaved fields in independent drafts for a new connection', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await fireEvent.click(screen.getByRole('switch', { name: 'Use custom Base URL' }));
    await fireEvent.input(screen.getByLabelText('Base URL'), {
      target: { value: 'https://draft-codex.example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator email'), {
      target: { value: 'codex-draft@example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator password'), {
      target: { value: 'codex-draft-password' },
    });

    await fireEvent.click(screen.getByRole('radio', { name: 'Claude' }));

    expect(screen.getByRole('switch', { name: 'Use custom Base URL' })).not.toBeChecked();
    expect(screen.getByLabelText('Sub2API administrator email')).toHaveValue('');
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveValue('');
    await fireEvent.input(screen.getByLabelText('Sub2API administrator email'), {
      target: { value: 'claude-draft@example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator password'), {
      target: { value: 'claude-draft-password' },
    });

    await fireEvent.click(screen.getByRole('radio', { name: 'Codex' }));

    expect(screen.getByRole('switch', { name: 'Use custom Base URL' })).toBeChecked();
    expect(screen.getByLabelText('Base URL')).toHaveValue('https://draft-codex.example.com');
    expect(screen.getByLabelText('Sub2API administrator email')).toHaveValue(
      'codex-draft@example.com',
    );
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveValue(
      'codex-draft-password',
    );

    await fireEvent.click(screen.getByRole('radio', { name: 'Claude' }));

    expect(screen.getByLabelText('Sub2API administrator email')).toHaveValue(
      'claude-draft@example.com',
    );
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveValue(
      'claude-draft-password',
    );
  });

  it('resolves an exact Codex provider and keeps Base URL read-only by default', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    const baseUrl = screen.getByLabelText('Base URL');
    const provider = screen.getByLabelText('Codex provider or profile');

    expect(baseUrl).toBeDisabled();
    expect(screen.getByRole('switch', { name: 'Use custom Base URL' })).not.toBeChecked();
    await fireEvent.input(provider, { target: { value: 'ShareCoder' } });
    expect(mocks.invoke).not.toHaveBeenCalledWith(
      'resolve_sub2api_codex_provider',
      expect.anything(),
    );

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('resolve_sub2api_codex_provider', {
        provider: 'ShareCoder',
      }),
    );
    await waitFor(() => expect(baseUrl).toHaveValue('https://resolved.example.com'));
    expect(baseUrl).toBeDisabled();
  });

  it('clears and disables Provider when custom Base URL is enabled', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    const provider = screen.getByLabelText('Codex provider or profile');
    await fireEvent.input(provider, { target: { value: 'ShareCoder' } });
    await waitFor(() =>
      expect(screen.getByLabelText('Base URL')).toHaveValue('https://resolved.example.com'),
    );

    await fireEvent.click(screen.getByRole('switch', { name: 'Use custom Base URL' }));

    expect(provider).toBeDisabled();
    expect(provider).toHaveValue('');
    expect(screen.getByLabelText('Base URL')).toBeEnabled();
    expect(screen.getByLabelText('Base URL')).toHaveValue('');
  });

  it('registers Enter only while the Save button is enabled', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));

    const email = screen.getByLabelText('Sub2API administrator email');
    await fireEvent.keyDown(email, { key: 'Enter' });
    expect(mocks.invoke).not.toHaveBeenCalledWith('save_sub2api_config', expect.anything());

    await fireEvent.input(screen.getByLabelText('Codex provider or profile'), {
      target: { value: 'shortcut' },
    });
    await waitFor(() =>
      expect(screen.getByLabelText('Base URL')).toHaveValue('https://resolved.example.com'),
    );
    await fireEvent.input(email, {
      target: { value: 'shortcut@example.com' },
    });
    const password = screen.getByLabelText('Sub2API administrator password');
    await fireEvent.input(password, {
      target: { value: 'shortcut-password' },
    });
    const saveButton = screen.getByRole('button', { name: 'Save' });
    expect(saveButton).toHaveAttribute('aria-keyshortcuts', 'Enter');
    await fireEvent.keyDown(screen.getByRole('switch', { name: 'Use custom Base URL' }), {
      key: 'Enter',
    });
    expect(mocks.invoke).not.toHaveBeenCalledWith('save_sub2api_config', expect.anything());
    await fireEvent.keyDown(password, { key: 'Enter' });

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('save_sub2api_config', {
        providerId: 'sub2api@2',
        config: {
          baseUrl: 'https://resolved.example.com',
          codexProvider: 'shortcut',
          customBaseUrl: false,
          email: 'shortcut@example.com',
          password: 'shortcut-password',
          upstream: 'codex',
        },
      }),
    );
    expect(screen.queryByRole('button', { name: 'Save' })).not.toBeInTheDocument();
  });

  it('confirms a save before backend persistence finishes', async () => {
    let resolveSave!: (value: {
      configured: boolean;
      baseUrl: string;
      codexProvider: string;
      customBaseUrl: boolean;
      email: string;
      upstream: string;
    }) => void;
    const pendingSave = new Promise<{
      configured: boolean;
      baseUrl: string;
      codexProvider: string;
      customBaseUrl: boolean;
      email: string;
      upstream: string;
    }>((resolve) => {
      resolveSave = resolve;
    });
    mocks.invoke.mockImplementation((command: string) => {
      if (command === 'get_sub2api_config_state') {
        return Promise.resolve({
          configured: false,
          baseUrl: '',
          codexProvider: '',
          customBaseUrl: false,
          email: '',
          upstream: 'codex',
        });
      }
      if (command === 'save_sub2api_config') return pendingSave;
      return Promise.reject(new Error(`unexpected command ${command}`));
    });
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await fireEvent.click(screen.getByRole('switch', { name: 'Use custom Base URL' }));
    await fireEvent.input(screen.getByLabelText('Base URL'), {
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
      codexProvider: '',
      customBaseUrl: true,
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
        return Promise.resolve({
          configured: false,
          baseUrl: '',
          codexProvider: '',
          customBaseUrl: false,
          email: '',
          upstream: 'codex',
        });
      }
      if (command === 'save_sub2api_config') return pendingSave;
      return Promise.reject(new Error(`unexpected command ${command}`));
    });
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await fireEvent.click(screen.getByRole('switch', { name: 'Use custom Base URL' }));
    await fireEvent.input(screen.getByLabelText('Base URL'), {
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
    expect(screen.getByLabelText('Base URL')).toHaveValue('https://failed.example.com');
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
          codexProvider: '',
          customBaseUrl: true,
          email: 'admin@example.com',
          upstream: 'codex',
        });
      }
      if (command === 'clear_sub2api_config') {
        return Promise.resolve({
          configured: false,
          baseUrl: '',
          codexProvider: '',
          customBaseUrl: false,
          email: '',
          upstream: 'codex',
        });
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
    expect(screen.getByText('Sub2API')).toBeInTheDocument();
  });

  it('deletes an item from the separate bottom action row', async () => {
    const onRemove = vi.fn();
    render(Sub2ApiConfigSection, { providerId: 'sub2api@3', onRemove });
    await screen.findByRole('region', { name: 'Connection' });
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
