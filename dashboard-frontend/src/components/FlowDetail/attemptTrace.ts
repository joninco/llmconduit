/**
 * attemptTrace (gap 11) — turn the gap-03 `attempts[]` failover trace into a render-ready
 * STEPPER for the FlowDetail inspector header. Pure + DOM-free (sibling of `latencyBreakdown.ts`/
 * `contextUtilization.ts`/`tokenEconomics.ts`) so the stepper can never disagree with the numbers
 * it derives, and so the don't-lie-with-zeros rules are unit-testable in isolation.
 *
 * The operator question (spec 11): "Which provider failed, why, how long did we wait, and what
 * served?" — answered as the ORDERED chain of dispatch attempts:
 *
 *   A failed: 503 · 0.8s  →  B served · 1.2s
 *
 * SOURCE (spec 11 — the oracle): the gap-03 `attempts[]` (each `Attempt` is body-free scalar
 * provenance + bounded taxonomic codes). The failover loop records one node per provider it tried
 * (the failed ones + the one that served); a non-failover flow records exactly ONE node. We render
 * exactly what the backend measured — never a synthesized/fake failover.
 *
 * DATA-QUALITY INVARIANTS (the heart of an honest trace — each is asserted in the sibling tests):
 *  - The whole trace + each node's WHICH/WHY (provider, model, status, `error_class`,
 *    `failover_reason`) is `measured` — it comes straight from `attempts[]`.
 *  - A node's DURATION is `measured` from `end_ms − start_ms` when both are present + ordered; a
 *    disordered pair (end < start) clamps to 0 + is FLAGGED (`disordered`), NEVER rendered negative.
 *  - A per-attempt `first_upstream_byte_ms` that is absent/null renders `—` (UNAVAILABLE), NEVER a
 *    fabricated `0ms` (spec 11 acceptance: "a per-attempt unmeasured time renders `—`, not `0`").
 *    A genuine measured `0` first-byte (start == byte) is DISTINCT — it reads `0ms`.
 *  - A SINGLE-attempt flow yields a SINGLE node and `isFailover === false` (no fake failover chain);
 *    `≥ 2` attempts yields the chain (`isFailover === true`).
 *  - The SERVED node (status `served`) is marked `isServed` so the UI can render it visually
 *    distinct; a trace with no served attempt (every provider failed) has none.
 *  - An EMPTY/absent `attempts[]` ⇒ an empty trace (`nodes: []`, `hasTrace === false`): the stepper
 *    renders nothing rather than inventing a node.
 */
import type { Attempt, AttemptErrorClass, AttemptFailoverReason, AttemptStatus } from '../../api/types';

/** Provenance of a derived figure — mirrors the dashboard's measured/derived/unavailable tags. */
export type Quality = 'measured' | 'derived' | 'estimated' | 'unavailable';

/** A render-ready first-byte figure for a node — its wire TTFB relative to the attempt start. */
export interface ByteFigure {
  /** ms from the attempt's `start_ms` to its first upstream byte. `null` ⇒ UNAVAILABLE (`—`). */
  valueMs: number | null;
  quality: Quality;
  /**
   * True when the first byte was observed BEFORE the attempt start (end < start) — an impossible
   * ordering CLAMPED to 0 + FLAGGED (mirrors the segment/duration disorder handling), so a
   * disordered `0` is never mistaken for a genuine measured `0ms` first byte (don't-lie-with-zeros).
   * A real measured `0` (byte ≥ start) keeps this `false`.
   */
  disordered: boolean;
  /** A short provenance note for the title attribute. */
  detail: string;
}

/** One node of the failover stepper — a single upstream dispatch attempt, fully decoded. */
export interface AttemptNode {
  /** 0-based position in the chain (drives keys + the "A/B/C" step label). */
  index: number;
  /** A short positional label (`A`, `B`, `C`, … then `#27`) for the stepper node. */
  step: string;
  /** The provider that handled this attempt; `null`/absent ⇒ `—` (never a fabricated name). */
  provider: string | null;
  /** The model dispatched on this attempt; `null`/absent ⇒ `—`. */
  model: string | null;
  /** The attempt outcome (`served` | `failed`) — straight from `attempts[]` (measured). */
  status: AttemptStatus;
  /** True for the served node — the UI renders it visually distinct (spec 11). */
  isServed: boolean;
  /** Bounded taxonomic failure code; `null` on the served node (never raw upstream text). */
  errorClass: AttemptErrorClass | null;
  /** Bounded taxonomic failover reason; `null` on the served node. */
  failoverReason: AttemptFailoverReason | null;
  /**
   * Attempt wall-clock (`end_ms − start_ms`). `null` ⇒ UNAVAILABLE (an endpoint was unmeasured) —
   * render `—`, never a fabricated `0`. A finite `0` is a real ~0ms attempt. Never negative.
   */
  durationMs: number | null;
  /** Provenance of `durationMs` — `measured` (both endpoints) or `unavailable`. */
  durationQuality: Quality;
  /**
   * The attempt's first upstream byte relative to its `start_ms` (wire TTFB for this attempt).
   * `null` ⇒ UNAVAILABLE (`—`, e.g. a failure before any header) — NEVER `0` (spec 11 core).
   */
  firstByte: ByteFigure;
  /**
   * True when the attempt's endpoints were observed out of order (end < start) and the duration was
   * CLAMPED to 0 rather than rendered negative. The UI flags it (a clock-skew note).
   */
  disordered: boolean;
}

/** The full failover trace the inspector renders. */
export interface AttemptTrace {
  /** The ordered stepper nodes (one per recorded attempt). Empty ⇒ render nothing. */
  nodes: AttemptNode[];
  /** True when there is at least one attempt to show (drives whether the stepper renders at all). */
  hasTrace: boolean;
  /** True when ≥ 2 attempts were recorded (a real failover chain) — single-attempt is NOT failover. */
  isFailover: boolean;
  /** Count of FAILED attempts before something served (or all of them, if none served). */
  failedCount: number;
  /** The served node's index, or `null` when every attempt failed (no served node). */
  servedIndex: number | null;
}

/** A finite, non-negative epoch-ms, else null (an absent/`0`-sentinel/garbage timestamp). */
function epoch(v: number | null | undefined): number | null {
  // A real wall-clock epoch is a large positive integer; treat a non-finite or non-positive value
  // as "unmeasured" (the Rust side never emits `0` for an occurred instant — `skip_serializing_if`).
  return typeof v === 'number' && Number.isFinite(v) && v > 0 ? v : null;
}

/** A non-empty trimmed string, else null (so a blank provider/model renders `—`, not empty). */
function text(v: string | null | undefined): string | null {
  if (typeof v !== 'string') return null;
  const t = v.trim();
  return t.length > 0 ? t : null;
}

/** Positional step label: A, B, … Z, then `#<n>` past the alphabet (keeps it bounded + readable). */
function stepLabel(index: number): string {
  return index < 26 ? String.fromCharCode(65 + index) : `#${index + 1}`;
}

/**
 * The first-byte figure for one attempt: ms from `start_ms` to `first_upstream_byte_ms`.
 *  - both present + ordered ⇒ `measured` (a finite `0` is a real measured 0ms first byte, `disordered: false`).
 *  - byte present but BEFORE start (impossible ordering) ⇒ clamp to 0 + `disordered: true` (FLAGGED,
 *    so it is NEVER mistaken for a genuine measured `0ms` — the same honest handling as the duration/
 *    segment disorder; don't-lie-with-zeros). Stays `measured` (the values are real, just out of order).
 *  - byte absent/null (no header ever arrived) ⇒ `unavailable` (`—`, never a fabricated `0`).
 */
function firstByteFigure(startMs: number | null, byteMs: number | null): ByteFigure {
  if (byteMs === null) {
    return {
      valueMs: null,
      quality: 'unavailable',
      disordered: false,
      detail: 'no first upstream byte measured for this attempt (failed before any header, or unmeasured)',
    };
  }
  if (startMs === null) {
    // The byte is known but the attempt start is not — we cannot honestly form the relative delta.
    return {
      valueMs: null,
      quality: 'unavailable',
      disordered: false,
      detail: 'first upstream byte known but the attempt start is unmeasured — cannot derive a relative time',
    };
  }
  if (byteMs < startMs) {
    // Impossible ordering (first byte before the attempt began): clamp to 0 + FLAG it, exactly like
    // a disordered duration/segment — a disordered `0` must stay distinct from a real measured `0ms`.
    return {
      valueMs: 0,
      quality: 'measured',
      disordered: true,
      detail: 'wire TTFB endpoints out of order (first byte before start) — clamped to 0 (clock skew), not a real 0ms',
    };
  }
  return {
    valueMs: byteMs - startMs,
    quality: 'measured',
    disordered: false,
    detail: 'wire time-to-first-byte for this attempt: start → first upstream byte (measured)',
  };
}

/** Decode one `Attempt` wire object into a render-ready node (no DOM, no formatting). */
function toNode(attempt: Attempt, index: number): AttemptNode {
  const startMs = epoch(attempt.start_ms);
  const endMs = epoch(attempt.end_ms);
  const byteMs = epoch(attempt.first_upstream_byte_ms);
  const isServed = attempt.status === 'served';

  // Duration: measured from a known ordered pair; disordered ⇒ clamp to 0 + flag (never negative);
  // either endpoint missing ⇒ unavailable (`—`, never a fabricated 0).
  let durationMs: number | null;
  let durationQuality: Quality;
  let disordered = false;
  if (startMs === null || endMs === null) {
    durationMs = null;
    durationQuality = 'unavailable';
  } else if (endMs < startMs) {
    durationMs = 0;
    durationQuality = 'measured';
    disordered = true;
  } else {
    durationMs = endMs - startMs;
    durationQuality = 'measured';
  }

  return {
    index,
    step: stepLabel(index),
    provider: text(attempt.provider),
    model: text(attempt.model),
    status: attempt.status,
    isServed,
    // `error_class`/`failover_reason` are meaningful only on a FAILED attempt; the served node
    // carries `null` (the backend already omits them there, but normalize defensively).
    errorClass: isServed ? null : attempt.error_class ?? null,
    failoverReason: isServed ? null : attempt.failover_reason ?? null,
    durationMs,
    durationQuality,
    firstByte: firstByteFigure(startMs, byteMs),
    disordered,
  };
}

/**
 * Build the failover trace from a flow's `attempts[]`. `attempts` is the already-merged list
 * (the caller resolves live vs frozen via `pickAttempts`, so an empty `[]` is never authoritative
 * over a populated trace). Renders EXACTLY the recorded attempts — never a synthesized failover.
 */
export function attemptTrace(attempts: Attempt[] | null | undefined): AttemptTrace {
  if (!attempts || attempts.length === 0) {
    // Honestly no trace: the stepper renders nothing (don't invent a node) — spec 11.
    return { nodes: [], hasTrace: false, isFailover: false, failedCount: 0, servedIndex: null };
  }

  const nodes = attempts.map(toNode);
  const servedIndex = nodes.findIndex((n) => n.isServed);
  const failedCount = nodes.filter((n) => n.status === 'failed').length;

  return {
    nodes,
    hasTrace: true,
    // A single attempt is NOT a failover (spec 11: "a single-attempt flow → a single node, no fake
    // failover"); only ≥ 2 recorded attempts form a chain.
    isFailover: nodes.length >= 2,
    failedCount,
    servedIndex: servedIndex >= 0 ? servedIndex : null,
  };
}
