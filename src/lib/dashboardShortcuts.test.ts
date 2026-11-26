import { afterEach, describe, expect, it } from 'vitest';
import { dashboardKeyshortcuts, dashboardShortcut, stepCycle } from './dashboardShortcuts';

function key(target: EventTarget, init: KeyboardEventInit) {
  const event = new KeyboardEvent('keydown', { bubbles: true, cancelable: true, ...init });
  target.dispatchEvent(event);
  return event;
}

const arrows = { altKey: true, metaKey: true };

describe('dashboard shortcuts', () => {
  afterEach(() => {
    document.body.replaceChildren();
  });

  it('cycles spend periods and usage scopes with Option-Command arrows', () => {
    expect(dashboardShortcut(key(document.body, { ...arrows, key: 'ArrowRight' }))).toEqual({
      kind: 'period',
      direction: 1,
    });
    expect(dashboardShortcut(key(document.body, { ...arrows, key: 'ArrowLeft' }))).toEqual({
      kind: 'period',
      direction: -1,
    });
    expect(dashboardShortcut(key(document.body, { ...arrows, key: 'ArrowDown' }))).toEqual({
      kind: 'view',
      direction: 1,
    });
    expect(dashboardShortcut(key(document.body, { ...arrows, key: 'ArrowUp' }))).toEqual({
      kind: 'view',
      direction: -1,
    });
    expect(stepCycle(['today', 'yesterday', 'last30Days'], 'today', 1)).toBe('yesterday');
    expect(stepCycle(['today', 'yesterday', 'last30Days'], 'today', -1)).toBe('last30Days');
    expect(stepCycle(['all', 'localDevice', 'account'], 'account', 1)).toBe('all');
    expect(stepCycle(['all', 'localDevice', 'account'], 'all', -1)).toBe('account');
  });

  it('requires Option and Command together, and leaves other keys alone', () => {
    for (const init of [
      { key: 'ArrowRight', altKey: true },
      { key: 'ArrowRight', metaKey: true },
      { key: 'ArrowDown', altKey: true, metaKey: true, shiftKey: true },
      { key: 'ArrowDown', altKey: true, metaKey: true, ctrlKey: true },
      { key: 'ArrowLeft', altKey: true, metaKey: true, repeat: true },
      { key: 'Enter', altKey: true, metaKey: true },
    ]) {
      expect(dashboardShortcut(key(document.body, init))).toBeNull();
    }
    const used = key(document.body, { ...arrows, key: 'ArrowRight' });
    used.preventDefault();
    expect(dashboardShortcut(used)).toBeNull();
  });

  it('leaves the keys to text fields and open dialogs', () => {
    document.body.innerHTML =
      '<input id="name" /><div role="dialog"><button id="ok">Ok</button></div>';
    expect(
      dashboardShortcut(key(document.getElementById('name')!, { ...arrows, key: 'ArrowLeft' })),
    ).toBeNull();
    expect(dashboardShortcut(key(document.body, { ...arrows, key: 'ArrowDown' }))).toBeNull();
    document.body.innerHTML = '<div role="alertdialog"></div>';
    expect(dashboardShortcut(key(document.body, { ...arrows, key: 'ArrowUp' }))).toBeNull();
  });
  it('announces the shortcuts only on macOS, where they are registered', () => {
    expect(dashboardKeyshortcuts('macos')).toEqual({
      period: 'Alt+Meta+ArrowLeft Alt+Meta+ArrowRight',
      view: 'Alt+Meta+ArrowUp Alt+Meta+ArrowDown',
    });
    expect(dashboardKeyshortcuts('windows')).toBeNull();
    expect(dashboardKeyshortcuts('linux')).toBeNull();
  });
});
