import { get } from 'svelte/store';
import { afterEach, describe, expect, it } from 'vitest';
import {
  forgetSub2ApiUpstream,
  rememberSub2ApiUpstream,
  sub2ApiDisplayName,
  sub2ApiEndpoints,
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
    expect(sub2ApiDisplayName('sub2api', 'codex')).toBe('Sub2API · Codex');
    expect(sub2ApiDisplayName('sub2api@2', 'claude')).toBe('Sub2API · Claude');
    expect(sub2ApiDisplayName('codex', 'codex')).toBeNull();
  });
});
