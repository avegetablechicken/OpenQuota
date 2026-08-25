import { fireEvent } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { saveShortcut } from './saveShortcut';

describe('save shortcut', () => {
  afterEach(() => {
    document.body.replaceChildren();
    vi.restoreAllMocks();
  });

  it('uses Command+S on macOS and unregisters when disabled', async () => {
    vi.spyOn(window.navigator, 'userAgent', 'get').mockReturnValue(
      'Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)',
    );
    const button = document.createElement('button');
    const click = vi.spyOn(button, 'click');
    document.body.append(button);
    const action = saveShortcut(button, true);

    expect(button).toHaveAttribute('aria-keyshortcuts', 'Meta+S');
    await fireEvent.keyDown(document, { key: 's', metaKey: true });
    await fireEvent.keyDown(document, { key: 's', ctrlKey: true });
    expect(click).toHaveBeenCalledOnce();

    action.update(false);
    expect(button).not.toHaveAttribute('aria-keyshortcuts');
    await fireEvent.keyDown(document, { key: 's', metaKey: true });
    expect(click).toHaveBeenCalledOnce();
    action.destroy();
  });

  it('uses Ctrl+S elsewhere and unregisters when destroyed', async () => {
    vi.spyOn(window.navigator, 'userAgent', 'get').mockReturnValue(
      'Mozilla/5.0 (Windows NT 10.0; Win64; x64)',
    );
    const button = document.createElement('button');
    const click = vi.spyOn(button, 'click');
    document.body.append(button);
    const action = saveShortcut(button, true);

    expect(button).toHaveAttribute('aria-keyshortcuts', 'Control+S');
    await fireEvent.keyDown(document, { key: 's', ctrlKey: true });
    await fireEvent.keyDown(document, { key: 's', ctrlKey: true, shiftKey: true });
    expect(click).toHaveBeenCalledOnce();

    action.destroy();
    expect(button).not.toHaveAttribute('aria-keyshortcuts');
    await fireEvent.keyDown(document, { key: 's', ctrlKey: true });
    expect(click).toHaveBeenCalledOnce();
  });
});
