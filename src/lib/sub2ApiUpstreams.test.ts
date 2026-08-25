import { get } from 'svelte/store';
import { afterEach, describe, expect, it } from 'vitest';
import {
  forgetSub2ApiUpstream,
  rememberSub2ApiUpstream,
  sub2ApiDisplayName,
  sub2ApiEndpoints,
  sub2ApiMetricSupported,
} from './sub2ApiUpstreams';

describe('Sub2API connection labels', () => {
  afterEach(() => forgetSub2ApiUpstream('sub2api@2'));

  it.each([
    ['https://quota.example.com/api/v1', 'quota.example.com'],
    ['http://10.0.0.8:6060', '10.0.0.8:6060'],
  ])('keeps only the host from %s', (baseUrl, endpoint) => {
    rememberSub2ApiUpstream('sub2api@2', 'claude', baseUrl);

    expect(get(sub2ApiEndpoints)['sub2api@2']).toBe(endpoint);
  });

  it('removes the endpoint when the connection is cleared', () => {
    rememberSub2ApiUpstream('sub2api@2', 'claude', 'https://quota.example.com');
    rememberSub2ApiUpstream('sub2api@2', 'codex', '');

    expect(get(sub2ApiEndpoints)['sub2api@2']).toBeUndefined();
  });

  it('uses one public name for every internal slot', () => {
    expect(sub2ApiDisplayName('sub2api')).toBe('Sub2API');
    expect(sub2ApiDisplayName('sub2api@2')).toBe(`Sub2API ${2}`);
    expect(sub2ApiDisplayName('sub2api@3')).toBe(`Sub2API ${3}`);
    expect(sub2ApiDisplayName('sub2api@2', 'Work')).toBe('Work');
    expect(sub2ApiDisplayName('codex')).toBeNull();
  });

  it('separates unsupported metrics from supported metrics that default off', () => {
    expect(sub2ApiMetricSupported('sub2api@2', 'sub2api@2.spark', 'claude')).toBe(false);
    expect(sub2ApiMetricSupported('sub2api@2', 'sub2api@2.rateLimitResets', 'claude')).toBe(false);
    expect(sub2ApiMetricSupported('sub2api@2', 'sub2api@2.sonnet', 'claude')).toBe(true);
    expect(sub2ApiMetricSupported('sub2api@2', 'sub2api@2.extra', 'claude')).toBe(false);
    expect(sub2ApiMetricSupported('sub2api@2', 'sub2api@2.sonnet', 'codex')).toBe(false);
    expect(sub2ApiMetricSupported('sub2api@2', 'sub2api@2.extra', 'codex')).toBe(false);
    expect(sub2ApiMetricSupported('sub2api@2', 'sub2api@2.spark', 'codex')).toBe(true);
    expect(sub2ApiMetricSupported('codex', 'codex.spark', 'codex')).toBe(true);
  });
});
