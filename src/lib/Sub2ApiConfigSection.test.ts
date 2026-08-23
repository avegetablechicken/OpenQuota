import { cleanup, fireEvent, render, screen, waitFor } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import Sub2ApiConfigSection from './Sub2ApiConfigSection.svelte';

const mocks = vi.hoisted(() => ({ invoke: vi.fn() }));
vi.mock('@tauri-apps/api/core', () => ({ invoke: mocks.invoke }));

describe('Sub2ApiConfigSection', () => {
  beforeEach(() => {
    mocks.invoke.mockReset().mockImplementation((command: string) => {
      if (command === 'get_sub2api_config_state') {
        return Promise.resolve({ configured: false, baseUrl: '', email: '' });
      }
      if (command === 'save_sub2api_config') {
        return Promise.resolve({
          configured: true,
          baseUrl: 'https://sub2api.example.com',
          email: 'admin@example.com',
        });
      }
      if (command === 'delete_sub2api_config') {
        return Promise.resolve({ configured: false, baseUrl: '', email: '' });
      }
      return Promise.reject(new Error(`unexpected command ${command}`));
    });
  });

  afterEach(cleanup);

  it('saves a Sub2API administrator login without retaining the password in the UI', async () => {
    render(Sub2ApiConfigSection);
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
    await fireEvent.click(screen.getByRole('button', { name: 'Save' }));

    await waitFor(() =>
      expect(mocks.invoke).toHaveBeenCalledWith('save_sub2api_config', {
        config: {
          baseUrl: 'https://sub2api.example.com',
          email: 'admin@example.com',
          password: 'secret-password',
        },
      }),
    );
    expect(screen.queryByDisplayValue('secret-password')).not.toBeInTheDocument();
    expect(screen.getByText('admin@example.com')).toBeInTheDocument();
  });

  it('removes a saved connection only after confirmation', async () => {
    mocks.invoke.mockImplementation((command: string) => {
      if (command === 'get_sub2api_config_state') {
        return Promise.resolve({
          configured: true,
          baseUrl: 'https://sub2api.example.com',
          email: 'admin@example.com',
        });
      }
      if (command === 'delete_sub2api_config') {
        return Promise.resolve({ configured: false, baseUrl: '', email: '' });
      }
      return Promise.reject(new Error(`unexpected command ${command}`));
    });
    render(Sub2ApiConfigSection);
    await screen.findByText('admin@example.com');
    await fireEvent.click(screen.getByRole('button', { name: 'Edit' }));
    await fireEvent.click(screen.getByRole('button', { name: 'Remove Sub2API connection' }));
    expect(mocks.invoke).not.toHaveBeenCalledWith('delete_sub2api_config');
    expect(
      screen.getByRole('group', { name: 'Remove Sub2API connection?' }),
    ).toHaveAccessibleDescription('The saved login will be removed from secure storage.');
    await fireEvent.click(screen.getByRole('button', { name: 'Remove' }));
    await waitFor(() => expect(mocks.invoke).toHaveBeenCalledWith('delete_sub2api_config'));
    expect(screen.getByText('Not configured')).toBeInTheDocument();
  });
});
