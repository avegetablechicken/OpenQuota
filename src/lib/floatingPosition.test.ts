import { describe, expect, it } from 'vitest';
import { clampFixedPopoverTop, computeFixedPopoverTop } from './floatingPosition';

describe('floatingPosition', () => {
  it('places preferred-above popovers above when space permits and flips below near the top edge', () => {
    expect(
      computeFixedPopoverTop({ top: 240, bottom: 258 }, 160, 400, { preferred: 'above', gap: 12 }),
    ).toBe(68);
    expect(
      computeFixedPopoverTop({ top: 40, bottom: 58 }, 160, 400, { preferred: 'above', gap: 12 }),
    ).toBe(70);
  });

  it('places preferred-below popovers below when space permits and flips above near the bottom edge', () => {
    expect(
      computeFixedPopoverTop({ top: 60, bottom: 76 }, 120, 400, { preferred: 'below', gap: 4 }),
    ).toBe(80);
    expect(
      computeFixedPopoverTop({ top: 340, bottom: 356 }, 120, 400, { preferred: 'below', gap: 4 }),
    ).toBe(216);
  });

  it('clamps tall popovers cleanly inside viewport margins', () => {
    expect(clampFixedPopoverTop(320, 120, 400)).toBe(272);
    expect(clampFixedPopoverTop(-20, 120, 400)).toBe(8);
    expect(clampFixedPopoverTop(100, 420, 400)).toBe(8);
  });
});
