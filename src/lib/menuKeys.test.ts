import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { editsText, handleMenuBarKey, menuBarStep } from './menuKeys';

function key(target: Element, init: KeyboardEventInit) {
  const event = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init });
  target.dispatchEvent(event);
  return event;
}

describe('menu bar keys', () => {
  let navigating = true;
  let navigated: boolean[] = [];
  const listener = (event: KeyboardEvent) =>
    handleMenuBarKey(event, navigating, (right) => navigated.push(right));

  beforeEach(() => {
    navigating = true;
    navigated = [];
    document.body.innerHTML = `
      <button id="back">Back</button>
      <input id="search" type="text" />
      <label><input id="toggle" type="checkbox" /></label>`;
    document.addEventListener('keydown', listener);
  });

  afterEach(() => {
    document.removeEventListener('keydown', listener);
    document.body.replaceChildren();
  });

  const byId = (id: string) => document.getElementById(id)!;

  it('moves menu bar navigation with left and right', () => {
    expect(key(byId('back'), { key: 'ArrowRight' }).defaultPrevented).toBe(true);
    expect(key(document.body, { key: 'ArrowLeft' }).defaultPrevented).toBe(true);
    key(byId('toggle'), { key: 'ArrowRight' });
    expect(navigated).toEqual([true, false, true]);
  });

  it('leaves up and down to the window, as after a click', () => {
    for (const arrow of ['ArrowUp', 'ArrowDown']) {
      expect(key(byId('back'), { key: arrow }).defaultPrevented).toBe(false);
    }
    expect(navigated).toEqual([]);
  });

  it('leaves left and right in text, with modifiers or used already', () => {
    expect(key(byId('search'), { key: 'ArrowLeft' }).defaultPrevented).toBe(false);
    for (const modifier of ['metaKey', 'altKey', 'shiftKey', 'ctrlKey']) {
      expect(key(byId('back'), { key: 'ArrowRight', [modifier]: true }).defaultPrevented).toBe(
        false,
      );
    }
    const own = byId('back');
    own.addEventListener('keydown', (event) => event.preventDefault());
    key(own, { key: 'ArrowRight' });
    expect(
      menuBarStep(new KeyboardEvent('keydown', { key: 'ArrowLeft', isComposing: true }), true),
    ).toBeNull();
    expect(navigated).toEqual([]);
  });

  it('keeps arrows as they were without keyboard navigation of the menu bar', () => {
    navigating = false;
    for (const arrow of ['ArrowLeft', 'ArrowRight']) {
      expect(key(byId('back'), { key: arrow }).defaultPrevented).toBe(false);
    }
    expect(navigated).toEqual([]);
  });

  it('tells text entry from other inputs', () => {
    expect(editsText(byId('search'))).toBe(true);
    expect(editsText(byId('toggle'))).toBe(false);
    expect(editsText(null)).toBe(false);
  });
});
