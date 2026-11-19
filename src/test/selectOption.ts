import { fireEvent, within } from '@testing-library/svelte';

/// Chooses the option named `name` in a native <select>, as its system menu
/// would: the value changes and the select reports it.
export async function selectOption(select: HTMLElement, name: string) {
  const option = within(select).getByRole<HTMLOptionElement>('option', { name });
  await fireEvent.change(select, { target: { value: option.value } });
}
