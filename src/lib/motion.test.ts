import { describe, expect, it } from 'vitest';
import { reorderFlip, springEasing, springMotion, verticalSlide } from './motion';

describe('shared motion profile', () => {
  it('matches the normalized endpoints of the shared spring', () => {
    expect(springEasing(0)).toBe(0);
    expect(springEasing(1)).toBe(1);
    expect(springEasing(0.55)).toBeGreaterThan(1);
  });

  it('uses the shared response and disables movement for reduced motion', () => {
    expect(reorderFlip(false).duration).toBe(630);
    expect(reorderFlip(true).duration).toBe(0);
    expect(reorderFlip(false).easing).toBe(springEasing);
    expect(reorderFlip).toBe(springMotion);
  });

  it('stays finite throughout the animation', () => {
    const samples: number[] = [];
    for (let step = 0; step <= 100; step += 1) {
      const value = springEasing(step / 100);
      samples.push(value);
      expect(Number.isFinite(value)).toBe(true);
    }
    expect(Math.min(...samples)).toBeGreaterThanOrEqual(0);
    expect(Math.max(...samples)).toBeLessThan(1.02);
  });

  it('slides cards vertically without clipping horizontal shadows', () => {
    const node = document.createElement('div');
    node.style.height = '120px';
    node.style.paddingTop = '14px';
    node.style.opacity = '1';
    document.body.append(node);
    try {
      const transition = verticalSlide(node, springMotion(false));
      expect(transition.duration).toBe(630);
      expect(transition.easing).toBe(springEasing);
      const start = transition.css?.(0, 1) ?? '';
      const mid = transition.css?.(0.5, 0.5) ?? '';
      const end = transition.css?.(1, 0) ?? '';
      expect(start).toContain('overflow-x: visible; overflow-y: clip;');
      expect(start).toContain('height: 0px;');
      expect(start).toContain('padding-top: 0px;');
      expect(start).toContain('opacity: 0;');
      expect(mid).toContain('height: 60px;');
      expect(mid).toContain('padding-top: 7px;');
      expect(mid).toContain('opacity: 1;');
      expect(end).toContain('height: 120px;');
      expect(end).toContain('padding-top: 14px;');
    } finally {
      node.remove();
    }
  });
});
