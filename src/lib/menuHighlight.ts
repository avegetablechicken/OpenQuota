/// A native menu has one highlighted item, which the pointer and the arrow
/// keys both move. Here the pointer over an item focuses it, so the focused
/// item is the highlighted one and the arrows go on from what is shown;
/// menus paint only `:focus`, whichever input put it there. Painting
/// `:focus-visible` instead would leave arrow moves unpainted after a click in
/// WebKit versions that count them as pointer focus (macOS 26).
export function menuHighlight(node: HTMLElement, items = 'button:not(:disabled), summary') {
  const follow = (event: PointerEvent) => {
    if (event.pointerType === 'touch' || !(event.target instanceof Element)) return;
    const item = event.target.closest<HTMLElement>(items);
    if (!item || !node.contains(item) || item === document.activeElement) return;
    item.focus({ preventScroll: true });
  };
  node.addEventListener('pointerover', follow);
  node.addEventListener('pointermove', follow);
  return {
    destroy() {
      node.removeEventListener('pointerover', follow);
      node.removeEventListener('pointermove', follow);
    },
  };
}
