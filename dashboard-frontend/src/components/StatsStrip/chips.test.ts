import { describe, it, expect } from 'vitest';
import type { EngineThroughputSample, InstantMetricSample } from '../../api/types';
import { CHIP_METRICS, deriveChips, deltaGlyph, ERROR_PCT_THRESHOLD } from './chips';

function win(over: Partial<InstantMetricSample> = {}): InstantMetricSample {
  // Default to a fully-measured window: the three denominators mirror `latency_samples` so a test
  // that sets `latency_samples: 0` (no finalized flow) also zeroes the tok/s + $/min denominators
  // unless it overrides them — keeping the gap-01 "unavailable" semantics intact. A test
  // that needs to diverge them (latency_samples > 0 but usage/priced = 0) passes them explicitly.
  const latency_samples = over.latency_samples ?? 252;
  return {
    interval_duration_ms: 1000, ready: true, accepted_requests: 252,
    accepted_per_sec: 4.2, active_streams_now: 3, failure_pct: 1.1,
    terminal_requests: latency_samples, terminal_per_sec: 4.2, successes: latency_samples,
    failures: 0, cancellations: 0, cancellation_pct: 0,
    p50_ms: 180, p95_ms: 920, p99_ms: 1840, reported_tokens_per_sec: 142, cost_per_min: 0.21,
    quantile_method: 'log_histogram_nearest_rank', max_relative_error: 0.062,
    latency_overflow_count: 0, p50_quality: 'measured', p95_quality: 'measured', p99_quality: 'measured', usage_anomaly_count: 0,
    latency_samples,
    usage_samples: latency_samples,
    priced_samples: latency_samples,
    cost_confidence: 'estimated',
    ...over,
  };
}

function engine(over: Partial<EngineThroughputSample> = {}): EngineThroughputSample {
  return {
    generated_tokens_per_sec: 321.4,
    sampled_at_ms: 10_000,
    measured_sources: 2,
    total_sources: 2,
    coverage: 'full',
    ...over,
  };
}

describe('chips', () => {
  it('emits one descriptor per chip metric in strip order', () => {
    const chips = deriveChips(win(), null);
    expect(chips.map((c) => c.key)).toEqual([...CHIP_METRICS]);
  });

  it('formats values (rate/ms/pct/tokens/money)', () => {
    const chips = deriveChips(win({ accepted_per_sec: 4.2, p95_ms: 920, failure_pct: 1.1, reported_tokens_per_sec: 1500, cost_per_min: 0.21 }), null);
    const byKey = Object.fromEntries(chips.map((c) => [c.key, c.value]));
    expect(byKey.accepted_per_sec).toBe('4.2');
    expect(byKey.p95_ms).toBe('920');
    expect(byKey.failure_pct).toBe('1.1');
    expect(byKey.reported_tokens_per_sec).toBe('1.5k'); // fmtTokens compaction
    expect(byKey.cost_per_min).toBe('0.21');
  });

  it('never formats a nonzero rate as zero', () => {
    const tiny = deriveChips(win({ accepted_per_sec: 0.001111, reported_tokens_per_sec: 31.109 }), null);
    const byKey = Object.fromEntries(tiny.map((chip) => [chip.key, chip.value]));
    expect(byKey.accepted_per_sec).toBe('0.0011');
    expect(byKey.reported_tokens_per_sec).toBe('31.1');
  });

  it('prefers physically de-duplicated engine generation throughput and labels the source', () => {
    const chip = deriveChips(
      win({ usage_samples: 0, reported_tokens_per_sec: null }),
      null,
      engine(),
    ).find((candidate) => candidate.key === 'reported_tokens_per_sec')!;
    expect(chip.label).toBe('engine gen tok/s');
    expect(chip.value).toBe('321');
    expect(chip.source).toBe('engine');
    expect(chip.quality).toBe('derived');
    expect(chip.details).toContain('2/2 physically distinct metrics sources');
    expect(chip.details).toContain('inverse mean per-request time-per-output-token');
    expect(chip.details).toContain('output tokens only');
    expect(chip.details).toContain('speculative accepted tokens are already reflected');
  });

  it('marks incomplete engine coverage partial and preserves a measured zero', () => {
    const chip = deriveChips(
      win(),
      null,
      engine({ generated_tokens_per_sec: 0, measured_sources: 1, total_sources: 3, coverage: 'partial' }),
    ).find((candidate) => candidate.key === 'reported_tokens_per_sec')!;
    expect(chip.value).toBe('0.0');
    expect(chip.quality).toBe('partial');
  });

  it('falls back to reported response usage when no engine interval is available', () => {
    const chip = deriveChips(win({ reported_tokens_per_sec: 142 }), null)
      .find((candidate) => candidate.key === 'reported_tokens_per_sec')!;
    expect(chip.label).toBe('reported tok/s');
    expect(chip.value).toBe('142');
    expect(chip.source).toBe('reported');
  });

  it('compares engine deltas only with the preceding engine interval', () => {
    const chip = deriveChips(win(), win(), engine({ generated_tokens_per_sec: 20 }), engine({ generated_tokens_per_sec: 10 }))
      .find((candidate) => candidate.key === 'reported_tokens_per_sec')!;
    expect(chip.delta).toBe('up');
  });

  it('renders "—" for every chip when there is no sample', () => {
    const chips = deriveChips(null, null);
    expect(chips.every((c) => c.value === '—')).toBe(true);
    expect(chips.every((c) => c.delta === 'flat')).toBe(true);
  });

  it('turns the err% chip red ONLY above the threshold', () => {
    const below = deriveChips(win({ failure_pct: ERROR_PCT_THRESHOLD - 0.1 }), null).find((c) => c.key === 'failure_pct')!;
    const above = deriveChips(win({ failure_pct: ERROR_PCT_THRESHOLD + 0.1 }), null).find((c) => c.key === 'failure_pct')!;
    expect(below.accent).not.toBe('down');
    expect(above.accent).toBe('down');
  });

  it('computes the delta direction vs. the previous sample', () => {
    const up = deriveChips(win({ accepted_per_sec: 5 }), win({ accepted_per_sec: 4 })).find((c) => c.key === 'accepted_per_sec')!;
    const down = deriveChips(win({ accepted_per_sec: 3 }), win({ accepted_per_sec: 4 })).find((c) => c.key === 'accepted_per_sec')!;
    const flat = deriveChips(win({ accepted_per_sec: 4 }), win({ accepted_per_sec: 4 })).find((c) => c.key === 'accepted_per_sec')!;
    expect(up.delta).toBe('up');
    expect(down.delta).toBe('down');
    expect(flat.delta).toBe('flat');
  });

  it('deltaGlyph maps direction → arrow', () => {
    expect(deltaGlyph('up')).toBe('▲');
    expect(deltaGlyph('down')).toBe('▼');
    expect(deltaGlyph('flat')).toBe('·');
  });

  // Gap 01 — don't lie with zeros.
  it('renders sample-derived metrics as UNAVAILABLE (—) when the window has zero latency_samples', () => {
    // A window with traffic in flight but NOTHING finalized: latency_samples 0, but req/s is a
    // genuine measured rate (and active_streams_now a live count). The numeric fields are 0
    // on the wire (no sample fed them) — they MUST render "—", never "0".
    const chips = deriveChips(win({ latency_samples: 0, failure_pct: 0, p50_ms: 0, p95_ms: 0, p99_ms: 0, reported_tokens_per_sec: 0, cost_per_min: 0, accepted_per_sec: 2.5, active_streams_now: 4 }), null);
    const byKey = Object.fromEntries(chips.map((c) => [c.key, c.value]));
    // Sample-derived metrics are unavailable (—), NOT a fabricated 0.
    expect(byKey.failure_pct).toBe('—');
    expect(byKey.p50_ms).toBe('—');
    expect(byKey.p95_ms).toBe('—');
    expect(byKey.p99_ms).toBe('—');
    expect(byKey.reported_tokens_per_sec).toBe('—');
    expect(byKey.cost_per_min).toBe('—');
    // req/s + active_streams_now are NOT sample-derived → they show real values.
    expect(byKey.accepted_per_sec).toBe('2.5');
    expect(byKey.active_streams_now).toBe('4.0');
  });

  it('distinguishes a GENUINE measured zero (latency_samples > 0) from unavailable', () => {
    // Real traffic finalized (latency_samples 12) that genuinely measured 0 latency/cost/err: those
    // are honest zeros and render numerically, NOT "—".
    const chips = deriveChips(win({ latency_samples: 12, failure_pct: 0, p50_ms: 0, cost_per_min: 0, accepted_per_sec: 0 }), null);
    const byKey = Object.fromEntries(chips.map((c) => [c.key, c.value]));
    expect(byKey.failure_pct).toBe('0.0');
    expect(byKey.p50_ms).toBe('0');
    expect(byKey.cost_per_min).toBe('0.00');
    expect(byKey.accepted_per_sec).toBe('0.0'); // genuine idle zero, also numeric
  });

  it('keeps a one-sample median visible but withholds misleading tail percentiles', () => {
    const chips = deriveChips(win({
      latency_samples: 1,
      p50_ms: 125,
      p95_ms: 125,
      p99_ms: 125,
      p50_quality: 'partial',
      p95_quality: 'partial',
      p99_quality: 'partial',
    }), null);
    const p50 = chips.find((candidate) => candidate.key === 'p50_ms')!;
    expect(p50.value).toBe('125');
    expect(p50.quality).toBe('partial');
    for (const key of ['p95_ms', 'p99_ms'] as const) {
      const chip = chips.find((candidate) => candidate.key === key)!;
      expect(chip.value).toBe('—');
      expect(chip.quality).toBe('unavailable');
      expect(chip.details).toContain('1 latency samples');
    }
  });

  it('an unavailable (zero-sample) window has a FLAT delta and no err% threshold accent', () => {
    const prev = win({ latency_samples: 10, failure_pct: 9.9 });
    const cur = win({ latency_samples: 0, failure_pct: 0, p50_ms: 0 });
    const chips = deriveChips(cur, prev);
    const err = chips.find((c) => c.key === 'failure_pct')!;
    const p50_ms = chips.find((c) => c.key === 'p50_ms')!;
    expect(err.value).toBe('—');
    expect(err.accent).not.toBe('down'); // an unavailable err% carries no threshold red
    expect(p50_ms.delta).toBe('flat'); // no trend for an unmeasurable value
  });

  // Gap 01 finding 3 — per-metric availability denominators diverge.
  it('renders tok/s + $/min as "—" when usage was not reported, even though latency IS measured', () => {
    // latency_samples 12 (latency measured) but usage_samples 0 (no flow reported tokens) and so
    // priced_samples 0 too. Latency/err% are real; tok/s + $/min are unmeasurable → "—".
    const chips = deriveChips(win({ latency_samples: 12, usage_samples: 0, priced_samples: 0, p50_ms: 200, reported_tokens_per_sec: 0, cost_per_min: 0 }), null);
    const byKey = Object.fromEntries(chips.map((c) => [c.key, c.value]));
    expect(byKey.p50_ms).toBe('200'); // latency measured (latency_samples > 0)
    expect(byKey.failure_pct).toBe('1.1');
    expect(byKey.reported_tokens_per_sec).toBe('—'); // no usage sample → unmeasurable
    expect(byKey.cost_per_min).toBe('—'); // no priced usage sample → unmeasurable
  });

  it('renders $/min as "—" when usage WAS reported but on an unpriced model (tok/s stays numeric)', () => {
    // usage_samples 8 (tok/s measurable) but priced_samples 0 (only unpriced models) →
    // $/min is unmeasurable ("—"), distinct from a genuine $0.00. tok/s renders normally.
    const chips = deriveChips(win({ latency_samples: 8, usage_samples: 8, priced_samples: 0, reported_tokens_per_sec: 142, cost_per_min: 0 }), null);
    const byKey = Object.fromEntries(chips.map((c) => [c.key, c.value]));
    expect(byKey.reported_tokens_per_sec).toBe('142'); // usage present → measurable
    expect(byKey.cost_per_min).toBe('—'); // no priced sample → unavailable, not $0.00
  });

  // Gap 01 finding 4 — provenance/quality on every chip state.
  it('tags each chip with measured/derived/estimated provenance when available', () => {
    const chips = deriveChips(win(), null);
    const byKey = Object.fromEntries(chips.map((c) => [c.key, c.quality]));
    expect(byKey.accepted_per_sec).toBe('measured');
    expect(byKey.active_streams_now).toBe('measured');
    expect(byKey.failure_pct).toBe('derived');
    expect(byKey.p50_ms).toBe('derived');
    expect(byKey.p95_ms).toBe('derived');
    expect(byKey.p99_ms).toBe('derived');
    expect(byKey.reported_tokens_per_sec).toBe('derived');
    expect(byKey.cost_per_min).toBe('estimated'); // priced → estimated, surfaced as such
  });

  it('tags an unmeasurable metric as "unavailable" while measured ones keep their tier', () => {
    // latency_samples 0 → latency/err%/tok-s/$/min all unavailable; req/s + active stay measured.
    const chips = deriveChips(win({ latency_samples: 0 }), null);
    const byKey = Object.fromEntries(chips.map((c) => [c.key, c.quality]));
    expect(byKey.accepted_per_sec).toBe('measured');
    expect(byKey.active_streams_now).toBe('measured');
    expect(byKey.failure_pct).toBe('unavailable');
    expect(byKey.p50_ms).toBe('unavailable');
    expect(byKey.reported_tokens_per_sec).toBe('unavailable');
    expect(byKey.cost_per_min).toBe('unavailable');
  });

  it('tags cost as unavailable but tok/s as derived when only pricing is missing', () => {
    const chips = deriveChips(win({ latency_samples: 8, usage_samples: 8, priced_samples: 0 }), null);
    const byKey = Object.fromEntries(chips.map((c) => [c.key, c.quality]));
    expect(byKey.reported_tokens_per_sec).toBe('derived'); // usage present
    expect(byKey.cost_per_min).toBe('unavailable'); // unpriced → not an estimate, a gap
  });

  it('every chip carries a quality tag from the closed set (no chip is untagged)', () => {
    const valid = new Set(['measured', 'derived', 'estimated', 'unavailable']);
    for (const cur of [win(), win({ latency_samples: 0 }), null]) {
      for (const chip of deriveChips(cur, null)) {
        expect(valid.has(chip.quality), `${chip.key} → ${chip.quality}`).toBe(true);
      }
    }
  });

  // Gap 07 review round 1, finding 5 — the $/min chip's quality/label is DERIVED from the
  // backend aggregate `cost_confidence`, not a hard-coded `estimated`. Operators can now tell a
  // confident aggregate cost from an estimated one.
  it('a CONFIDENT aggregate $/min reads as "derived", not "estimated"', () => {
    const chip = deriveChips(win({ cost_confidence: 'confident' }), null).find((c) => c.key === 'cost_per_min')!;
    expect(chip.value).toBe('0.21'); // a real, rendered number
    expect(chip.quality).toBe('derived'); // confident ⇒ a real computed cost, not a modelled estimate
  });

  it('an ESTIMATED aggregate $/min stays labelled "estimated"', () => {
    const chip = deriveChips(win({ cost_confidence: 'estimated' }), null).find((c) => c.key === 'cost_per_min')!;
    expect(chip.value).toBe('0.21');
    expect(chip.quality).toBe('estimated'); // a priced bucket bills cached at the default 0.0 (or an unpriced bucket bears usage)
  });

  it('an UNAVAILABLE cost ($/min) renders "—" and is tagged unavailable regardless of the cost_confidence value', () => {
    // priced_samples 0 ⇒ the denominator branch makes $/min unavailable; the cost_confidence the
    // backend pairs with that is `unavailable` too, but even a stray non-unavailable tag cannot
    // resurrect a value (the denominator wins).
    const chip = deriveChips(win({ latency_samples: 8, usage_samples: 8, priced_samples: 0, cost_confidence: 'unavailable' }), null)
      .find((c) => c.key === 'cost_per_min')!;
    expect(chip.value).toBe('—');
    expect(chip.quality).toBe('unavailable');
  });

  it('the $/min cost_confidence override does NOT affect the tok/s chip (only the cost chip)', () => {
    // tok/s keeps its intrinsic `derived` tier even when the window cost is confident.
    const toks = deriveChips(win({ cost_confidence: 'confident' }), null).find((c) => c.key === 'reported_tokens_per_sec')!;
    expect(toks.quality).toBe('derived');
  });
});
