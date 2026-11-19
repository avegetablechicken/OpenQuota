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

async function build(entries: NativeMenuEntry[], made: Closable[]): Promise<Item[]> {
  const items: Item[] = [];
  for (const entry of entries) {
    if (entry === 'separator') {
      const item = await PredefinedMenuItem.new({ item: 'Separator' });
      made.push(item);
      items.push(item);
      continue;
    }
    const enabled = entry.enabled ?? true;
    const action = entry.action ? () => entry.action?.() : undefined;
    const item: Item = entry.items
      ? await Submenu.new({ text: entry.text, enabled, items: await build(entry.items, made) })
      : entry.checked !== undefined
        ? await CheckMenuItem.new({ text: entry.text, enabled, checked: entry.checked, action })
        : await MenuItem.new({
            text: entry.text,
            enabled,
            accelerator: entry.accelerator,
            action,
          });
    made.push(item);
    items.push(item);
  }
  return items;
}

/// Opens `entries` as a system menu whose top left is at `at`, in the
/// window's coordinates.
export async function popupMenu(entries: NativeMenuEntry[], at: { x: number; y: number }) {
  const stale = previous;
  previous = [];
  await Promise.all(stale.map((item) => item.close().catch(() => undefined)));
  const made: Closable[] = [];
  const menu = await Menu.new({ items: await build(entries, made) });
  made.push(menu);
  previous = made;
  await menu.popup(new LogicalPosition(at.x, at.y));
}

/// Where a menu pulled down from `element` opens: under it, from its left.
export function below(element: Element) {
  const bounds = element.getBoundingClientRect();
  return { x: bounds.left, y: bounds.bottom + 4 };
}
