import { LogicalPosition } from '@tauri-apps/api/dpi';
import { CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu } from '@tauri-apps/api/menu';

/// An entry of a menu the system draws: highlighted, checked, keyed and
/// dismissed as every other menu on the platform.
export type NativeMenuEntry =
  | 'separator'
  | {
      text: string;
      action?: () => void;
      /// Shown beside the item, as `CmdOrCtrl+,`. Only shown: the page
      /// handles the shortcut while the menu is closed.
      accelerator?: string;
      enabled?: boolean;
      /// A check mark, for a choice among the entries.
      checked?: boolean;
      items?: NativeMenuEntry[];
    };

type Closable = { close(): Promise<void> };
type Item = MenuItem | CheckMenuItem | Submenu | PredefinedMenuItem;

// The last menu's items stay alive until the next opens, so an action chosen
// as it closes still arrives.
let previous: Closable[] = [];
let activeGeneration = 0;

async function closeAll(items: Closable[]) {
  await Promise.all(
    items.map(async (item) => {
      try {
        await item.close();
      } catch {
        // Ignore close errors so one failed handle does not leak others.
      }
    }),
  );
}

async function build(
  entries: NativeMenuEntry[],
  made: Closable[],
  isCurrent: () => boolean,
): Promise<Item[]> {
  const items: Item[] = [];
  for (const entry of entries) {
    if (!isCurrent()) return items;
    if (entry === 'separator') {
      const item = await PredefinedMenuItem.new({ item: 'Separator' });
      made.push(item);
      items.push(item);
      continue;
    }
    const enabled = entry.enabled ?? true;
    const action = entry.action ? () => entry.action?.() : undefined;
    let item: Item;
    if (entry.items) {
      const subItems = await build(entry.items, made, isCurrent);
      if (!isCurrent()) return items;
      item = await Submenu.new({ text: entry.text, enabled, items: subItems });
    } else {
      item =
        entry.checked !== undefined
          ? await CheckMenuItem.new({ text: entry.text, enabled, checked: entry.checked, action })
          : await MenuItem.new({
              text: entry.text,
              enabled,
              accelerator: entry.accelerator,
              action,
            });
    }
    made.push(item);
    items.push(item);
  }
  return items;
}

/// Opens `entries` as a system menu whose top left is at `at`, in the
/// window's coordinates.
export async function popupMenu(entries: NativeMenuEntry[], at: { x: number; y: number }) {
  const generation = ++activeGeneration;
  const isCurrent = () => generation === activeGeneration;
  const stale = previous;
  previous = [];
  await closeAll(stale);
  if (!isCurrent()) return;

  const made: Closable[] = [];
  let published = false;
  try {
    const items = await build(entries, made, isCurrent);
    if (!isCurrent()) {
      await closeAll(made);
      return;
    }
    const menu = await Menu.new({ items });
    made.push(menu);
    if (!isCurrent()) {
      await closeAll(made);
      return;
    }
    const position = new LogicalPosition(at.x, at.y);
    previous = made;
    published = true;
    await menu.popup(position);
  } catch (error) {
    if (!published) {
      await closeAll(made);
    } else if (previous === made) {
      previous = [];
      await closeAll(made);
    }
    throw error;
  }
}

/// Where a menu pulled down from `element` opens: under it, from its left.
export function below(element: Element) {
  const bounds = element.getBoundingClientRect();
  return { x: bounds.left, y: bounds.bottom + 4 };
}
