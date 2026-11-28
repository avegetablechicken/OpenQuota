import type { EasingFunction, TransitionConfig } from 'svelte/transition';

const SPRING_RESPONSE_SECONDS = 0.42;
const SPRING_DAMPING = 0.8;
const SPRING_SETTLE_RESPONSES = 1.5;
const TAU = Math.PI * 2;

function springStep(progress: number) {
  const clamped = Math.min(1, Math.max(0, progress));
  const responseTime = clamped * SPRING_SETTLE_RESPONSES;
  const dampedFrequency = Math.sqrt(1 - SPRING_DAMPING ** 2);
  const phase = TAU * dampedFrequency * responseTime;
  const envelope = Math.exp(-SPRING_DAMPING * TAU * responseTime);
  return 1 - envelope * (Math.cos(phase) + (SPRING_DAMPING / dampedFrequency) * Math.sin(phase));
}

const springEnd = springStep(1);

/** SwiftUI-style response 0.42 / damping 0.80 spring, normalized to finish exactly at 1. */
export function springEasing(progress: number) {
  if (progress <= 0) return 0;
  if (progress >= 1) return 1;
  return springStep(progress) / springEnd;
}

export function springMotion(reducedMotion: boolean) {
  return {
    duration: reducedMotion ? 0 : SPRING_RESPONSE_SECONDS * SPRING_SETTLE_RESPONSES * 1000,
    easing: springEasing,
  };
}

export const reorderFlip = springMotion;

interface VerticalSlideParams {
  delay?: number;
  duration?: number;
  easing?: EasingFunction;
}

/**
 * Slides a card vertically in normal document flow while keeping horizontal
 * overflow visible so soft card shadows do not clip at the left/right edges.
 */
export function verticalSlide(
  node: Element,
  {
    delay = 0,
    duration = SPRING_RESPONSE_SECONDS * SPRING_SETTLE_RESPONSES * 1000,
    easing = springEasing,
  }: VerticalSlideParams = {},
): TransitionConfig {
  const style = getComputedStyle(node);
  const parsedOpacity = Number.parseFloat(style.opacity);
  const opacity = Number.isFinite(parsedOpacity) ? parsedOpacity : 1;
  const height = Number.parseFloat(style.height) || 0;
  const paddingTop = Number.parseFloat(style.paddingTop) || 0;
  const paddingBottom = Number.parseFloat(style.paddingBottom) || 0;
  const marginTop = Number.parseFloat(style.marginTop) || 0;
  const marginBottom = Number.parseFloat(style.marginBottom) || 0;
  const borderTopWidth = Number.parseFloat(style.borderTopWidth) || 0;
  const borderBottomWidth = Number.parseFloat(style.borderBottomWidth) || 0;

  return {
    delay,
    duration,
    easing,
    css: (t) => {
      const progress = Math.max(0, t);
      return (
        'overflow-x: visible; overflow-y: clip;' +
        `opacity: ${Math.min(progress * 20, 1) * opacity};` +
        `height: ${progress * height}px;` +
        `padding-top: ${progress * paddingTop}px;` +
        `padding-bottom: ${progress * paddingBottom}px;` +
        `margin-top: ${progress * marginTop}px;` +
        `margin-bottom: ${progress * marginBottom}px;` +
        `border-top-width: ${progress * borderTopWidth}px;` +
        `border-bottom-width: ${progress * borderBottomWidth}px;` +
        'min-height: 0;'
      );
    },
  };
}
