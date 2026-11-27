import { describe, expect, it } from 'vitest';
import tokensCss from '../styles/tokens.css?raw';
import { providerIconColor, providerSpendColorVariable } from './providerIconPaths';
import { clearSub2ApiTrack, setSub2ApiTrack } from '../test/sub2ApiTracks';

describe('Sub2API account colors', () => {
  it.each(['codex', 'claude', 'antigravity', 'grok'] as const)(
    'gives all eight %s accounts distinct chart colors while preserving the original icon color',
    (upstream) => {
      const colors = Array.from({ length: 8 }, (_, index) => {
        const id = index === 0 ? 'sub2api' : `sub2api@${index + 1}`;
        const variable = providerSpendColorVariable(id, upstream);
        expect(variable).toContain(`--provider-sub2api-${upstream}`);
        const color = tokensCss.match(new RegExp(`${variable}: (#[a-f0-9]+);`))?.[1];
        expect(color).toBeDefined();
        expect(providerIconColor(id)).toBe('#39D9E7');
        return color;
      });
      expect(new Set(colors).size).toBe(8);
    },
  );

  it('keeps a remembered account color stable when other accounts change', () => {
    try {
      setSub2ApiTrack('sub2api@2', 'claude');
      const icon = providerIconColor('sub2api@2');
      const chart = providerSpendColorVariable('sub2api@2');
      expect(icon).toBe('#39D9E7');
      expect(chart).toBe('--provider-sub2api-claude-2');
      setSub2ApiTrack('sub2api', 'claude');
      expect(providerIconColor('sub2api@2')).toBe(icon);
      clearSub2ApiTrack('sub2api');
      expect(providerSpendColorVariable('sub2api@2')).toBe(chart);
      expect(providerIconColor('sub2api@2')).toBe(icon);
    } finally {
      clearSub2ApiTrack('sub2api');
      clearSub2ApiTrack('sub2api@2');
    }
  });
});
