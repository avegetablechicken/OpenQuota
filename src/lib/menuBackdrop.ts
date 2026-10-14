import { captureMenuBackdrop } from './backend';
import { desktopPlatform } from './platform';

// WebKitGTK can accept backdrop-filter without actually blurring. Capture the
// rendered application before revealing the menu and blur it in software.
export function menuBackdrop(node: HTMLElement) {
  if (desktopPlatform() !== 'linux' || !('__TAURI_INTERNALS__' in window)) return {};
  const details = node.parentElement instanceof HTMLDetailsElement ? node.parentElement : null;
  let generation = 0;
  let disposed = false;
  let snapshotImage = '';
  let snapshotSize = '';
  const property = (name: string, value: string) => {
    if (node.style.getPropertyValue(name) !== value) node.style.setProperty(name, value);
  };
  const frame = () => new Promise<void>((resolve) => requestAnimationFrame(() => resolve()));
  const align = () => {
    const bounds = node.getBoundingClientRect();
    property('--menu-snapshot-position', `${-bounds.left}px ${-bounds.top}px`);
    if (snapshotImage) {
      property('--menu-snapshot-image', snapshotImage);
      property('--menu-snapshot-size', snapshotSize);
    }
  };
  async function capture() {
    const current = ++generation;
    node.classList.remove('menu-snapshot-ready');
    if (details && !details.open) return;
    node.classList.add('menu-snapshot-pending');
    try {
      // Let layout, portals and the hidden-menu paint settle before WebKit snapshots.
      await frame();
      await frame();
      if (disposed || current !== generation) return;
      const width = window.innerWidth;
      const height = window.innerHeight;
      const image = await captureMenuBackdrop();
      if (disposed || current !== generation) return;
      snapshotImage = `url("${image}")`;
      snapshotSize = `${width}px ${height}px`;
      align();
      node.classList.add('menu-snapshot-ready');
    } catch {
      // Keep an opaque readable surface if capturing fails; never expose sharp text.
    } finally {
      if (!disposed && current === generation) node.classList.remove('menu-snapshot-pending');
    }
  }
  // Svelte rewrites inline placement styles when a portalled listbox moves.
  const placement = new MutationObserver(align);
  placement.observe(node, { attributes: true, attributeFilter: ['style'] });
  node.classList.add('menu-software-backdrop');
  const toggle = () => {
    void capture();
  };
  details?.addEventListener('toggle', toggle);
  window.addEventListener('resize', align);
  document.addEventListener('scroll', align, true);
  void capture();
  return {
    destroy() {
      disposed = true;
      generation++;
      placement.disconnect();
      details?.removeEventListener('toggle', toggle);
      window.removeEventListener('resize', align);
      document.removeEventListener('scroll', align, true);
    },
  };
}
