import { describe, it, expect } from 'vitest';
import {
  clientCell,
  clientKey,
  clientRollup,
  fmtLatency,
  fmtRate,
  sourceStrength,
  strengthQuality,
  UNAVAILABLE,
} from './clientAttribution';
import type { FlowSummary } from '../../api/types';

/**
 * clientAttribution (gap 15) PURE model. Asserts the cross-cutting rules the column + panel + filter
 * all depend on: source-STRENGTH DQ tags (key-hash/configured-id = strong/measured; user-agent =
 * weak/derived), don't-lie-with-zeros (absent ⇒ `—`/unavailable, NEVER a fabricated id), and the
 * per-client cost/errors/latency roll-up (priced/timed denominators, strongest-source-wins grouping).
 */
function flow(over: Partial<FlowSummary> = {}): FlowSummary {
  return {
    api_call_id: `api_${Math.random().toString(36).slice(2, 8)}`,
    method: 'POST',
    uri: '/v1/responses',
    status: 'completed',
    started_ms: 1_700_000_000_000,
    cost_confidence: 'unavailable',
    ...over,
  };
}

describe('sourceStrength / strengthQuality — source-strength DQ tags', () => {
  it('key-hash + configured-id are STRONG (measured); user-agent is WEAK (derived); absent is unavailable', () => {
    expect(sourceStrength('key_hash')).toBe('strong');
    expect(sourceStrength('configured_header')).toBe('strong');
    expect(sourceStrength('user_agent')).toBe('weak');
    expect(sourceStrength(null)).toBe('unavailable');
    expect(sourceStrength(undefined)).toBe('unavailable');

    expect(strengthQuality('strong')).toBe('measured');
    expect(strengthQuality('weak')).toBe('derived');
    expect(strengthQuality('unavailable')).toBe('unavailable');
  });
});

describe('clientCell — per-row CLIENT cell', () => {
  it('a key-hash is a STRONG measured identity (label = the hash prefix, never a raw key)', () => {
    const cell = clientCell(flow({ client_label: 'key-9f3a1c0b2d4e', client_source: 'key_hash' }));
    expect(cell.attributed).toBe(true);
    expect(cell.label).toBe('key-9f3a1c0b2d4e');
    expect(cell.strength).toBe('strong');
    expect(cell.quality).toBe('measured');
    expect(cell.weak).toBe(false);
    expect(cell.badge).toBe('key');
  });

  it('a configured-id is a STRONG measured identity', () => {
    const cell = clientCell(flow({ client_label: 'svc-checkout', client_source: 'configured_header' }));
    expect(cell.strength).toBe('strong');
    expect(cell.quality).toBe('measured');
    expect(cell.weak).toBe(false);
    expect(cell.badge).toBe('id');
  });

  it('a user-agent is a WEAK derived fallback (visibly weaker, NOT an identity claim)', () => {
    const cell = clientCell(flow({ client_label: 'python-httpx/0.27', client_source: 'user_agent' }));
    expect(cell.attributed).toBe(true);
    expect(cell.label).toBe('python-httpx/0.27');
    expect(cell.strength).toBe('weak');
    expect(cell.quality).toBe('derived'); // derived, NOT measured — the key distinction
    expect(cell.weak).toBe(true);
    expect(cell.badge).toBe('ua');
  });

  it('NO attribution ⇒ — (unavailable), NEVER a fabricated id (don\'t-lie-with-zeros)', () => {
    const absent = clientCell(flow({ client_label: null, client_source: null }));
    expect(absent.attributed).toBe(false);
    expect(absent.label).toBe(UNAVAILABLE);
    expect(absent.quality).toBe('unavailable');
    expect(absent.badge).toBeNull();
    // A blank/whitespace label is treated as absent too (not rendered as "").
    const blank = clientCell(flow({ client_label: '   ', client_source: 'user_agent' }));
    expect(blank.attributed).toBe(false);
    expect(blank.label).toBe(UNAVAILABLE);
  });

  it('a present label with NO source is still a real label but the weakest tagged tier (never a confirmed identity)', () => {
    const cell = clientCell(flow({ client_label: 'mystery', client_source: null }));
    expect(cell.attributed).toBe(true);
    expect(cell.label).toBe('mystery');
    expect(cell.strength).toBe('unavailable'); // no strength signal ⇒ not strong
    expect(cell.quality).toBe('unavailable');
    expect(cell.weak).toBe(false);
  });
});

describe('clientKey', () => {
  it('is the trimmed client_label, or null when absent/blank', () => {
    expect(clientKey(flow({ client_label: 'key-abc' }))).toBe('key-abc');
    expect(clientKey(flow({ client_label: '  key-abc  ' }))).toBe('key-abc');
    expect(clientKey(flow({ client_label: null }))).toBeNull();
    expect(clientKey(flow({ client_label: '' }))).toBeNull();
  });
});

describe('clientRollup — aggregate cost / errors / latency by client', () => {
  it('empty / all-unattributed input ⇒ available:false (every figure —, never 0)', () => {
    expect(clientRollup([]).available).toBe(false);
    const m = clientRollup([flow({ client_label: null }), flow({ client_label: null })]);
    expect(m.available).toBe(false);
    expect(m.rows).toEqual([]);
    expect(m.totalFlows).toBe(2);
    expect(m.unattributedFlows).toBe(2); // counted explicitly, not invented as a client
  });

  it('groups multiple flows under one client; sums priced cost + means timed latency (measured/derived)', () => {
    const m = clientRollup([
      flow({ client_label: 'key-A', client_source: 'key_hash', status: 'completed', cost: 0.006, elapsed_ms: 2400 }),
      flow({ client_label: 'key-A', client_source: 'key_hash', status: 'completed', cost: 0.002, elapsed_ms: 4200 }),
    ]);
    expect(m.available).toBe(true);
    expect(m.rows.length).toBe(1);
    const row = m.rows[0]!;
    expect(row.key).toBe('key-A');
    expect(row.total).toBe(2);
    expect(row.failed).toBe(0);
    expect(row.errorRateText).toBe('0%'); // measured-base zero, NOT —
    // cost summed (priced), latency averaged (timed).
    expect(row.cost).toBeCloseTo(0.008, 6);
    expect(row.costQuality).toBe('measured');
    expect(row.pricedFlows).toBe(2);
    expect(row.avgLatencyMs).toBe(3300);
    expect(row.latencyQuality).toBe('derived');
    expect(row.strength).toBe('strong');
  });

  it('derives a per-client error rate; an unpriced/untimed client reads — (never a fabricated $0.00 / 0ms)', () => {
    const m = clientRollup([
      // svc-checkout: 1 of 1 failed ⇒ 100%; no cost (null) + no elapsed ⇒ cost/latency unavailable.
      flow({ client_label: 'svc-checkout', client_source: 'configured_header', status: 'failed', cost: null, elapsed_ms: null }),
    ]);
    const row = m.rows[0]!;
    expect(row.errorRatePct).toBe(100);
    expect(row.errorRateText).toBe('100%');
    expect(row.cost).toBeNull();
    expect(row.costQuality).toBe('unavailable');
    expect(row.avgLatencyMs).toBeNull();
    expect(row.latencyQuality).toBe('unavailable');
  });

  it('the WEAK user-agent client is tagged derived; a key-hash client is tagged measured', () => {
    const m = clientRollup([
      flow({ client_label: 'python-httpx/0.27', client_source: 'user_agent', status: 'completed' }),
      flow({ client_label: 'key-A', client_source: 'key_hash', status: 'completed' }),
    ]);
    const ua = m.rows.find((r) => r.key === 'python-httpx/0.27')!;
    const key = m.rows.find((r) => r.key === 'key-A')!;
    expect(ua.weak).toBe(true);
    expect(ua.attributionQuality).toBe('derived');
    expect(key.weak).toBe(false);
    expect(key.attributionQuality).toBe('measured');
  });

  it('the STRONGEST source a client ever presented wins its row tag (key-hash beats a later UA sighting)', () => {
    const m = clientRollup([
      flow({ client_label: 'dual', client_source: 'user_agent', status: 'completed' }),
      flow({ client_label: 'dual', client_source: 'key_hash', status: 'completed' }),
    ]);
    expect(m.rows.length).toBe(1);
    expect(m.rows[0]!.source).toBe('key_hash');
    expect(m.rows[0]!.strength).toBe('strong'); // not weak — the strong sighting wins
  });

  it('orders clients by observed flow count desc; counts unattributed separately', () => {
    const m = clientRollup([
      flow({ client_label: 'busy', client_source: 'key_hash' }),
      flow({ client_label: 'busy', client_source: 'key_hash' }),
      flow({ client_label: 'quiet', client_source: 'key_hash' }),
      flow({ client_label: null }), // unattributed
    ]);
    expect(m.rows.map((r) => r.key)).toEqual(['busy', 'quiet']);
    expect(m.totalFlows).toBe(4);
    expect(m.unattributedFlows).toBe(1);
  });
});

describe('fmtRate / fmtLatency — formatting + don\'t-lie-with-zeros', () => {
  it('a measured-base 0 reads 0%; null/non-finite reads — (unavailable)', () => {
    expect(fmtRate(0)).toBe('0%');
    expect(fmtRate(4.2)).toBe('4.2%');
    expect(fmtRate(50)).toBe('50%');
    expect(fmtRate(null)).toBe(UNAVAILABLE);
    expect(fmtRate(NaN)).toBe(UNAVAILABLE);
  });

  it('latency renders ms under a second, s above; null reads —', () => {
    expect(fmtLatency(820)).toBe('820ms');
    expect(fmtLatency(2400)).toBe('2.4s');
    expect(fmtLatency(null)).toBe(UNAVAILABLE);
  });
});
