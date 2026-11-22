import { cleanup, fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { chooseMenuItem, menuLabels, openMenu } from '../test/nativeMenu';
import { selectOption } from '../test/selectOption';
import SelectMenu from './SelectMenu.svelte';

const options = [
  { value: 'cost', label: 'Cost' },
  { value: 'tokens', label: 'Tokens' },
];

afterEach(() => {
  cleanup();
});

describe('SelectMenu', () => {
  it('is a native select, whose list is the system menu', () => {
    render(SelectMenu, { label: 'Metric', value: 'tokens', options, onChange: vi.fn() });

    const select = screen.getByRole('combobox', { name: 'Metric' });
    expect(select.tagName).toBe('SELECT');
    expect(select).toHaveDisplayValue('Tokens');
    expect(
      within(select)
        .getAllByRole('option')
        .map((option) => option.textContent),
    ).toEqual(['Cost', 'Tokens']);
    // The trigger drawn beneath shows the chosen label, and only once to
    // assistive technology: through the select.
    const shown = select.parentElement!.querySelector('.select-menu__trigger')!;
    expect(shown).toHaveTextContent('Tokens');
    expect(shown).toHaveAttribute('aria-hidden', 'true');
  });

  it('reports a choice and shows it once applied', async () => {
    const onChange = vi.fn();
    const { rerender } = render(SelectMenu, { label: 'Metric', value: 'cost', options, onChange });
    const select = screen.getByRole('combobox', { name: 'Metric' });

    await selectOption(select, 'Tokens');
    expect(onChange).toHaveBeenCalledWith('tokens');
    // Until the page applies it, the select still shows the current value.
    expect(select).toHaveDisplayValue('Cost');

    await rerender({ label: 'Metric', value: 'tokens', options, onChange });
    expect(select).toHaveDisplayValue('Tokens');
    expect(select.parentElement!.querySelector('.select-menu__trigger')).toHaveTextContent(
      'Tokens',
    );
  });

  it('does not report choosing the current value', async () => {
    const onChange = vi.fn();
    render(SelectMenu, { label: 'Metric', value: 'cost', options, onChange });

    await fireEvent.change(screen.getByRole('combobox', { name: 'Metric' }), {
      target: { value: 'cost' },
    });
    expect(onChange).not.toHaveBeenCalled();
  });

  it('pulls a title down as a system menu with the chosen option checked', async () => {
    const onChange = vi.fn();
    render(SelectMenu, { label: 'Metric', value: 'tokens', options, onChange, variant: 'title' });
    const button = screen.getByRole('button', { name: 'Metric' });
    expect(button).toHaveTextContent('Tokens');
    vi.spyOn(button, 'getBoundingClientRect').mockReturnValue(
      DOMRect.fromRect({ x: 16, y: 10, width: 80, height: 24 }),
    );

    await fireEvent.click(button);
    expect(menuLabels()).toEqual(['Cost', '✓ Tokens']);
    // Below the title, not over it and what is above.
    expect(openMenu()!.at).toEqual({ x: 16, y: 38 });

    await chooseMenuItem('Cost');
    expect(onChange).toHaveBeenCalledWith('cost');
    await fireEvent.click(button);
    await chooseMenuItem('Tokens');
    expect(onChange).toHaveBeenCalledTimes(1);
  });

  it('disables the field select and ignores changes when disabled', async () => {
    const onChange = vi.fn();
    render(SelectMenu, { label: 'Metric', value: 'cost', options, onChange, disabled: true });

    const select = screen.getByRole('combobox', { name: 'Metric' });
    expect(select).toBeDisabled();
    expect(select.closest('.select-menu')).toHaveClass('select-menu--disabled');

    await selectOption(select, 'Tokens');
    expect(onChange).not.toHaveBeenCalled();
    expect(select).toHaveDisplayValue('Cost');
  });

  it('disables the title button and guards both click and late menu actions when disabled', async () => {
    const onChange = vi.fn();
    const { rerender } = render(SelectMenu, {
      label: 'Metric',
      value: 'tokens',
      options,
      onChange,
      variant: 'title',
      disabled: true,
    });

    const button = screen.getByRole('button', { name: 'Metric' });
    expect(button).toBeDisabled();
    expect(button.closest('.select-menu')).toHaveClass('select-menu--disabled');

    await fireEvent.click(button);
    expect(openMenu()).toBeNull();
    expect(onChange).not.toHaveBeenCalled();

    // If opened while enabled and then disabled before the user picks an item,
    // the late action callback does not fire onChange.
    await rerender({
      label: 'Metric',
      value: 'tokens',
      options,
      onChange,
      variant: 'title',
      disabled: false,
    });
    await fireEvent.click(button);
    expect(openMenu()).not.toBeNull();

    await rerender({
      label: 'Metric',
      value: 'tokens',
      options,
      onChange,
      variant: 'title',
      disabled: true,
    });
    await chooseMenuItem('Cost');
    expect(onChange).not.toHaveBeenCalled();
  });
});
