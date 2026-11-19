import { tick } from 'svelte';
import type { NativeMenuEntry } from '../lib/nativeMenu';

// Stands in for the system menu in tests: records the last menu opened, and
// `chooseMenuItem` picks from it as a click on the system menu would.
let open: { entries: NativeMenuEntry[]; at: { x: number; y: number } } | null = null;

export async function popupMenu(entries: NativeMenuEntry[], at: { x: number; y: number }) {
  open = { entries, at };
}

export function below(element: Element) {
  const bounds = element.getBoundingClientRect();
  return { x: bounds.left, y: bounds.bottom + 4 };
}

/// The menu open now, or null.
export function openMenu() {
  return open;
}

export function closeMenu() {
  open = null;
}

/// Labels of `entries`, with `-` for a separator and `✓` before a checked one.
export function menuLabels(entries = open?.entries ?? []) {
  return entries.map((entry) =>
    entry === 'separator' ? '-' : `${entry.checked ? '✓ ' : ''}${entry.text}`,
  );
}

/// Chooses the item at `path` (through submenus) in the open menu, which
/// then closes.
export async function chooseMenuItem(...path: string[]) {
  if (!open) throw new Error('No menu is open');
  let entries: NativeMenuEntry[] = open.entries;
  for (const [index, text] of path.entries()) {
    const entry: NativeMenuEntry | undefined = entries.find(
      (item) => item !== 'separator' && item.text === text,
    );
    if (!entry || entry === 'separator') {
      throw new Error(`No menu item ${text} in ${menuLabels(entries).join(', ')}`);
    }
    if (index < path.length - 1) {
      entries = entry.items ?? [];
      continue;
    }
    if (entry.enabled === false) throw new Error(`Menu item ${text} is disabled`);
    open = null;
    entry.action?.();
  }
  await tick();
}
