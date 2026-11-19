/// ←/→ on the window while keyboard navigation of the menu bar holds it open
/// (macOS 27). The focused window receives them, so unless something in it
/// uses them they move the navigation on to the neighbouring item, as from a
/// menu. Every other key keeps its own meaning.

/// Whether keys on `target` edit text, where arrows move the caret.
export function editsText(target: EventTarget | null) {
  if (!(target instanceof Element)) return false;
  return (
    target.closest(
      'textarea, [contenteditable]:not([contenteditable="false"]), input:not([type="checkbox"]):not([type="radio"]):not([type="button"]):not([type="submit"]):not([type="reset"])',
    ) !== null
  );
}

/// The direction `event` moves menu bar navigation in, if it does: plain ←/→
/// during keyboard navigation, outside text and not used already.
export function menuBarStep(event: KeyboardEvent, navigating: boolean): 'left' | 'right' | null {
  if (!navigating || event.defaultPrevented || event.isComposing) return null;
  if (event.metaKey || event.ctrlKey || event.altKey || event.shiftKey) return null;
  const step = event.key === 'ArrowLeft' ? 'left' : event.key === 'ArrowRight' ? 'right' : null;
  if (!step || editsText(event.target)) return null;
  return step;
}

/// Hands `event` to `navigate` if it moves menu bar navigation. Returns
/// whether it did.
export function handleMenuBarKey(
  event: KeyboardEvent,
  navigating: boolean,
  navigate: (right: boolean) => void,
) {
  const step = menuBarStep(event, navigating);
  if (!step) return false;
  event.preventDefault();
  navigate(step === 'right');
  return true;
}
