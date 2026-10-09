import { describe, expect, it } from 'vitest';
import tokensCss from '../styles/tokens.css?raw';
import { providerIconColor, providerSpendColorVariable } from './providerIconPaths';
import { forgetSub2ApiUpstream, rememberSub2ApiUpstream } from './sub2ApiUpstreams';

describe('Sub2API account colors', () => {
  it.each(['codex', 'claude'] as const)(
    'gives all eight %s accounts distinct, matching icon and chart colors',
    (upstream) => {
      const colors = Array.from({ length: 8 }, (_, index) => {
        const id = index === 0 ? 'sub2api' : `sub2api@${index + 1}`;
        const color = providerIconColor(id, upstream)!;
        const variable = providerSpendColorVariable(id, upstream);
        expect(variable).toContain(`--provider-sub2api-${upstream}`);
        expect(tokensCss).toContain(`${variable}: ${color.toLowerCase()};`);
        return color;
      });
      expect(new Set(colors).size).toBe(8);
    },
  );

  it('keeps a remembered account color stable when other accounts change', () => {
    try {
      rememberSub2ApiUpstream('sub2api@2', 'claude');
      const icon = providerIconColor('sub2api@2');
      const chart = providerSpendColorVariable('sub2api@2');
      expect(icon).toBe(providerIconColor('sub2api@2', 'claude'));
      expect(chart).toBe('--provider-sub2api-claude-2');
      rememberSub2ApiUpstream('sub2api', 'claude');
      expect(providerIconColor('sub2api@2')).toBe(icon);
      forgetSub2ApiUpstream('sub2api');
      expect(providerSpendColorVariable('sub2api@2')).toBe(chart);
      expect(providerIconColor('sub2api@2')).toBe(icon);
    } finally {
      forgetSub2ApiUpstream('sub2api');
      forgetSub2ApiUpstream('sub2api@2');
    }
  });
});
