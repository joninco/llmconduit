import { describe, it, expect } from 'vitest';
import { attemptTrace } from './attemptTrace';
import type { Attempt } from '../../api/types';

/**
 * attemptTrace (gap 11) — the pure failover/attempt-trace model. The oracle is spec 11's
 * acceptance: a node per attempt (provider/status/error_class/duration/failover_reason), the served
 * node distinct, single-attempt ⇒ a single node (no fake failover), and an unmeasured per-attempt
 * time renders `—` (the component shows it) — modelled here as `null` + `unavailable`, never `0`.
 */

const T = 1_700_000_000_000;

const FAILED_A: Attempt = {
  provider: 'vllm-a',
  model: 'llama-3.1-70b',
  start_ms: T,
  end_ms: T + 800,
  status: 'failed',
  error_class: 'http_status',
  failover_reason: 'provider_failed',
};
const SERVED_B: Attempt = {
  provider: 'openai',
  model: 'gpt-4o',
  start_ms: T + 800,
  end_ms: T + 2000,
  first_upstream_byte_ms: T + 1100,
  status: 'served',
};

describe('attemptTrace — failover chain', () => {
  it('builds one node per attempt, ordered, with the served node marked distinct', () => {
    const trace = attemptTrace([FAILED_A, SERVED_B]);
    expect(trace.hasTrace).toBe(true);
    expect(trace.isFailover).toBe(true);
    expect(trace.nodes).toHaveLength(2);
    expect(trace.failedCount).toBe(1);
    expect(trace.servedIndex).toBe(1);

    const [a, b] = trace.nodes;
    expect(a!.step).toBe('A');
    expect(a!.status).toBe('failed');
    expect(a!.isServed).toBe(false);
    expect(a!.provider).toBe('vllm-a');
    expect(a!.errorClass).toBe('http_status');
    expect(a!.failoverReason).toBe('provider_failed');

    expect(b!.step).toBe('B');
    expect(b!.status).toBe('served');
    expect(b!.isServed).toBe(true);
    // The served node never carries an error class / failover reason.
    expect(b!.errorClass).toBeNull();
    expect(b!.failoverReason).toBeNull();
  });

  it('measures each node duration from end−start (measured, never negative)', () => {
    const trace = attemptTrace([FAILED_A, SERVED_B]);
    expect(trace.nodes[0]!.durationMs).toBe(800);
    expect(trace.nodes[0]!.durationQuality).toBe('measured');
    expect(trace.nodes[1]!.durationMs).toBe(1200);
    expect(trace.nodes[1]!.durationQuality).toBe('measured');
  });

  it('derives the served node first upstream byte relative to its own start (measured)', () => {
    const trace = attemptTrace([FAILED_A, SERVED_B]);
    // SERVED_B: byte at T+1100, start at T+800 ⇒ 300ms wire TTFB for that attempt.
    expect(trace.nodes[1]!.firstByte.valueMs).toBe(300);
    expect(trace.nodes[1]!.firstByte.quality).toBe('measured');
  });
});

describe('attemptTrace — single attempt (no fake failover)', () => {
  it('a single served attempt ⇒ a SINGLE node, isFailover false', () => {
    const trace = attemptTrace([SERVED_B]);
    expect(trace.nodes).toHaveLength(1);
    expect(trace.isFailover).toBe(false);
    expect(trace.failedCount).toBe(0);
    expect(trace.servedIndex).toBe(0);
    expect(trace.nodes[0]!.isServed).toBe(true);
  });

  it('a single FAILED attempt ⇒ one node, no served index, not a failover', () => {
    const trace = attemptTrace([FAILED_A]);
    expect(trace.nodes).toHaveLength(1);
    expect(trace.isFailover).toBe(false);
    expect(trace.servedIndex).toBeNull();
    expect(trace.failedCount).toBe(1);
  });
});

describe("attemptTrace — don't-lie-with-zeros (the core acceptance)", () => {
  it("a FAILED attempt with NO first_upstream_byte ⇒ firstByte UNAVAILABLE (—), never 0", () => {
    // FAILED_A has no `first_upstream_byte_ms` (failed before any header).
    const node = attemptTrace([FAILED_A]).nodes[0]!;
    expect(node.firstByte.valueMs).toBeNull();
    expect(node.firstByte.quality).toBe('unavailable');
  });

  it('a node with a MEASURED 0ms first byte (start == byte) reads 0, DISTINCT from unavailable + NOT disordered', () => {
    const zeroByte: Attempt = { ...SERVED_B, start_ms: T, first_upstream_byte_ms: T };
    const node = attemptTrace([zeroByte]).nodes[0]!;
    expect(node.firstByte.valueMs).toBe(0);
    expect(node.firstByte.quality).toBe('measured'); // a real 0, not unavailable
    // A GENUINE measured 0 (byte == start) is NOT flagged disordered — it must stay distinct from a
    // clamped/skewed 0 (review MEDIUM on `13c1e5fb`).
    expect(node.firstByte.disordered).toBe(false);
  });

  it('a DISORDERED first byte (byte before start) clamps to 0 + flags skew — NOT a real measured 0', () => {
    // first_upstream_byte_ms BEFORE start_ms is impossible — it must be clamped + FLAGGED, not
    // silently reported as a measured `0ms` (the don't-lie-with-zeros MEDIUM on `13c1e5fb`).
    const skewed: Attempt = { ...SERVED_B, start_ms: T + 500, first_upstream_byte_ms: T + 100 };
    const node = attemptTrace([skewed]).nodes[0]!;
    expect(node.firstByte.valueMs).toBe(0); // clamped, never negative
    expect(node.firstByte.disordered).toBe(true); // FLAGGED — distinct from a genuine measured 0
    expect(node.firstByte.quality).toBe('measured'); // the values are real, just out of order
  });

  it('an attempt missing an endpoint ⇒ duration UNAVAILABLE (—), never a fabricated 0', () => {
    // end_ms = 0 (the Rust never emits 0 for an occurred instant ⇒ treated as unmeasured).
    const noEnd: Attempt = { ...FAILED_A, end_ms: 0 };
    const node = attemptTrace([noEnd]).nodes[0]!;
    expect(node.durationMs).toBeNull();
    expect(node.durationQuality).toBe('unavailable');
  });

  it('a 0-sentinel first_upstream_byte_ms is treated as UNMEASURED (—), not a real 0', () => {
    const sentinel: Attempt = { ...SERVED_B, first_upstream_byte_ms: 0 };
    const node = attemptTrace([sentinel]).nodes[0]!;
    expect(node.firstByte.quality).toBe('unavailable');
    expect(node.firstByte.valueMs).toBeNull();
  });

  it('clock-disordered endpoints clamp the duration to 0 + flag disordered (never negative)', () => {
    const disordered: Attempt = { ...FAILED_A, start_ms: T + 500, end_ms: T + 100 };
    const node = attemptTrace([disordered]).nodes[0]!;
    expect(node.durationMs).toBe(0);
    expect(node.disordered).toBe(true);
    expect(node.durationQuality).toBe('measured');
  });

  it('a blank/whitespace provider or model normalizes to null (renders — , not empty)', () => {
    const blank: Attempt = { ...FAILED_A, provider: '   ', model: '' };
    const node = attemptTrace([blank]).nodes[0]!;
    expect(node.provider).toBeNull();
    expect(node.model).toBeNull();
  });
});

describe('attemptTrace — empty / absent', () => {
  it('an empty list ⇒ no trace (renders nothing), no fabricated node', () => {
    const trace = attemptTrace([]);
    expect(trace.hasTrace).toBe(false);
    expect(trace.nodes).toHaveLength(0);
    expect(trace.isFailover).toBe(false);
    expect(trace.servedIndex).toBeNull();
  });

  it('null/undefined ⇒ no trace', () => {
    expect(attemptTrace(null).hasTrace).toBe(false);
    expect(attemptTrace(undefined).hasTrace).toBe(false);
  });
});

describe('attemptTrace — all-failed chain (no served node)', () => {
  it('every attempt failed ⇒ servedIndex null, isFailover true, all failed counted', () => {
    const second: Attempt = { ...FAILED_A, provider: 'vllm-b', start_ms: T + 800, end_ms: T + 1500, error_class: 'timeout', failover_reason: 'terminal_no_failover' };
    const trace = attemptTrace([FAILED_A, second]);
    expect(trace.isFailover).toBe(true);
    expect(trace.servedIndex).toBeNull();
    expect(trace.failedCount).toBe(2);
    expect(trace.nodes[1]!.errorClass).toBe('timeout');
    expect(trace.nodes[1]!.failoverReason).toBe('terminal_no_failover');
  });
});

describe('attemptTrace — step labels past the alphabet stay bounded', () => {
  it('labels A..Z then #27', () => {
    const many: Attempt[] = Array.from({ length: 27 }, (_, i) => ({
      ...FAILED_A,
      start_ms: T + i * 10,
      end_ms: T + i * 10 + 5,
    }));
    const nodes = attemptTrace(many).nodes;
    expect(nodes[0]!.step).toBe('A');
    expect(nodes[25]!.step).toBe('Z');
    expect(nodes[26]!.step).toBe('#27');
  });
});
