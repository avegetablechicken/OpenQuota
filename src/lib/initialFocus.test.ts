import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { dropFocusUntilInteraction } from './initialFocus';

describe('focus in a freshly shown panel', () => {
  let guard: ReturnType<typeof dropFocusUntilInteraction>;
  const button = () => document.querySelector<HTMLButtonElement>('#metric')!;

  beforeEach(() => {
    document.body.innerHTML = `<button id="metric">Tokens</button><button id="next">Next</button>`;
    guard = dropFocusUntilInteraction();
  });

  afterEach(() => {
    guard.dispose();
    document.body.replaceChildren();
  });

  it('drops focus given before the user has done anything', () => {
    button().focus();
    expect(document.activeElement).toBe(document.body);
  });

  it('keeps focus once the user presses a key', () => {
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab' }));
    button().focus();
    expect(document.activeElement).toBe(button());
  });

  it('keeps focus once the user clicks', () => {
    window.dispatchEvent(new Event('pointerdown'));
    button().focus();
    expect(document.activeElement).toBe(button());
  });

  it('keeps focus once a click arrives without a pointer down', () => {
    button().click();
    button().focus();
    expect(document.activeElement).toBe(button());
  });

  it('drops focus again after the panel is hidden', () => {
    window.dispatchEvent(new KeyboardEvent('keydown', { key: 'Tab' }));
    button().focus();
    guard.reset();
    expect(document.activeElement).toBe(document.body);
    document.querySelector<HTMLButtonElement>('#next')!.focus();
    expect(document.activeElement).toBe(document.body);
  });
});
