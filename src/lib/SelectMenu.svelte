<script lang="ts">
  import Icon from './Icon.svelte';
  import { below, popupMenu } from './nativeMenu';

  export interface SelectOption {
    value: string;
    label: string;
  }

  interface Props {
    label: string;
    value: string;
    options: SelectOption[];
    onChange: (value: string) => void;
    variant?: 'field' | 'title';
  }

  let { label, value, options, onChange, variant = 'field' }: Props = $props();
  let pullDown = $state<HTMLButtonElement>();
  const selectedLabel = $derived(options.find((option) => option.value === value)?.label ?? value);

  function change(event: Event) {
    const select = event.currentTarget as HTMLSelectElement;
    const next = select.value;
    // The page owns the value: it shows `value` until the change is applied.
    select.value = value;
    if (next !== value) onChange(next);
  }

  // A title pulls its menu down below it, the chosen option checked, rather
  // than laying the menu over itself and what is above as a pop-up does.
  function openPullDown() {
    if (!pullDown) return;
    void popupMenu(
      options.map((option) => ({
        text: option.label,
        checked: option.value === value,
        action: () => {
          if (option.value !== value) onChange(option.value);
        },
      })),
      below(pullDown),
    ).catch(() => undefined);
  }
</script>

<!-- A field is a native <select>, whose list is the system's own menu: it
     highlights, checks and moves with the keys as every other menu does. The
     trigger drawn beneath shows the chosen label and sizes the control to it.
     A title is a pull-down button opening a system menu (nativeMenu). -->
<div class="select-menu" class:select-menu--title={variant === 'title'}>
  {#if variant === 'title'}
    <button
      type="button"
      class="select-menu__trigger"
      aria-label={label}
      aria-haspopup="menu"
      bind:this={pullDown}
      onclick={openPullDown}
    >
      <span class="select-menu__label">{selectedLabel}</span><span class="select-menu__stepper"
        ><Icon name="chevron-down" size={11} strokeWidth={2.2} /></span
      >
    </button>
  {:else}
    <span class="select-menu__trigger" aria-hidden="true">
      <span class="select-menu__label">{selectedLabel}</span><span class="select-menu__stepper"
        ><Icon name="chevron-up-down" size={11} strokeWidth={2.2} /></span
      >
    </span>
    <select class="select-menu__native" aria-label={label} {value} onchange={change}>
      {#each options as option (option.value)}
        <option value={option.value}>{option.label}</option>
      {/each}
    </select>
  {/if}
</div>

<style>
  :global {
    /* A pop-up button as System Settings draws one in a list: the value,
       then a round button with up and down chevrons.

       The system menu takes its font from the select and puts the chosen
       item's label a fixed distance in from the select's left edge (as for
       a pop-up button, whatever the select's padding), so the select starts
       that far left of the value for the two to line up. The menu is as
       wide as the select and starts a little left of it, so the select also
       reaches that much past the trigger's right edge for the menu to end
       where the trigger does. Clipped to the trigger, it takes clicks only
       there. */
    .select-menu {
      --select-inset: 0px;
      --popup-title-inset: 17.5px;
      --popup-menu-lead: 13.5px;
      position: relative;
      min-width: 0;
      flex: 0 0 auto;
      font-size: 12px;
      line-height: 18px;
    }

    .select-menu__trigger {
      display: flex;
      width: 100%;
      min-height: 28px;
      align-items: center;
      justify-content: flex-end;
      gap: 6px;
      padding: 0 0 0 var(--select-inset);
      border: 0;
      border-radius: 5px;
      color: var(--text);
      background: transparent;
      font: inherit;
      white-space: nowrap;
    }

    .select-menu__label {
      color: var(--secondary);
    }

    .select-menu__stepper {
      display: grid;
      width: 20px;
      height: 20px;
      flex: 0 0 20px;
      place-items: center;
      border-radius: 50%;
      color: var(--text);
      background: color-mix(in srgb, var(--text) 8%, transparent);
      transition: background var(--motion-switch);
    }

    .select-menu:hover .select-menu__stepper {
      background: color-mix(in srgb, var(--text) 14%, transparent);
    }

    /* Covers the trigger so a click or a key reaches the native control. */
    .select-menu__native {
      --select-overhang: calc(var(--popup-title-inset) - var(--select-inset));
      position: absolute;
      top: 0;
      left: calc(0px - var(--select-overhang));
      width: calc(100% + var(--select-overhang) + var(--popup-menu-lead));
      height: 100%;
      margin: 0;
      padding: 0;
      border: 0;
      -webkit-appearance: none;
      appearance: none;
      background: transparent;
      clip-path: inset(0 var(--popup-menu-lead) 0 var(--select-overhang));
      opacity: 0;
      font: inherit;
      cursor: pointer;
    }

    .select-menu:has(.select-menu__native:focus-visible) .select-menu__stepper {
      outline: 2px solid color-mix(in srgb, var(--meter-fill) 55%, transparent);
      outline-offset: 1px;
    }

    :root[data-density='compact'] .select-menu {
      line-height: 16px;
    }

    :root[data-density='compact'] .select-menu__trigger {
      min-height: 26px;
    }
  }
</style>
