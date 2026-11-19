import { fireEvent } from '@testing-library/svelte';
import { afterEach, describe, expect, it } from 'vitest';
import { menuHighlight } from './menuHighlight';

describe('menu highlight', () => {
  afterEach(() => document.body.replaceChildren());

  function menu() {
    document.body.innerHTML = `
      <button id="outside">Outside</button>
      <div id="menu" role="menu" tabindex="-1">
        <button id="first"><span id="label">First</span></button>
        <button id="off" disabled>Off</button>
        <details><summary id="sub">Submenu</summary></details>
        <hr id="rule" />
      </div>`;
    const node = document.getElementById('menu')!;
    return { node, action: menuHighlight(node) };
  }
  const byId = (id: string) => document.getElementById(id)!;

  it('focuses the item under the pointer, so the arrows go on from it', async () => {
    menu();
    await fireEvent.pointerMove(byId('label'), { pointerType: 'mouse' });
    expect(byId('first')).toHaveFocus();
    await fireEvent.pointerOver(byId('sub'), { pointerType: 'mouse' });
    expect(byId('sub')).toHaveFocus();
  });

  it('leaves focus on disabled items, separators, touches and after removal', async () => {
    const { action } = menu();
    byId('first').focus();
    await fireEvent.pointerMove(byId('off'), { pointerType: 'mouse' });
    await fireEvent.pointerMove(byId('rule'), { pointerType: 'mouse' });
    await fireEvent.pointerMove(byId('sub'), { pointerType: 'touch' });
    expect(byId('first')).toHaveFocus();
    action.destroy();
    await fireEvent.pointerMove(byId('sub'), { pointerType: 'mouse' });
    expect(byId('first')).toHaveFocus();
  });

  it('takes only the items it is given', async () => {
    document.body.innerHTML = `
      <div id="list" role="listbox">
        <button id="option" role="option">Option</button>
        <button id="extra">Extra</button>
      </div>`;
    const action = menuHighlight(byId('list'), '[role="option"]');
    await fireEvent.pointerMove(byId('extra'), { pointerType: 'mouse' });
    expect(document.activeElement).toBe(document.body);
    await fireEvent.pointerMove(byId('option'), { pointerType: 'mouse' });
    expect(byId('option')).toHaveFocus();
    action.destroy();
  });
});
