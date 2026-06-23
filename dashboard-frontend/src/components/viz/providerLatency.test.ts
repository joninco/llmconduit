import { describe, it, expect } from 'vitest';
import {
  buildProviderLatency,
  providerNodeEmphasis,
  providerLabelFor,
  fmtErrorRate,
  fmtProviderLatencyMs,
  OVERFLOW_PROVIDER_KEY,
  UNKNOWN_PROVIDER_KEY,
  DEGRADING_ERROR_RATE_PCT,
  DEGRADING_P99_MS,
} from './providerLatency';
import type { ProviderLatency } from '../../api/types';

/** A populated per-provider DTO (mirrors the real Rust `ProviderLatency`); override per-test. */
function per(over: Partial<ProviderLatency> = {}): ProviderLatency {
  return {
    provider: 'vllm-a',
    data_quality: 'derived',
    samples: 50,
    served: 48,
    failed: 2,
    p50: 82,
    p95: 190,
    p99: 240,
    error_rate: 4,
    errors: { http_status: 2 },
    ...over,
  };
}

describe('providerLatency — formatters (don\'t-lie-with-zeros)', () => {
  it('fmtProviderLatencyMs rounds a derived latency, — when null/non-finite', () => {
    expect(fmtProviderLatencyMs(82.4)).toBe('82ms');
    expect(fmtProviderLatencyMs(null)).toBe('—');
    expect(fmtProviderLatencyMs(Number.NaN)).toBe('—');
  });

  it('fmtErrorRate renders a MEASURED 0 as 0% (distinct from the unavailable —)', () => {
    expect(fmtErrorRate(0)).toBe('0%'); // all-served measured zero — NOT —
    expect(fmtErrorRate(4.2)).toBe('4.2%');
    expect(fmtErrorRate(33.33)).toBe('33%');
    expect(fmtErrorRate(null)).toBe('—'); // unavailable — distinct from a measured 0%
  });
});

describe('providerLatency — buildProviderLatency present (a derived measurement)', () => {
  it('tags percentiles derived + the error rate measured, with samples context', () => {
    const m = buildProviderLatency(per());
    expect(m.available).toBe(true);
    expect(m.p50).toEqual({ text: '82ms', quality: 'derived' });
    expect(m.p95).toEqual({ text: '190ms', quality: 'derived' });
    expect(m.p99).toEqual({ text: '240ms', quality: 'derived' });
    expect(m.errorRate).toEqual({ text: '4.0%', quality: 'measured' });
    expect(m.samplesText).toBe('48/50');
  });

  it('an all-served provider reports a MEASURED 0% (not absent/—)', () => {
    const m = buildProviderLatency(per({ served: 50, failed: 0, error_rate: 0, errors: {} }));
    expect(m.available).toBe(true);
    expect(m.errorRate.text).toBe('0%');
    expect(m.errorRate.quality).toBe('measured'); // a real measured zero, distinct from unavailable
    expect(m.errors).toEqual([]);
  });

  it('lists ONLY the error classes that occurred, in taxonomy order', () => {
    const m = buildProviderLatency(
      per({ failed: 6, errors: { timeout: 1, connect: 2, http_status: 3 } }),
    );
    // ATTEMPT_ERROR_CLASSES order is connect, http_status, timeout, ... (absent classes omitted).
    expect(m.errors.map((e) => e.class)).toEqual(['connect', 'http_status', 'timeout']);
    expect(m.errors.map((e) => e.count)).toEqual([2, 3, 1]);
    expect(m.errors.find((e) => e.class === 'http_status')?.label).toBe('http status');
  });
});

describe('providerLatency — buildProviderLatency absent (no in-window samples)', () => {
  it('a zero-sample provider is UNAVAILABLE everywhere — — never 0ms/0%', () => {
    const m = buildProviderLatency(null, 'never-seen');
    expect(m.available).toBe(false);
    expect(m.providerLabel).toBe('never-seen'); // named via the hint even with no data
    for (const f of [m.p50, m.p95, m.p99, m.errorRate]) {
      expect(f.text).toBe('—');
      expect(f.quality).toBe('unavailable');
    }
    expect(m.samplesText).toBe('—');
    expect(m.errors).toEqual([]);
  });

  it('undefined per behaves identically to null (absent)', () => {
    expect(buildProviderLatency(undefined, 'p').available).toBe(false);
  });
});

describe('providerLatency — __other__ / unknown overflow keys are surfaced honestly', () => {
  it('labels the __other__ overflow bucket (not hidden)', () => {
    expect(providerLabelFor(OVERFLOW_PROVIDER_KEY)).toBe('other providers (overflow)');
    const m = buildProviderLatency(per({ provider: OVERFLOW_PROVIDER_KEY }));
    expect(m.isOverflow).toBe(true);
    expect(m.providerLabel).toBe('other providers (overflow)');
  });

  it('labels the unknown provider sentinel', () => {
    expect(providerLabelFor(UNKNOWN_PROVIDER_KEY)).toBe('unknown provider');
    const m = buildProviderLatency(per({ provider: UNKNOWN_PROVIDER_KEY }));
    expect(m.isUnknown).toBe(true);
    expect(m.providerLabel).toBe('unknown provider');
  });
});

describe('providerLatency — providerNodeEmphasis (node sizing/color)', () => {
  it('absent ⇒ NEUTRAL: base size, no ring, errorRatePct null (not 0)', () => {
    const e = providerNodeEmphasis(null);
    expect(e.state).toBe('unavailable');
    expect(e.sizeScale).toBe(1); // never 0-sized
    expect(e.showErrorRing).toBe(false);
    expect(e.errorRatePct).toBeNull(); // not a fabricated 0
  });

  it('low error rate ⇒ nominal: base size, no ring', () => {
    const e = providerNodeEmphasis(per({ error_rate: 2 }));
    expect(e.state).toBe('nominal');
    expect(e.sizeScale).toBe(1);
    expect(e.showErrorRing).toBe(false);
    expect(e.errorRatePct).toBe(2);
  });

  it('an all-served provider is nominal but DISTINCT from absent (measured 0%)', () => {
    const e = providerNodeEmphasis(per({ served: 50, failed: 0, error_rate: 0 }));
    expect(e.state).toBe('nominal'); // has data → not unavailable
    expect(e.errorRatePct).toBe(0); // a measured 0, not null
  });

  it('elevated error rate ⇒ degrading: enlarged + an error ring', () => {
    const e = providerNodeEmphasis(per({ error_rate: DEGRADING_ERROR_RATE_PCT + 5 }));
    expect(e.state).toBe('degrading');
    expect(e.sizeScale).toBeGreaterThan(1);
    expect(e.showErrorRing).toBe(true);
  });

  it('the degrading size scale is BOUNDED (one hot provider cannot dominate)', () => {
    const e = providerNodeEmphasis(per({ error_rate: 100 }));
    expect(e.sizeScale).toBeLessThanOrEqual(1.5);
  });

  // The spec requires node sizing/color to reflect per-provider LATENCY, not just errors.
  it('a slow-but-NO-errors provider (high p99, measured 0%) is degrading on LATENCY — enlarged, NO error ring', () => {
    const e = providerNodeEmphasis(per({ served: 50, failed: 0, error_rate: 0, p50: 400, p95: 1800, p99: DEGRADING_P99_MS + 500 }));
    expect(e.state).toBe('degrading'); // flagged despite 0% errors
    expect(e.latencyDegraded).toBe(true);
    expect(e.sizeScale).toBeGreaterThan(1); // enlarged by latency
    expect(e.showErrorRing).toBe(false); // no failures → no red error ring
    expect(e.errorRatePct).toBe(0); // a MEASURED 0% (not absent)
    expect(e.p99Ms).toBe(DEGRADING_P99_MS + 500);
  });

  it('a fast provider with no errors is nominal (low p99 below the latency threshold)', () => {
    const e = providerNodeEmphasis(per({ error_rate: 0, p99: DEGRADING_P99_MS - 1 }));
    expect(e.state).toBe('nominal');
    expect(e.latencyDegraded).toBe(false);
    expect(e.sizeScale).toBe(1);
  });

  it('BOTH elevated error + latency ⇒ degrading with the error ring (error emphasis wins for the ring)', () => {
    const e = providerNodeEmphasis(per({ error_rate: DEGRADING_ERROR_RATE_PCT + 5, p99: DEGRADING_P99_MS + 1000 }));
    expect(e.state).toBe('degrading');
    expect(e.latencyDegraded).toBe(true);
    expect(e.showErrorRing).toBe(true);
    expect(e.sizeScale).toBeLessThanOrEqual(1.5); // still bounded
  });

  it('an ABSENT provider gets NO fabricated latency emphasis (don\'t-lie-with-zeros)', () => {
    const e = providerNodeEmphasis(null);
    expect(e.state).toBe('unavailable');
    expect(e.latencyDegraded).toBe(false);
    expect(e.p99Ms).toBeNull(); // not a fabricated 0
    expect(e.sizeScale).toBe(1);
  });
});
