import { desktopPlatform } from './platform';

function matchesSaveShortcut(event: KeyboardEvent, platform = desktopPlatform()) {
  if (
    event.defaultPrevented ||
    event.isComposing ||
    event.repeat ||
    event.altKey ||
    event.shiftKey ||
    (event.key.toLowerCase() !== 's' && event.code !== 'KeyS')
  ) {
    return false;
  }
  return platform === 'macos' ? event.metaKey && !event.ctrlKey : event.ctrlKey && !event.metaKey;
}

export function saveShortcut(node: HTMLButtonElement, enabled: boolean) {
  const platform = desktopPlatform();
  let listening = false;

  function handleKeydown(event: KeyboardEvent) {
    if (!matchesSaveShortcut(event, platform) || node.disabled || !node.isConnected) return;
    event.preventDefault();
    node.click();
  }

  function update(next: boolean) {
    if (next === listening) return;
    listening = next;
    if (listening) {
      document.addEventListener('keydown', handleKeydown);
      node.setAttribute('aria-keyshortcuts', platform === 'macos' ? 'Meta+S' : 'Control+S');
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
