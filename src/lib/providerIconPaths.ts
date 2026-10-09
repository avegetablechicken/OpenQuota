import antigravity from '../assets/provider-icons/antigravity.svg?raw';
import claude from '../assets/provider-icons/claude.svg?raw';
import codex from '../assets/provider-icons/codex.svg?raw';
import copilot from '../assets/provider-icons/copilot.svg?raw';
import cursor from '../assets/provider-icons/cursor.svg?raw';
import devin from '../assets/provider-icons/devin.svg?raw';
import grok from '../assets/provider-icons/grok.svg?raw';
import kimi from '../assets/provider-icons/kimi.svg?raw';
import minimax from '../assets/provider-icons/minimax.svg?raw';
import opencode from '../assets/provider-icons/opencode.svg?raw';
import openrouter from '../assets/provider-icons/openrouter.svg?raw';
import sub2api from '../assets/provider-icons/sub2api.svg?raw';
import zai from '../assets/provider-icons/zai.svg?raw';
import { get } from 'svelte/store';
import { sub2ApiUpstreams } from './sub2ApiUpstreams';
import type { Sub2ApiUpstream } from './types';

const visuals: Record<string, { source: string; color: string | null }> = {
  antigravity: { source: antigravity, color: '#4285F4' },
  claude: { source: claude, color: '#DE7356' },
  codex: { source: codex, color: null },
  copilot: { source: copilot, color: null },
  cursor: { source: cursor, color: null },
  devin: { source: devin, color: null },
  grok: { source: grok, color: null },
  kimi: { source: kimi, color: '#1783FF' },
  minimax: { source: minimax, color: '#E2167E' },
  opencode: { source: opencode, color: null },
  openrouter: { source: openrouter, color: null },
  sub2api: { source: sub2api, color: '#39D9E7' },
  zai: { source: zai, color: null },
};

const SUB2API_SPEND_COLOR_SLOTS = 8;
const sub2ApiColors: Record<Sub2ApiUpstream, readonly string[]> = {
  codex: ['#74CDA8', '#23543E', '#65A64B', '#B5D8A4', '#287F71', '#81956A', '#38B86A', '#567B6E'],
  claude: ['#F0B487', '#8E3F26', '#D89839', '#653D2B', '#E6C49D', '#C35D20', '#AC8368', '#B76D55'],
};

// Use the persistent account slot, so reordering or removing accounts cannot recolor others.
function sub2ApiColorSlot(providerId: string) {
  const match = providerId.match(/^sub2api(?:@(\d+))?$/);
  const ordinal = match?.[1] ? Number(match[1]) : 1;
  if (!Number.isSafeInteger(ordinal) || ordinal < 1) return 1;
  return ((ordinal - 1) % SUB2API_SPEND_COLOR_SLOTS) + 1;
}
export const OTHERS_SPEND_ID = 'others';
export const UNPRICED_OTHERS_SPEND_ID = 'others-unpriced';

export function providerFamily(providerId: string) {
  return providerId.split('@', 1)[0];
}

export function providerIconPath(providerId: string) {
  const source = visuals[providerFamily(providerId)]?.source;
  if (!source) return '';
  return [...source.matchAll(/<path\b[^>]*\bd="([^"]+)"/g)].map((match) => match[1]).join(' ');
}

export function providerIconColor(
  providerId: string,
  upstream: Sub2ApiUpstream | null | undefined = get(sub2ApiUpstreams)[providerId],
) {
  if (providerFamily(providerId) === 'sub2api' && upstream) {
    return sub2ApiColors[upstream][sub2ApiColorSlot(providerId) - 1];
  }
  return visuals[providerFamily(providerId)]?.color ?? null;
}

export function providerSpendColorVariable(
  providerId: string,
  upstream: Sub2ApiUpstream | undefined = get(sub2ApiUpstreams)[providerId],
) {
  if (providerId === OTHERS_SPEND_ID) return '--provider-others';
  if (providerId === UNPRICED_OTHERS_SPEND_ID) return '--provider-others-unpriced';
  const family = providerFamily(providerId);
  if (family !== 'sub2api') return `--provider-${family}`;
  const slot = sub2ApiColorSlot(providerId);
  const base = upstream ? `--provider-sub2api-${upstream}` : '--provider-sub2api';
  return slot === 1 ? base : `${base}-${slot}`;
}

export function providerIconViewBox(providerId: string) {
  return (
    visuals[providerFamily(providerId)]?.source.match(/viewBox="([^"]+)"/)?.[1] ?? '0 0 100 100'
  );
}
