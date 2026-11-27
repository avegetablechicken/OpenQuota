import { get } from 'svelte/store';
import { afterEach, describe, expect, it } from 'vitest';
import {
  forgetSub2ApiEndpoint,
  rememberSub2ApiEndpoint,
  showSub2ApiMetric,
  sub2ApiDisplayName,
  sub2ApiEndpoints,
  sub2ApiPublicName,
  sub2ApiUpstreamAccountName,
  sub2ApiUpstreams,
  syncSub2ApiTracks,
} from './sub2ApiUpstreams';
import { codexState } from '../test/appFixtures';

describe('Sub2API connection labels', () => {
  afterEach(() => forgetSub2ApiEndpoint('sub2api@2'));

  it.each([
    ['https://quota.example.com/api/v1', 'quota.example.com'],
    ['http://10.0.0.8:6060', '10.0.0.8:6060'],
  ])('keeps only the host from %s', (baseUrl, endpoint) => {
    rememberSub2ApiEndpoint('sub2api@2', baseUrl);

    expect(get(sub2ApiEndpoints)['sub2api@2']).toBe(endpoint);
  });

  it('removes the endpoint when the connection is cleared', () => {
    rememberSub2ApiEndpoint('sub2api@2', 'https://quota.example.com');
    rememberSub2ApiEndpoint('sub2api@2', '');

    expect(get(sub2ApiEndpoints)['sub2api@2']).toBeUndefined();
  });

  it('takes each connection Track from the settings view state', () => {
    const policy = (track: 'claude' | null) => ({
      track,
      visibleMetricIds: [],
      starrableMetricIds: [],
      maxStars: 2,
    });
    syncSub2ApiTracks({
      'sub2api@2': policy('claude'),
      sub2api: policy(null),
      codex: policy(null),
    });
    expect(get(sub2ApiUpstreams)).toEqual({ 'sub2api@2': 'claude' });
    syncSub2ApiTracks({ 'sub2api@2': policy(null) });
    expect(get(sub2ApiUpstreams)).toEqual({});
  });

  it('uses one public name for every internal slot', () => {
    expect(sub2ApiDisplayName('sub2api')).toBe('Sub2API');
    expect(sub2ApiDisplayName('sub2api@2')).toBe(`Sub2API ${2}`);
    expect(sub2ApiDisplayName('sub2api@3')).toBe(`Sub2API ${3}`);
    expect(sub2ApiDisplayName('sub2api@2', 'Work')).toBe('Work');
    expect(sub2ApiDisplayName('codex')).toBeNull();
  });

  it('names Antigravity and Grok upstream accounts', () => {
    expect(sub2ApiPublicName('antigravity')).toBe('Sub2API · Antigravity');
    expect(sub2ApiPublicName('grok')).toBe('Sub2API · Grok');
    expect(sub2ApiUpstreamAccountName('Sub2API · Antigravity · agy')).toBe('agy');
    expect(sub2ApiUpstreamAccountName('Sub2API · Grok · grok')).toBe('grok');
  });

  it('keeps percentage monthly quotas on flattened Grok admin account snapshots', () => {
    const childSnapshot = {
      ...structuredClone(codexState.snapshot!),
      providerId: 'sub2api@2',
      accounts: undefined,
      quotas: [
        {
          ...codexState.snapshot!.quotas[0],
          id: 'monthly',
          label: 'Monthly',
          format: 'percent' as const,
          usedPercent: 35,
          limitValue: null,
        },
      ],
    };

    const grok = {
      track: 'grok' as const,
      visibleMetricIds: ['sub2api@2.monthly'],
      starrableMetricIds: ['sub2api@2.monthly'],
      maxStars: 2,
    };
    expect(showSub2ApiMetric(childSnapshot, 'sub2api@2.monthly', grok, false)).toBe(true);
    expect(showSub2ApiMetric(childSnapshot, 'sub2api@2.monthly', grok, true)).toBe(false);
  });
});
