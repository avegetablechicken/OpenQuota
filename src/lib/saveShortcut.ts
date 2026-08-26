function matchesSaveShortcut(event: KeyboardEvent) {
  if (
    event.defaultPrevented ||
    event.isComposing ||
    event.repeat ||
    event.ctrlKey ||
    event.metaKey ||
    event.altKey ||
    event.shiftKey ||
    event.key !== 'Enter'
  ) {
    return false;
  }
  if (!(event.target instanceof Element)) return true;
  const input = event.target.closest<HTMLInputElement>('input');
  if (input) {
    return (
      ['email', 'number', 'password', 'search', 'tel', 'text', 'url'].includes(input.type) &&
      !input.disabled &&
      !input.readOnly
    );
  }
  return (
    event.target.closest(
      'button, a, select, textarea, summary, [contenteditable], [role="button"], [role="menuitem"], [role="option"], [role="combobox"], [role="switch"]',
    ) === null
  );
}

export function saveShortcut(node: HTMLButtonElement, enabled: boolean) {
  let listening = false;

  function handleKeydown(event: KeyboardEvent) {
    if (!matchesSaveShortcut(event) || node.disabled || !node.isConnected) return;
    event.preventDefault();
    node.click();
  }

  function update(next: boolean) {
    if (next === listening) return;
    listening = next;
    if (listening) {
      document.addEventListener('keydown', handleKeydown);
      node.setAttribute('aria-keyshortcuts', 'Enter');
    } else {
      document.removeEventListener('keydown', handleKeydown);
      node.removeAttribute('aria-keyshortcuts');
    }
  }

  update(enabled);
  return {
    update,
    destroy() {
      if (listening) document.removeEventListener('keydown', handleKeydown);
      listening = false;
      node.removeAttribute('aria-keyshortcuts');
    },
  };
}
