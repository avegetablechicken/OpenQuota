<script lang="ts">
  import { formatReset } from './pacing';
  import type { AppSettings } from './types';

  interface Props {
    resetsAt: string | null;
    now: number;
    resetDisplay: AppSettings['resetDisplay'];
    timeFormat: AppSettings['timeFormat'];
    onToggleReset: () => void;
  }

  let { resetsAt, now, resetDisplay, timeFormat, onToggleReset }: Props = $props();
  const tooltip = $derived(
    resetsAt
      ? formatReset(resetsAt, now, resetDisplay === 'countdown' ? 'exact' : 'countdown', timeFormat)
      : undefined,
  );
</script>

<button type="button" data-tooltip={tooltip} onclick={onToggleReset}>
  {formatReset(resetsAt, now, resetDisplay, timeFormat)}
</button>
