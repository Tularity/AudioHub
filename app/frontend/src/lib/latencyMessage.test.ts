import { describe, expect, it } from 'vitest';
import { confidenceKey, readLatency } from './metrics';
import { enUS as en } from '../i18n/en-US';
import { zhCN as zh } from '../i18n/zh-CN';

describe('unavailable latency does not assert a peer version', () => {
  it('keeps missing measurements unknown even when peer stages are present', () => {
    const reading = readLatency({ id: 1, peer_fingerprint: 'same-version', kind: 'spk', dir: 'send',
      stats: { pipeline: { side: 'send', stages: [], peer_stages: [],
        local_ms: null, peer_local_ms: null, net_ms: 0.5, sum_ms: null,
        confidence: 'unavailable' } } });
    expect(reading?.confidence).toBe('unavailable');
    expect(reading?.totalMs).toBeUndefined();
    const key = confidenceKey('unavailable');
    expect(en[key]).toContain('Incomplete measurements');
    expect(en[key]).not.toMatch(/old|version/i);
    expect(zh[key]).toContain('测量数据不完整');
    expect(zh[key]).not.toMatch(/版本|较旧/);
  });
});
