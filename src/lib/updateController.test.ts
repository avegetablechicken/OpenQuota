import { describe, expect, it } from 'vitest';
import { updateFailure } from './updateController.svelte';

describe('update controller helpers', () => {
  it('normalizes structured and unknown update failures', () => {
    expect(
      updateFailure(
        { code: 'download_forbidden', message: 'Download refused.', retryable: false },
        'fallback',
      ),
    ).toEqual({
      code: 'download_forbidden',
      message: 'Download refused.',
      action: 'Try again later.',
      retryable: false,
    });
    expect(updateFailure(new Error('Update service unavailable.'), 'Safe fallback').message).toBe(
      'Update service unavailable.',
    );
  });
});
