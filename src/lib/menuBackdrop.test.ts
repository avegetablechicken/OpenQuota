import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { menuBackdrop } from './menuBackdrop';
import { captureMenuBackdrop } from './backend';

vi.mock('./backend', () => ({ captureMenuBackdrop: vi.fn() }));
vi.mock('./platform', () => ({ desktopPlatform: () => 'linux' }));

beforeEach(() => {
  vi.stubGlobal('__TAURI_INTERNALS__', {});
  vi.stubGlobal('requestAnimationFrame', (callback: FrameRequestCallback) => {
    queueMicrotask(() => callback(0));
    return 1;
  });
  vi.mocked(captureMenuBackdrop).mockReset();
});
afterEach(() => {
  document.body.replaceChildren();
  vi.unstubAllGlobals();
});

describe('software menu backdrop', () => {
  it('hides only the new menu while capturing and aligns the blurred application image', async () => {
    const node = document.createElement('div');
    document.body.append(node);
    vi.spyOn(node, 'getBoundingClientRect').mockReturnValue({ left: 20, top: 40 } as DOMRect);
    vi.mocked(captureMenuBackdrop).mockImplementation(async () => {
      expect(node.classList.contains('menu-snapshot-pending')).toBe(true);
      return 'data:image/png;base64,AAAA';
    });
    const action = menuBackdrop(node);
    await vi.waitFor(() => expect(node.classList.contains('menu-snapshot-ready')).toBe(true));
    expect(node.classList.contains('menu-snapshot-pending')).toBe(false);
    expect(node.style.getPropertyValue('--menu-snapshot-position')).toBe('-20px -40px');
    expect(node.style.getPropertyValue('--menu-snapshot-image')).toContain('data:image/png');
    node.setAttribute('style', 'top: 40px; left: 20px');
    await vi.waitFor(() =>
      expect(node.style.getPropertyValue('--menu-snapshot-image')).toContain('data:image/png'),
    );
    action.destroy?.();
  });

  it('reveals an opaque fallback when the native snapshot fails', async () => {
    const node = document.createElement('div');
    vi.mocked(captureMenuBackdrop).mockRejectedValue(new Error('snapshot failed'));
    const action = menuBackdrop(node);
    await vi.waitFor(() => expect(captureMenuBackdrop).toHaveBeenCalledOnce());
    expect(node.classList.contains('menu-software-backdrop')).toBe(true);
    expect(node.classList.contains('menu-snapshot-pending')).toBe(false);
    expect(node.classList.contains('menu-snapshot-ready')).toBe(false);
    action.destroy?.();
  });

  it('waits until details opens and ignores results after destruction', async () => {
    const details = document.createElement('details');
    const node = document.createElement('div');
    details.append(node);
    document.body.append(details);
    let finish!: (image: string) => void;
    vi.mocked(captureMenuBackdrop).mockImplementation(
      () =>
        new Promise((resolve) => {
          finish = resolve;
        }),
    );
    const action = menuBackdrop(node);
    expect(captureMenuBackdrop).not.toHaveBeenCalled();
    details.open = true;
    await vi.waitFor(() => expect(captureMenuBackdrop).toHaveBeenCalledOnce());
    action.destroy?.();
    finish('data:image/png;base64,AAAA');
    await Promise.resolve();
    expect(node.classList.contains('menu-snapshot-ready')).toBe(false);
  });
});
