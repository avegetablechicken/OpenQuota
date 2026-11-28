import { describe, expect, it } from 'vitest';
import { globalShortcutKey } from './globalShortcutKey';

describe('global shortcut key', () => {
  it('keeps the short names for letters and digits', () => {
    expect(globalShortcutKey('KeyA')).toBe('A');
    expect(globalShortcutKey('Digit7')).toBe('7');
  });

  it('uses physical code names the backend parser accepts', () => {
    for (const code of [
      'Space',
      'Semicolon',
      'Equal',
      'Comma',
      'F1',
      'F24',
      'Numpad5',
      'ArrowUp',
    ]) {
      expect(globalShortcutKey(code)).toBe(code);
    }
  });

  it('rejects keys the backend parser cannot register', () => {
    for (const code of ['', 'IntlBackslash', 'ContextMenu', 'F25', 'Unidentified', 'ShiftLeft']) {
      expect(globalShortcutKey(code)).toBeNull();
    }
  });
});
