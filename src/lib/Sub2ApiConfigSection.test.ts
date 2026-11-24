import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { get } from 'svelte/store';
import Sub2ApiConfigSection from './Sub2ApiConfigSection.svelte';
import { providerIconPath } from './providerIconPaths';
import { selectOption } from '../test/selectOption';
import {
  forgetSub2ApiUpstream,
  rememberSub2ApiUpstream,
  sub2ApiUpstreams,
} from './sub2ApiUpstreams';

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));

async function chooseAdministrator() {
  await selectOption(screen.getByRole('combobox', { name: 'Track' }), 'Upstream accounts (admin)');
}

async function chooseUpstream(name: 'Codex' | 'Claude') {
  await selectOption(screen.getByRole('combobox', { name: 'Upstream' }), name);
}

// Without a custom Base URL, the resolved address is shown as a note, not a field.
function resolvedBaseUrl() {
  return screen.getByRole('status', { name: 'Resolved Base URL' });
}

// Whether the saved connection is configured, as the header's status dot shows.
function statusDot() {
  return document.querySelector('.sub2api-config-summary i')!;
}

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
      if (command === 'resolve_sub2api_claude_provider') {
        return Promise.resolve('https://resolved-claude.example.com');
      }
      if (command === 'resolve_sub2api_codex_provider') {
        return Promise.resolve('https://resolved.example.com');
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
    expect(within(connections).getByText('Provider or custom Base URL')).toBeInTheDocument();
    await waitFor(() => expect(get(sub2ApiUpstreams)['sub2api@2']).toBeUndefined());
  });

  it('saves a named subscription using the provider key without collecting a secret', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await fireEvent.click(await screen.findByRole('button', { name: 'Add' }));
    await fireEvent.input(screen.getByLabelText('Codex provider or profile'), {
      target: { value: 'ShareCoder' },
    });
    await waitFor(() => expect(screen.getByRole('button', { name: 'Save' })).toBeEnabled());
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    const call = mocks.invoke.mock.calls.find(([cmd]) => cmd === 'save_sub2api_config');
    expect(call?.[1].config).toMatchObject({
      mode: 'subscription',
      useProviderKey: true,
      codexProvider: 'ShareCoder',
    });
    expect(call?.[1].config).not.toHaveProperty('apiKey');
  });

  it('defaults to the user subscription and saves only its key', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await fireEvent.click(await screen.findByRole('button', { name: 'Add' }));
    expect(screen.getByRole('combobox', { name: 'Track' })).toHaveValue('subscription');
    expect(screen.getByRole('combobox', { name: 'Upstream' })).toHaveValue('codex');
    expect(screen.queryByLabelText('Billing method')).toBeNull();
    expect(screen.queryByLabelText('Sub2API administrator email')).toBeNull();
    await fireEvent.input(screen.getByLabelText('Codex provider or profile'), {
      target: { value: 'ShareCoder' },
    });
    await waitFor(() => expect(resolvedBaseUrl()).toHaveTextContent('resolved.example.com'));
    expect(screen.getByRole('button', { name: 'Save' })).toBeEnabled();
    expect(screen.queryByRole('switch', { name: 'Use provider API key' })).toBeNull();
    const key = screen.getByLabelText('Sub2API subscription API key');
    expect(key).toHaveAttribute('type', 'password');
    await fireEvent.input(key, { target: { value: 'test-key' } });
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('save_sub2api_config', {
        providerId: 'sub2api@2',
        config: expect.objectContaining({
          mode: 'subscription',
          apiKey: 'test-key',
          codexProvider: 'ShareCoder',
          email: '',
          password: '',
        }),
      }),
    );
    expect(screen.queryByDisplayValue('test-key')).toBeNull();
  });

  it.each([false, true])(
    'keeps a saved override unless explicitly cleared (clear=%s)',
    async (clear) => {
      const saved = {
        configured: true,
        mode: 'subscription',
        upstream: 'codex',
        codexProvider: 'relay',
        baseUrl: 'https://relay.example.com',
        customBaseUrl: false,
        email: '',

        useProviderKey: false,
      };
      mocks.invoke.mockResolvedValue(saved);
      render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
      await fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
      const key = screen.getByLabelText('Sub2API subscription API key');
      expect(key).toHaveValue('');
      expect(key).toHaveAttribute('placeholder', 'Unchanged');
      if (clear) await fireEvent.click(screen.getByRole('button', { name: 'Clear saved API key' }));
      await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
      const call = mocks.invoke.mock.calls.find(([command]) => command === 'save_sub2api_config');
      expect(call?.[1].config.useProviderKey ?? false).toBe(clear);
    },
  );

  it('keeps a saved API key out of the administrator form and requires a new key for a different endpoint', async () => {
    mocks.invoke.mockResolvedValue({
      configured: true,
      mode: 'subscription',
      upstream: 'codex',
      customBaseUrl: true,
      baseUrl: 'https://quota.example.com',
      codexProvider: '',
      email: '',
    });
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    const key = screen.getByLabelText('Sub2API subscription API key');
    expect(key).toHaveValue('');
    expect(key).toHaveAttribute('placeholder', 'Unchanged');
    expect(screen.getByRole('button', { name: 'Save' })).toBeEnabled();
    await chooseAdministrator();
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveAttribute(
      'placeholder',
      'Password',
    );
    expect(screen.queryByLabelText('Sub2API subscription API key')).toBeNull();
    await selectOption(screen.getByRole('combobox', { name: 'Track' }), 'User subscription');
    await fireEvent.input(screen.getByLabelText('Base URL'), {
      target: { value: 'https://other.example.com' },
    });
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
  });

  it('keeps the selected upstream across saved tracks while restoring their endpoints and credentials', async () => {
    const subscription = {
      configured: true,
      mode: 'subscription',
      upstream: 'claude',
      customBaseUrl: true,
      baseUrl: 'https://user.example.com',
      codexProvider: '',
      email: '',
    };
    const admin = {
      configured: true,
      mode: 'admin',
      upstream: 'codex',
      customBaseUrl: true,
      baseUrl: 'https://admin.example.com',
      codexProvider: '',
      email: 'admin@example.com',
    };
    mocks.invoke.mockImplementation(async (cmd, args) =>
      cmd === 'save_sub2api_config'
        ? { ...admin, upstream: args.config.upstream, otherMode: subscription }
        : { ...subscription, otherMode: admin },
    );
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    await chooseAdministrator();
    expect(screen.getByLabelText('Base URL')).toHaveValue(admin.baseUrl);
    expect(screen.getByLabelText('Upstream')).toHaveValue('claude');
    expect(screen.queryByLabelText('Billing method')).toBeNull();
    expect(screen.getByLabelText('Sub2API administrator email')).toHaveValue(admin.email);
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveAttribute(
      'placeholder',
      'Unchanged',
    );
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('save_sub2api_config', {
        providerId: 'sub2api@2',
        config: expect.objectContaining({
          mode: 'admin',
          upstream: 'claude',
          password: '',
          baseUrl: admin.baseUrl,
        }),
      }),
    );
    await fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    await chooseUpstream('Codex');
    await selectOption(screen.getByLabelText('Track'), 'User subscription');
    expect(screen.getByLabelText('Upstream')).toHaveValue('codex');
    expect(screen.getByLabelText('Base URL')).toHaveValue(subscription.baseUrl);
    expect(screen.getByLabelText('Sub2API subscription API key')).toHaveAttribute(
      'placeholder',
      'Unchanged',
    );
    expect(screen.getByRole('button', { name: 'Save' })).toBeEnabled();
  });

  it('shows the upstream and email being edited in the header', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    const upstreamLayer = () =>
      document.querySelector('.sub2api-config-summary [data-icon-layer="upstream"] path');
    expect(upstreamLayer()).toBeNull();
    expect(screen.queryByLabelText('Sub2API administrator email')).not.toBeInTheDocument();

    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await chooseAdministrator();
    expect(upstreamLayer()).toHaveAttribute('d', providerIconPath('codex'));
    expect(screen.getByText('Sub2API · Codex')).toBeInTheDocument();
    await chooseUpstream('Claude');
    expect(upstreamLayer()).toHaveAttribute('d', providerIconPath('claude'));
    expect(screen.getByText('Sub2API · Claude')).toBeInTheDocument();
    expect(screen.getByText('Base URL and administrator login')).toBeInTheDocument();
    await fireEvent.input(screen.getByLabelText('Sub2API administrator email'), {
      target: { value: 'draft@example.com' },
    });
    expect(screen.getByText('draft@example.com')).toBeInTheDocument();
    expect(statusDot()).toHaveClass('missing');

    // Closed without saving, the header shows the saved connection again.
    await fireEvent.click(screen.getByRole('button', { name: 'Done' }));
    expect(upstreamLayer()).toBeNull();
    expect(screen.getByText('Not configured')).toBeInTheDocument();
  });

  it('keeps the upstream but leaves the other track’s address empty on first use', async () => {
    const original = mocks.invoke.getMockImplementation()!;
    mocks.invoke.mockImplementation(async (cmd, args) =>
      cmd === 'get_sub2api_config_state'
        ? {
            configured: true,
            mode: 'admin',
            upstream: 'claude',
            providerSource: 'codex',
            codexProvider: 'relay',
            baseUrl: 'https://resolved.example.com',
            customBaseUrl: false,
            email: 'admin@example.com',
          }
        : original(cmd, args),
    );
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    await selectOption(screen.getByLabelText('Track'), 'User subscription');
    expect(screen.getByLabelText('Upstream')).toHaveValue('claude');
    expect(screen.getByLabelText('Claude settings provider')).toHaveValue('');
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
    await chooseAdministrator();
    expect(screen.getByLabelText('Codex provider or profile')).toHaveValue('relay');
  });

  it('preserves unsaved upstream and independent track drafts through a late initial read', async () => {
    const original = mocks.invoke.getMockImplementation()!;
    let finishRead!: (value: unknown) => void;
    const initialRead = new Promise((resolve) => {
      finishRead = resolve;
    });
    mocks.invoke.mockImplementation((cmd, args) =>
      cmd === 'get_sub2api_config_state' ? initialRead : original(cmd, args),
    );
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await fireEvent.click(await screen.findByRole('button', { name: 'Add' }));
    await chooseUpstream('Claude');
    await fireEvent.click(screen.getByLabelText('Use custom Base URL'));
    await fireEvent.input(screen.getByLabelText('Base URL'), {
      target: { value: 'https://user.example' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API subscription API key'), {
      target: { value: 'draft-key' },
    });
    await chooseAdministrator();
    expect(screen.getByLabelText('Upstream')).toHaveValue('claude');
    await fireEvent.click(screen.getByLabelText('Use custom Base URL'));
    expect(screen.getByLabelText('Base URL')).toHaveValue('');
    await fireEvent.input(screen.getByLabelText('Base URL'), {
      target: { value: 'https://admin.example' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator email'), {
      target: { value: 'draft@example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator password'), {
      target: { value: 'draft-password' },
    });
    await chooseUpstream('Codex');
    finishRead({
      configured: false,
      mode: 'subscription',
      upstream: 'codex',
      codexProvider: '',
      baseUrl: '',
      customBaseUrl: false,
      email: '',
    });
    await initialRead;
    await selectOption(screen.getByLabelText('Track'), 'User subscription');
    expect(screen.getByLabelText('Upstream')).toHaveValue('codex');
    expect(screen.getByLabelText('Base URL')).toHaveValue('https://user.example');
    expect(screen.getByLabelText('Sub2API subscription API key')).toHaveValue('draft-key');
    await chooseAdministrator();
    expect(screen.getByLabelText('Upstream')).toHaveValue('codex');
    expect(screen.getByLabelText('Base URL')).toHaveValue('https://admin.example');
    expect(screen.getByLabelText('Sub2API administrator email')).toHaveValue('draft@example.com');
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveValue('draft-password');
    expect(mocks.invoke).not.toHaveBeenCalledWith('save_sub2api_config', expect.anything());
  });

  it('saves a Claude upstream login without retaining the password in the UI', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api' });
    expect(await screen.findByRole('region', { name: 'Connection' })).toBeInTheDocument();
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await chooseAdministrator();
    await chooseUpstream('Claude');
    expect(screen.queryByLabelText('Base URL')).not.toBeInTheDocument();
    expect(resolvedBaseUrl()).not.toHaveTextContent('https://');
    await fireEvent.input(screen.getByLabelText('Claude settings provider'), {
      target: { value: 'work' },
    });
    await waitFor(() =>
      expect(resolvedBaseUrl()).toHaveTextContent('https://resolved-claude.example.com'),
    );
    expect(mocks.invoke).toHaveBeenCalledWith('resolve_sub2api_claude_provider', {
      provider: 'work',
    });
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
          mode: 'admin',
          baseUrl: 'https://resolved-claude.example.com',
          codexProvider: 'work',
          customBaseUrl: false,
          email: 'admin@example.com',
          password: 'secret-password',
          upstream: 'claude',
        },
      }),
    );
    expect(screen.queryByDisplayValue('secret-password')).not.toBeInTheDocument();
    expect(screen.getByText('admin@example.com')).toBeInTheDocument();
    expect(screen.getByText('Sub2API · Claude')).toBeInTheDocument();
  });

  it('requires a Claude provider or a manually entered custom URL', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await chooseAdministrator();
    await chooseUpstream('Claude');
    await fireEvent.input(screen.getByLabelText('Sub2API administrator email'), {
      target: { value: 'admin@example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator password'), {
      target: { value: 'secret' },
    });
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
    await fireEvent.click(screen.getByRole('switch', { name: 'Use custom Base URL' }));
    expect(screen.queryByLabelText('Claude settings provider')).not.toBeInTheDocument();
    await fireEvent.input(screen.getByLabelText('Base URL'), { target: { value: '   ' } });
    expect(screen.getByRole('button', { name: 'Save' })).toBeDisabled();
    await fireEvent.input(screen.getByLabelText('Base URL'), {
      target: { value: 'https://custom.example.com' },
    });
    expect(screen.getByRole('button', { name: 'Save' })).toBeEnabled();
    expect(
      mocks.invoke.mock.calls.some(
        ([command]) =>
          command === 'resolve_sub2api_claude_provider' ||
          command === 'resolve_sub2api_codex_provider',
      ),
    ).toBe(false);
  });

  it('uses the backend custom mode for a saved Claude connection', async () => {
    mocks.invoke.mockResolvedValue({
      configured: true,
      baseUrl: 'https://previous.example.com',
      codexProvider: '',
      customBaseUrl: true,
      email: 'admin@example.com',
      upstream: 'claude',
    });
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    expect(screen.getByLabelText('Base URL')).toBeEnabled();
    expect(screen.getByLabelText('Base URL')).toHaveValue('https://previous.example.com');
    expect(mocks.invoke.mock.calls.map(([command]) => command)).toEqual([
      'get_sub2api_config_state',
    ]);
  });

  it('switches the tracked upstream without losing a saved provider or requiring the password again', async () => {
    const saved = {
      configured: true,
      mode: 'admin',
      baseUrl: 'https://relay.example.com',
      codexProvider: 'work',
      customBaseUrl: false,
      email: 'admin@example.com',
      upstream: 'codex',
    };
    mocks.invoke.mockImplementation(async (command: string, args) => {
      if (command === 'get_sub2api_config_state') return saved;
      if (command === 'save_sub2api_config') return { ...saved, ...args.config };
      throw new Error('Upstream changes must keep the configured endpoint');
    });
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    await chooseUpstream('Claude');
    expect(screen.getByLabelText('Codex provider or profile')).toHaveValue('work');
    expect(resolvedBaseUrl()).toHaveTextContent(saved.baseUrl);
    expect(screen.getByLabelText('Sub2API administrator email')).toHaveValue(saved.email);
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveAttribute(
      'placeholder',
      'Unchanged',
    );
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('save_sub2api_config', {
        providerId: 'sub2api@2',
        config: expect.objectContaining({
          upstream: 'claude',
          providerSource: 'codex',
          codexProvider: 'work',
          password: '',
          baseUrl: saved.baseUrl,
        }),
      }),
    );
    await fireEvent.click(await screen.findByRole('button', { name: 'Edit' }));
    expect(screen.getByLabelText('Upstream')).toHaveValue('claude');
    expect(screen.getByLabelText('Codex provider or profile')).toHaveValue('work');
  });

  it('keeps edits to the same administrator connection across upstream changes', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await fireEvent.click(await screen.findByRole('button', { name: 'Add' }));
    await chooseAdministrator();
    await fireEvent.click(screen.getByRole('switch', { name: 'Use custom Base URL' }));
    await fireEvent.input(screen.getByLabelText('Base URL'), {
      target: { value: 'https://relay.example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator email'), {
      target: { value: 'admin@example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator password'), {
      target: { value: 'first' },
    });
    await chooseUpstream('Claude');
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveValue('first');
    await fireEvent.input(screen.getByLabelText('Sub2API administrator password'), {
      target: { value: 'edited' },
    });
    await chooseUpstream('Codex');
    expect(screen.getByRole('switch', { name: 'Use custom Base URL' })).toBeChecked();
    expect(screen.getByLabelText('Base URL')).toHaveValue('https://relay.example.com');
    expect(screen.getByLabelText('Sub2API administrator email')).toHaveValue('admin@example.com');
    expect(screen.getByLabelText('Sub2API administrator password')).toHaveValue('edited');
    expect(screen.getByRole('button', { name: 'Save' })).toBeEnabled();
  });

  it('resolves an exact Codex provider and keeps Base URL read-only by default', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await chooseAdministrator();
    const provider = screen.getByLabelText('Codex provider or profile');

    expect(screen.queryByLabelText('Base URL')).not.toBeInTheDocument();
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
    await waitFor(() =>
      expect(resolvedBaseUrl()).toHaveTextContent('https://resolved.example.com'),
    );
    expect(screen.queryByLabelText('Base URL')).not.toBeInTheDocument();
  });

  it('swaps Provider for an editable Base URL when custom Base URL is enabled', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await chooseAdministrator();
    const provider = screen.getByLabelText('Codex provider or profile');
    await fireEvent.input(provider, { target: { value: 'ShareCoder' } });
    await waitFor(() =>
      expect(resolvedBaseUrl()).toHaveTextContent('https://resolved.example.com'),
    );

    await fireEvent.click(screen.getByRole('switch', { name: 'Use custom Base URL' }));

    expect(provider).not.toBeInTheDocument();
    expect(screen.queryByRole('status', { name: 'Resolved Base URL' })).not.toBeInTheDocument();
    expect(screen.getByLabelText('Base URL')).toBeEnabled();
    expect(screen.getByLabelText('Base URL')).toHaveValue('');

    await fireEvent.click(screen.getByRole('switch', { name: 'Use custom Base URL' }));
    expect(screen.getByLabelText('Codex provider or profile')).toHaveValue('');
    expect(screen.queryByLabelText('Base URL')).not.toBeInTheDocument();
  });

  it('registers Enter only while the Save button is enabled', async () => {
    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await chooseAdministrator();

    const email = screen.getByLabelText('Sub2API administrator email');
    await fireEvent.keyDown(email, { key: 'Enter' });
    expect(mocks.invoke).not.toHaveBeenCalledWith('save_sub2api_config', expect.anything());

    await fireEvent.input(screen.getByLabelText('Codex provider or profile'), {
      target: { value: 'shortcut' },
    });
    await waitFor(() =>
      expect(resolvedBaseUrl()).toHaveTextContent('https://resolved.example.com'),
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
          mode: 'admin',
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
    await chooseAdministrator();
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
    await chooseAdministrator();
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
    expect(statusDot()).toHaveClass('missing');
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
    await fireEvent.click(screen.getByRole('button', { name: 'Clear connection' }));
    expect(screen.getByRole('group', { name: 'Clear connection?' })).toHaveAccessibleDescription(
      'The saved connections and credentials for both tracking modes will be removed. This account will remain.',
    );
    await fireEvent.click(screen.getByRole('button', { name: 'Clear' }));
    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('clear_sub2api_config', {
        providerId: 'sub2api@2',
      }),
    );
    expect(mocks.invoke).not.toHaveBeenCalledWith('delete_sub2api_config', expect.anything());
    expect(statusDot()).toHaveClass('missing');
  });

  it('deletes an item from the separate bottom action row', async () => {
    const onRemove = vi.fn();
    render(Sub2ApiConfigSection, { providerId: 'sub2api@3', onRemove });
    await screen.findByRole('region', { name: 'Connection' });
    await fireEvent.click(screen.getByRole('button', { name: 'Delete Account' }));

    expect(screen.getByRole('group', { name: 'Delete this account?' })).toHaveAccessibleDescription(
      'This empty account will be removed.',
    );
    await fireEvent.click(screen.getByRole('button', { name: 'Delete' }));

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('delete_sub2api_config', {
        providerId: 'sub2api@3',
      }),
    );
    expect(onRemove).toHaveBeenCalledOnce();
  });

  it('disables Upstream and Track selectors while saving and re-enables on settle', async () => {
    const createDeferredSave = () => {
      let resolve!: (value: {
        configured: boolean;
        baseUrl: string;
        codexProvider: string;
        customBaseUrl: boolean;
        email: string;
        upstream: string;
      }) => void;
      let reject!: (reason: Error) => void;
      const promise = new Promise<{
        configured: boolean;
        baseUrl: string;
        codexProvider: string;
        customBaseUrl: boolean;
        email: string;
        upstream: string;
      }>((res, rej) => {
        resolve = res;
        reject = rej;
      });
      return { promise, resolve, reject };
    };

    let currentSave = createDeferredSave();
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
      if (command === 'save_sub2api_config') return currentSave.promise;
      return Promise.reject(new Error(`unexpected command ${command}`));
    });

    render(Sub2ApiConfigSection, { providerId: 'sub2api@2' });
    await screen.findByText('Not configured');
    await fireEvent.click(screen.getByRole('button', { name: 'Add' }));
    await chooseAdministrator();
    await fireEvent.click(screen.getByRole('switch', { name: 'Use custom Base URL' }));
    await fireEvent.input(screen.getByLabelText('Base URL'), {
      target: { value: 'https://saving.example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator email'), {
      target: { value: 'saving@example.com' },
    });
    await fireEvent.input(screen.getByLabelText('Sub2API administrator password'), {
      target: { value: 'saving-password' },
    });

    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('save_sub2api_config', expect.anything()),
    );

    await fireEvent.click(screen.getByRole('button', { name: 'Edit' }));
    const upstreamSelect = screen.getByRole('combobox', { name: 'Upstream' });
    const trackSelect = screen.getByRole('combobox', { name: 'Track' });
    expect(upstreamSelect).toBeDisabled();
    expect(trackSelect).toBeDisabled();

    // Attempting to change either selector while saving is in flight is blocked.
    await chooseUpstream('Claude');
    await selectOption(trackSelect, 'User subscription');
    expect(upstreamSelect).toHaveValue('codex');
    expect(trackSelect).toHaveValue('admin');

    currentSave.resolve({
      configured: true,
      baseUrl: 'https://saving.example.com',
      codexProvider: '',
      customBaseUrl: true,
      email: 'saving@example.com',
      upstream: 'codex',
    });
    await waitFor(() => expect(upstreamSelect).toBeEnabled());
    expect(trackSelect).toBeEnabled();

    // Verify re-enablement on save rejection as well.
    currentSave = createDeferredSave();
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));
    expect(screen.queryByRole('combobox', { name: 'Upstream' })).not.toBeInTheDocument();

    currentSave.reject(new Error('Network timeout'));
    expect(await screen.findByRole('alert')).toHaveTextContent('Network timeout');
    expect(screen.getByRole('combobox', { name: 'Upstream' })).toBeEnabled();
    expect(screen.getByRole('combobox', { name: 'Track' })).toBeEnabled();
  });
});
