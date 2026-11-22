import { beforeEach, describe, expect, it, vi } from 'vitest';
import type { NativeMenuEntry } from './nativeMenu';

vi.unmock('./nativeMenu');

interface MockHandle {
  kind: string;
  label?: string;
  close: ReturnType<typeof vi.fn<() => Promise<void>>>;
  popup?: ReturnType<typeof vi.fn<(position: unknown) => Promise<void>>>;
  opts?: unknown;
}

function deferred<T = void>() {
  let resolve!: (value: T | PromiseLike<T>) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function makeHandle(kind: string, label?: string, opts?: unknown): MockHandle {
  return {
    kind,
    label,
    opts,
    close: vi.fn<() => Promise<void>>().mockResolvedValue(undefined),
  };
}

const mocks = vi.hoisted(() => {
  class LogicalPosition {
    x: number;
    y: number;
    constructor(x: number, y: number) {
      this.x = x;
      this.y = y;
    }
  }

  return {
    LogicalPosition,
    MenuNew: vi.fn(),
    MenuItemNew: vi.fn(),
    CheckMenuItemNew: vi.fn(),
    PredefinedMenuItemNew: vi.fn(),
    SubmenuNew: vi.fn(),
  };
});

vi.mock('@tauri-apps/api/dpi', () => ({
  LogicalPosition: mocks.LogicalPosition,
}));

vi.mock('@tauri-apps/api/menu', () => ({
  Menu: { new: mocks.MenuNew },
  MenuItem: { new: mocks.MenuItemNew },
  CheckMenuItem: { new: mocks.CheckMenuItemNew },
  PredefinedMenuItem: { new: mocks.PredefinedMenuItemNew },
  Submenu: { new: mocks.SubmenuNew },
}));

let popupMenu: (entries: NativeMenuEntry[], at: { x: number; y: number }) => Promise<void>;
let below: (element: Element) => { x: number; y: number };

describe('nativeMenu', () => {
  beforeEach(async () => {
    vi.resetModules();
    mocks.MenuNew.mockReset().mockImplementation(async (opts: unknown) => {
      const handle = makeHandle('menu', undefined, opts);
      handle.popup = vi.fn<(position: unknown) => Promise<void>>().mockResolvedValue(undefined);
      return handle;
    });
    mocks.MenuItemNew.mockReset().mockImplementation(async (opts: { text?: string }) =>
      makeHandle('item', opts.text, opts),
    );
    mocks.CheckMenuItemNew.mockReset().mockImplementation(async (opts: { text?: string }) =>
      makeHandle('check', opts.text, opts),
    );
    mocks.PredefinedMenuItemNew.mockReset().mockImplementation(async (opts: unknown) =>
      makeHandle('separator', 'Separator', opts),
    );
    mocks.SubmenuNew.mockReset().mockImplementation(async (opts: { text?: string }) =>
      makeHandle('submenu', opts.text, opts),
    );

    const mod = await import('./nativeMenu');
    popupMenu = mod.popupMenu;
    below = mod.below;
  });

  it('builds entries, pops up at LogicalPosition, and closes stale handles on the next call', async () => {
    const firstItems: MockHandle[] = [];
    mocks.MenuItemNew.mockImplementationOnce(async (opts: { text?: string }) => {
      const handle = makeHandle('item', opts.text, opts);
      handle.close.mockRejectedValueOnce(new Error('async close error'));
      firstItems.push(handle);
      return handle;
    });
    mocks.CheckMenuItemNew.mockImplementationOnce(async (opts: { text?: string }) => {
      const handle = makeHandle('check', opts.text, opts);
      handle.close.mockImplementationOnce(() => {
        throw new Error('sync close error');
      });
      firstItems.push(handle);
      return handle;
    });
    mocks.PredefinedMenuItemNew.mockImplementationOnce(async (opts: unknown) => {
      const handle = makeHandle('separator', 'Separator', opts);
      firstItems.push(handle);
      return handle;
    });
    mocks.SubmenuNew.mockImplementationOnce(async (opts: { text?: string }) => {
      const handle = makeHandle('submenu', opts.text, opts);
      firstItems.push(handle);
      return handle;
    });
    let firstMenu!: MockHandle;
    mocks.MenuNew.mockImplementationOnce(async (opts: unknown) => {
      firstMenu = makeHandle('menu', undefined, opts);
      firstMenu.popup = vi.fn<(position: unknown) => Promise<void>>().mockResolvedValue(undefined);
      return firstMenu;
    });

    const shareAction = vi.fn();
    await popupMenu(
      [
        { text: 'Refresh', accelerator: 'CmdOrCtrl+R', action: shareAction },
        'separator',
        { text: 'Checked', checked: true, enabled: false },
        { text: 'Share', items: [{ text: 'Copy' }] },
      ],
      { x: 12, y: 34 },
    );

    expect(firstMenu.popup).toHaveBeenCalledOnce();
    const position = firstMenu.popup!.mock.calls[0][0];
    expect(position).toBeInstanceOf(mocks.LogicalPosition);
    expect(position).toEqual({ x: 12, y: 34 });

    // Handles remain open until the next popupMenu call.
    for (const handle of [...firstItems, firstMenu]) {
      expect(handle.close).not.toHaveBeenCalled();
    }

    // Second popupMenu closes all stale handles even when some close() calls throw/reject.
    await popupMenu([{ text: 'Next' }], { x: 0, y: 0 });
    for (const handle of [...firstItems, firstMenu]) {
      expect(handle.close).toHaveBeenCalledOnce();
    }
  });

  it('supports empty entries and closes the created menu on subsequent popupMenu', async () => {
    let emptyMenu!: MockHandle;
    mocks.MenuNew.mockImplementationOnce(async (opts: unknown) => {
      emptyMenu = makeHandle('menu', undefined, opts);
      emptyMenu.popup = vi.fn<(position: unknown) => Promise<void>>().mockResolvedValue(undefined);
      return emptyMenu;
    });

    await popupMenu([], { x: 5, y: 8 });
    expect(mocks.MenuNew).toHaveBeenCalledWith({ items: [] });
    expect(emptyMenu.popup).toHaveBeenCalledOnce();
    expect(emptyMenu.close).not.toHaveBeenCalled();

    await popupMenu([], { x: 10, y: 12 });
    expect(emptyMenu.close).toHaveBeenCalledOnce();
  });

  it('aborts superseded call that stalls while closing stale handles', async () => {
    const staleGate = deferred();
    const staleReached = deferred();
    const initialMenu = makeHandle('menu');
    initialMenu.popup = vi.fn<(position: unknown) => Promise<void>>().mockResolvedValue(undefined);
    initialMenu.close.mockImplementationOnce(() => {
      staleReached.resolve();
      return staleGate.promise;
    });
    mocks.MenuNew.mockResolvedValueOnce(initialMenu);

    await popupMenu([{ text: 'Initial' }], { x: 1, y: 1 });

    // Call 1 starts and stalls inside closeAll(stale).
    const call1 = popupMenu([{ text: 'StaleCall' }], { x: 2, y: 2 });
    await staleReached.promise;

    // Call 2 runs while Call 1 is stalled in closeAll(stale).
    const menu2 = makeHandle('menu');
    menu2.popup = vi.fn<(position: unknown) => Promise<void>>().mockResolvedValue(undefined);
    const item2 = makeHandle('item', 'FreshCall');
    mocks.MenuItemNew.mockResolvedValueOnce(item2);
    mocks.MenuNew.mockResolvedValueOnce(menu2);

    await popupMenu([{ text: 'FreshCall' }], { x: 3, y: 3 });

    // Release Call 1's stale close.
    staleGate.resolve();
    await call1;

    // Call 1 never built its item or created/popped a menu, and Call 2's handles are intact.
    expect(mocks.MenuItemNew).toHaveBeenCalledTimes(2); // Initial + FreshCall only
    expect(menu2.popup).toHaveBeenCalledOnce();
    expect(menu2.close).not.toHaveBeenCalled();
    expect(item2.close).not.toHaveBeenCalled();

    // Call 3 only closes Call 2's handles.
    await popupMenu([], { x: 4, y: 4 });
    expect(initialMenu.close).toHaveBeenCalledOnce();
    expect(item2.close).toHaveBeenCalledOnce();
    expect(menu2.close).toHaveBeenCalledOnce();
  });

  it('closes partial handles and avoids submenu/Menu/popup when superseded mid-build', async () => {
    const firstItemGate = deferred<MockHandle>();
    const firstItemReached = deferred();
    const builtItem = makeHandle('item', 'Child1');
    const secondItem = makeHandle('item', 'Fresh');
    const menu2 = makeHandle('menu');
    menu2.popup = vi.fn<(position: unknown) => Promise<void>>().mockResolvedValue(undefined);

    mocks.MenuItemNew.mockImplementationOnce(() => {
      firstItemReached.resolve();
      return firstItemGate.promise;
    }) // Call 1 submenu child 1
      .mockResolvedValueOnce(secondItem); // Call 2 item
    mocks.MenuNew.mockResolvedValueOnce(menu2);

    const call1 = popupMenu(
      [
        {
          text: 'Parent',
          items: [{ text: 'Child1' }, { text: 'Child2' }],
        },
        { text: 'AfterSubmenu' },
      ],
      { x: 10, y: 10 },
    );
    await firstItemReached.promise;

    // Call 2 starts and completes while Call 1 is awaiting Child1.
    await popupMenu([{ text: 'Fresh' }], { x: 20, y: 20 });

    // Now Child1 resolves for Call 1.
    firstItemGate.resolve(builtItem);
    await call1;

    expect(builtItem.close).toHaveBeenCalledOnce();
    expect(mocks.SubmenuNew).not.toHaveBeenCalled();
    expect(mocks.MenuNew).toHaveBeenCalledTimes(1); // Only Call 2's Menu.new
    expect(mocks.MenuItemNew).toHaveBeenCalledTimes(2); // Child1 + Fresh; Child2/AfterSubmenu skipped
    expect(menu2.popup).toHaveBeenCalledOnce();
    expect(menu2.close).not.toHaveBeenCalled();
    expect(secondItem.close).not.toHaveBeenCalled();
  });

  it('closes built handles including newly created menu when superseded during Menu.new', async () => {
    const menuGate = deferred<MockHandle>();
    const menuReached = deferred();
    const item1 = makeHandle('item', 'First');
    const menu1 = makeHandle('menu');
    menu1.popup = vi.fn<(position: unknown) => Promise<void>>().mockResolvedValue(undefined);

    const item2 = makeHandle('item', 'Second');
    const menu2 = makeHandle('menu');
    menu2.popup = vi.fn<(position: unknown) => Promise<void>>().mockResolvedValue(undefined);

    mocks.MenuItemNew.mockResolvedValueOnce(item1).mockResolvedValueOnce(item2);
    mocks.MenuNew.mockImplementationOnce(() => {
      menuReached.resolve();
      return menuGate.promise;
    }).mockResolvedValueOnce(menu2);

    const call1 = popupMenu([{ text: 'First' }], { x: 10, y: 10 });
    await menuReached.promise;

    await popupMenu([{ text: 'Second' }], { x: 20, y: 20 });

    menuGate.resolve(menu1);
    await call1;

    expect(menu1.popup).not.toHaveBeenCalled();
    expect(item1.close).toHaveBeenCalledOnce();
    expect(menu1.close).toHaveBeenCalledOnce();
    expect(menu2.popup).toHaveBeenCalledOnce();
    expect(item2.close).not.toHaveBeenCalled();
    expect(menu2.close).not.toHaveBeenCalled();
  });

  it('keeps newer previous intact if superseded popup rejects late', async () => {
    const popupGate = deferred<void>();
    const popupReached = deferred();
    const item1 = makeHandle('item', 'First');
    const menu1 = makeHandle('menu');
    menu1.popup = vi.fn<(position: unknown) => Promise<void>>().mockImplementationOnce(() => {
      popupReached.resolve();
      return popupGate.promise;
    });

    const item2 = makeHandle('item', 'Second');
    const menu2 = makeHandle('menu');
    menu2.popup = vi.fn<(position: unknown) => Promise<void>>().mockResolvedValue(undefined);

    mocks.MenuItemNew.mockResolvedValueOnce(item1).mockResolvedValueOnce(item2);
    mocks.MenuNew.mockResolvedValueOnce(menu1).mockResolvedValueOnce(menu2);

    const call1 = popupMenu([{ text: 'First' }], { x: 1, y: 1 });
    await popupReached.promise;

    // Call 2 closes Call 1's handles and opens menu2.
    await popupMenu([{ text: 'Second' }], { x: 2, y: 2 });
    expect(item1.close).toHaveBeenCalledOnce();
    expect(menu1.close).toHaveBeenCalledOnce();

    // Now menu1.popup rejects late.
    popupGate.reject(new Error('menu1 closed'));
    await expect(call1).rejects.toThrow('menu1 closed');

    // Call 1 handles must not be double-closed, and Call 2 handles remain in previous.
    expect(item1.close).toHaveBeenCalledOnce();
    expect(menu1.close).toHaveBeenCalledOnce();
    expect(item2.close).not.toHaveBeenCalled();
    expect(menu2.close).not.toHaveBeenCalled();

    // Call 3 closes Call 2's handles.
    await popupMenu([], { x: 3, y: 3 });
    expect(item2.close).toHaveBeenCalledOnce();
    expect(menu2.close).toHaveBeenCalledOnce();
  });

  it('closes built handles and leaves previous empty on mid-build item rejection', async () => {
    const built = makeHandle('item', 'OkItem');
    const sep = makeHandle('separator', 'Separator');
    mocks.MenuItemNew.mockResolvedValueOnce(built).mockRejectedValueOnce(new Error('build fail'));
    mocks.PredefinedMenuItemNew.mockResolvedValueOnce(sep);

    await expect(
      popupMenu([{ text: 'OkItem' }, 'separator', { text: 'BrokenItem' }], { x: 0, y: 0 }),
    ).rejects.toThrow('build fail');

    expect(built.close).toHaveBeenCalledOnce();
    expect(sep.close).toHaveBeenCalledOnce();

    // Next call does not re-close failed call's handles.
    await popupMenu([], { x: 0, y: 0 });
    expect(built.close).toHaveBeenCalledOnce();
    expect(sep.close).toHaveBeenCalledOnce();
  });

  it('closes outer and nested child handles when Submenu.new rejects', async () => {
    const outerItem = makeHandle('item', 'Outer');
    const childItem = makeHandle('item', 'Inner');
    mocks.MenuItemNew.mockResolvedValueOnce(outerItem).mockResolvedValueOnce(childItem);
    mocks.SubmenuNew.mockRejectedValueOnce(new Error('submenu fail'));

    await expect(
      popupMenu([{ text: 'Outer' }, { text: 'Group', items: [{ text: 'Inner' }] }], { x: 0, y: 0 }),
    ).rejects.toThrow('submenu fail');

    expect(outerItem.close).toHaveBeenCalledOnce();
    expect(childItem.close).toHaveBeenCalledOnce();
  });

  it('closes all built handles when Menu.new rejects', async () => {
    const item = makeHandle('item', 'Item');
    mocks.MenuItemNew.mockResolvedValueOnce(item);
    mocks.MenuNew.mockRejectedValueOnce(new Error('menu new fail'));

    await expect(popupMenu([{ text: 'Item' }], { x: 0, y: 0 })).rejects.toThrow('menu new fail');
    expect(item.close).toHaveBeenCalledOnce();
  });

  it('closes menu and items and clears previous when non-superseded menu.popup rejects', async () => {
    const item = makeHandle('item', 'Item');
    const menu = makeHandle('menu');
    menu.popup = vi
      .fn<(position: unknown) => Promise<void>>()
      .mockRejectedValueOnce(new Error('popup fail'));

    mocks.MenuItemNew.mockResolvedValueOnce(item);
    mocks.MenuNew.mockResolvedValueOnce(menu);

    await expect(popupMenu([{ text: 'Item' }], { x: 0, y: 0 })).rejects.toThrow('popup fail');
    expect(item.close).toHaveBeenCalledOnce();
    expect(menu.close).toHaveBeenCalledOnce();

    // Previous was cleared, so next popupMenu does not close them again.
    await popupMenu([], { x: 0, y: 0 });
    expect(item.close).toHaveBeenCalledOnce();
    expect(menu.close).toHaveBeenCalledOnce();
  });

  it('closes failed superseded build handles without clearing the newer previous menu', async () => {
    const failGate = deferred<MockHandle>();
    const failReached = deferred();
    const builtBeforeFail = makeHandle('item', 'Partial');
    const freshItem = makeHandle('item', 'Fresh');
    const freshMenu = makeHandle('menu');
    freshMenu.popup = vi.fn<(position: unknown) => Promise<void>>().mockResolvedValue(undefined);

    mocks.MenuItemNew.mockResolvedValueOnce(builtBeforeFail)
      .mockImplementationOnce(() => {
        failReached.resolve();
        return failGate.promise;
      })
      .mockResolvedValueOnce(freshItem);
    mocks.MenuNew.mockResolvedValueOnce(freshMenu);

    const call1 = popupMenu([{ text: 'Partial' }, { text: 'Fails' }], { x: 1, y: 1 });
    await failReached.promise;

    await popupMenu([{ text: 'Fresh' }], { x: 2, y: 2 });
    failGate.reject(new Error('late build fail'));
    await expect(call1).rejects.toThrow('late build fail');

    expect(builtBeforeFail.close).toHaveBeenCalledOnce();
    expect(freshItem.close).not.toHaveBeenCalled();
    expect(freshMenu.close).not.toHaveBeenCalled();

    await popupMenu([], { x: 3, y: 3 });
    expect(freshItem.close).toHaveBeenCalledOnce();
    expect(freshMenu.close).toHaveBeenCalledOnce();
  });

  it('computes popup position below an element', () => {
    const element = document.createElement('button');
    vi.spyOn(element, 'getBoundingClientRect').mockReturnValue(
      DOMRect.fromRect({ x: 24, y: 10, width: 100, height: 30 }),
    );
    expect(below(element)).toEqual({ x: 24, y: 44 });
  });
});
