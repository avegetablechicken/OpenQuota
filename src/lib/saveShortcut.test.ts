import { fireEvent } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { saveShortcut } from './saveShortcut';

describe('save shortcut', () => {
  afterEach(() => {
    document.body.replaceChildren();
    vi.restoreAllMocks();
  });

  it('uses Enter from text fields and unregisters when disabled', async () => {
    const button = document.createElement('button');
    const input = document.createElement('input');
    input.type = 'password';
    const click = vi.spyOn(button, 'click');
    document.body.append(input, button);
    const action = saveShortcut(button, true);

    expect(button).toHaveAttribute('aria-keyshortcuts', 'Enter');
    await fireEvent.keyDown(input, { key: 'Enter' });
    await fireEvent.keyDown(input, { key: 'Enter', metaKey: true });
    expect(click).toHaveBeenCalledOnce();

    action.update(false);
    expect(button).not.toHaveAttribute('aria-keyshortcuts');
    await fireEvent.keyDown(input, { key: 'Enter' });
    expect(click).toHaveBeenCalledOnce();
    action.destroy();
  });

  it('preserves Enter on controls that already own it', async () => {
    const save = document.createElement('button');
    const otherButton = document.createElement('button');
    const checkbox = document.createElement('input');
    const textarea = document.createElement('textarea');
    checkbox.type = 'checkbox';
    const click = vi.spyOn(save, 'click');
    document.body.append(otherButton, checkbox, textarea, save);
    const action = saveShortcut(save, true);

    await fireEvent.keyDown(otherButton, { key: 'Enter' });
    await fireEvent.keyDown(checkbox, { key: 'Enter' });
    await fireEvent.keyDown(textarea, { key: 'Enter' });
    expect(click).not.toHaveBeenCalled();

    action.destroy();
    expect(save).not.toHaveAttribute('aria-keyshortcuts');
  });
});
