export interface VerticalBounds {
  top: number;
  bottom: number;
}

export interface FixedPopoverOptions {
  preferred?: 'above' | 'below';
  gap?: number;
  margin?: number;
}

export function clampFixedPopoverTop(
  top: number,
  height: number,
  viewportHeight: number,
  margin = 8,
): number {
  const maxTop = Math.max(margin, viewportHeight - margin - height);
  return Math.round(Math.max(margin, Math.min(top, maxTop)));
}

export function computeFixedPopoverTop(
  anchor: VerticalBounds,
  height: number,
  viewportHeight: number,
  options?: FixedPopoverOptions,
): number {
  const preferred = options?.preferred ?? 'below';
  const gap = options?.gap ?? 7;
  const margin = options?.margin ?? 8;
  const belowTop = anchor.bottom + gap;
  const aboveTop = anchor.top - gap - height;
  const fitsBelow = belowTop + height <= viewportHeight - margin;
  const fitsAbove = aboveTop >= margin;

  if (preferred === 'above') {
    if (fitsAbove || (!fitsBelow && anchor.top >= viewportHeight - anchor.bottom)) {
      return clampFixedPopoverTop(aboveTop, height, viewportHeight, margin);
    }
    return clampFixedPopoverTop(belowTop, height, viewportHeight, margin);
  }

  if (fitsBelow || (!fitsAbove && viewportHeight - anchor.bottom >= anchor.top)) {
    return clampFixedPopoverTop(belowTop, height, viewportHeight, margin);
  }
  return clampFixedPopoverTop(aboveTop, height, viewportHeight, margin);
}
