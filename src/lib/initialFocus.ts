const SETTLING_EVENTS = ['pointerdown', 'mousedown', 'click', 'keydown'] as const;

/// As the panel's window becomes key, WebKit can hand focus to the first
/// control on the page, the Total Spend title, and draw its focus ring. Nothing
/// was chosen yet, so focus that arrives before the user's first key or click
/// in a freshly shown panel is dropped; from then on focus moves as usual.
export function dropFocusUntilInteraction(target: Window = window) {
  let fresh = true;
  const settle = () => (fresh = false);
  const drop = (event: FocusEvent) => {
    if (fresh && event.target instanceof HTMLElement) event.target.blur();
  };
  for (const type of SETTLING_EVENTS) target.addEventListener(type, settle, true);
  target.document.addEventListener('focusin', drop);
  return {
    /// The panel was hidden: drop focus again once it is next shown.
    reset() {
      fresh = true;
      const active = target.document.activeElement;
      if (active instanceof HTMLElement && active !== target.document.body) active.blur();
    },
    dispose() {
      for (const type of SETTLING_EVENTS) target.removeEventListener(type, settle, true);
      target.document.removeEventListener('focusin', drop);
    },
  };
}
