/**
 * failureTaxonomy (gap 14) — the PURE, DOM-free model behind the AGGREGATE failure deep-dive +
 * the inspector's captured-error-body view. Sibling of `tokenEconomics.ts`/`contextUtilization.ts`/
 * `latencyBreakdown.ts`/`attemptTrace.ts`/`providerLatency.ts`: unit-testable in isolation so the
 * panel + the error chip + the ErrorTab can never disagree with the numbers/states they derive.
 *
 * Operator question (spec 14): "What is failing and why, in AGGREGATE — not one red row at a time?"
 * Answered by GROUPING the observed flows by (model/provider) × failure reason, with a DERIVED error
 * RATE per group, plus an overall error rate for the chip.
 *
 * SOURCES — all ALREADY on the wire (gate F; verified by code search):
 *  - `FlowSummary.status` (`failed` is the failure signal) + `model_served`/`model_requested` +
 *    `upstream_target` — the grouping dimensions (all `measured`, straight off the row).
 *  - `FlowSummary.terminal_reason` — the flow-level reason string (`measured`); the PRIMARY reason
 *    label. NOT a raw upstream body — it is the gateway's terminal classification.
 *  - `FlowSummary.attempts[]` `error_class` (gap 03, bounded taxonomy) — a structured FALLBACK
 *    reason when `terminal_reason` is absent (e.g. the failed leaf attempt's class), and never raw
 *    upstream text.
 *  - `FlowDetail.upstream_response` (gap 05, LIVE detail only) — the captured upstream error BODY for
 *    the ErrorTab. Its PRESENCE is the capture-on signal; ABSENCE ⇒ "capture disabled" (UNAVAILABLE),
 *    NOT "no error".
 *
 * DATA-QUALITY INVARIANTS (the heart of an honest failure surface — asserted in the sibling tests):
 *  - The GROUPING (which model/provider, which reason, the failure COUNTS) is `measured` — directly
 *    counted off the rows. The error RATE (`failed/total × 100`) is `derived`.
 *  - DON'T-LIE-WITH-ZEROS: a window with ZERO observed flows ⇒ the whole surface is UNAVAILABLE
 *    (`available: false`, every rate `—`), NEVER `0%`. A group that WAS observed with 0 failures
 *    reports a genuine MEASURED-base `derived 0%` (distinct from the unavailable `—`).
 *  - The captured error BODY: PRESENT ⇒ shown (`measured`, truncation flagged honestly); ABSENT ⇒
 *    `captureState: 'unavailable'` ("capture disabled / no captured body") — never a blank implying
 *    "no error". An EMPTY captured body (`""`) is PRESENT (distinct from absent).
 */
import type { Attempt, AttemptErrorClass, FlowSummary, FlowUpstreamResponse } from '../../api/types';
import { ATTEMPT_ERROR_CLASSES, isFlowUpstreamResponse } from '../../api/types';

/** Provenance of a figure — mirrors the dashboard's measured/derived/estimated/unavailable tags. */
export type Quality = 'measured' | 'derived' | 'estimated' | 'unavailable';

/** The unavailable / no-data marker (a value that cannot be measured renders this, never `0`). */
export const UNAVAILABLE = '—';

/** Human label for a bounded error class (the gap-03 taxonomy), e.g. `http_status` → "http status". */
const ERROR_CLASS_LABEL: Record<AttemptErrorClass, string> = {
  connect: 'connect',
  http_status: 'http status',
  timeout: 'timeout',
  stream: 'stream',
  terminal: 'terminal',
  other: 'other',
};

// ---------------------------------------------------------------------------
// Aggregate failure groups (the panel).
// ---------------------------------------------------------------------------

/** How a failure reason was sourced — all BOUNDED (never a free-form string becomes a key). */
export type FailureReasonSource = 'error_class' | 'terminal_reason' | 'unclassified';

/** One failure REASON row within a model/provider group (a distinct BOUNDED reason). */
export interface FailureReasonRow {
  /** A stable, BOUNDED key: `class:<error_class>` (gap-03), `terminal:<bounded_code>`, or `__unclassified__`. */
  key: string;
  /** Display label for the reason, e.g. "http status" or "content filter". */
  label: string;
  /** How the reason was sourced — the gap-03 attempt class, a WHITELISTED terminal code, or unclassified. */
  source: FailureReasonSource;
  /** The number of FAILED flows in this group attributed to this reason (always `>= 1`; measured). */
  count: number;
}

/** One model/provider group — its total observed flows, its failures, the derived rate, the reasons. */
export interface FailureGroup {
  /** Stable group key (`<provider>|<model>`). */
  key: string;
  /** The served upstream/provider for this group, or `—` when the row carried none. */
  provider: string;
  /** The model for this group (served, else requested), or `—` when neither was set. */
  model: string;
  /** Total flows OBSERVED in this group (the measurability denominator; always `>= 1`). */
  total: number;
  /** Of `total`, the count that FAILED (`status === 'failed'`); a measured count. */
  failed: number;
  /** Error rate `failed/total × 100` (`derived`). A group with 0 failures is a real `0%`. */
  errorRatePct: number;
  /** Pre-formatted error-rate string (`33%` / `0%`) — always available for an observed group. */
  errorRateText: string;
  /** The per-reason breakdown of this group's failures (ordered by count desc, then label). Empty when
   *  the group had no failures (a real, measured `0`-failure group — NOT a fabricated reason). */
  reasons: FailureReasonRow[];
}

/** The aggregate failure-taxonomy model the panel renders. */
export interface FailureTaxonomy {
  /** Whether ANY flow was observed (the measurability gate). `false` ⇒ every figure is `—`. */
  available: boolean;
  /** Total flows observed across all groups (the overall denominator). */
  totalFlows: number;
  /** Total FAILED flows across all groups (measured). */
  totalFailed: number;
  /** Overall error rate `totalFailed/totalFlows × 100` (`derived`) — drives the error-rate chip. */
  overallErrorRatePct: number;
  /** Pre-formatted overall rate (`—` when no flow observed, else `0%`+/derived). */
  overallErrorRateText: string;
  /** Provenance of the overall rate — `derived` when observed, `unavailable` when not. */
  overallQuality: Quality;
  /** The model/provider groups that had AT LEAST ONE failure, ordered by failed-count desc then rate. */
  groups: FailureGroup[];
}

/** A non-empty trimmed string, else null (so a blank field is treated as absent, not rendered ""). */
function text(v: string | null | undefined): string | null {
  if (typeof v !== 'string') return null;
  const t = v.trim();
  return t.length > 0 ? t : null;
}

/**
 * Format a `derived` error rate (a percentage in [0,100]) for a group/chip, or `—` when unavailable.
 * A MEASURED-base `0` reads `0%` (an observed group with no failures) — distinct from the unavailable
 * `—`. One decimal under 10% (so 4.2% is visible), whole percent otherwise.
 */
export function fmtFailureRate(pct: number | null): string {
  if (pct === null || !Number.isFinite(pct)) return UNAVAILABLE;
  if (pct === 0) return '0%';
  if (pct < 10) return `${pct.toFixed(1)}%`;
  return `${Math.round(pct)}%`;
}

/** The model/provider grouping dimensions for one flow (served identity wins; blank ⇒ a `—` sentinel). */
function dimsOf(flow: FlowSummary): { provider: string; model: string } {
  const provider = text(flow.upstream_target) ?? UNAVAILABLE;
  const model = text(flow.model_served) ?? text(flow.model_requested) ?? UNAVAILABLE;
  return { provider, model };
}

/**
 * The BOUNDED `terminal_reason` vocabulary — the EXACT serialized `TerminalReason` enum the engine
 * emits (`src/models/responses.rs`: `stop`/`length`/`tool_calls`/`content_filter`/`other`). This is a
 * CLOSED whitelist: the dashboard `finalize` seam can stamp an ARBITRARY/free-form `terminal_reason`
 * string (e.g. a raw "upstream 503", or an error-body-derived message) which is `cap_scalar`-capped
 * but NOT vocabulary-bounded — letting such a string become a GROUP KEY would blow up cardinality and
 * defeat gap-03's whole point (a bounded snake_case taxonomy). So `terminal_reason` is used as a
 * reason ONLY when it is one of these recognized bounded codes; anything else ⇒ `__unclassified__`.
 */
export const BOUNDED_TERMINAL_REASONS = ['stop', 'length', 'tool_calls', 'content_filter', 'other'] as const;
export type BoundedTerminalReason = (typeof BOUNDED_TERMINAL_REASONS)[number];

/** Human label for a bounded terminal reason (e.g. `tool_calls` → "tool calls", `content_filter` → "content filter"). */
const TERMINAL_REASON_LABEL: Record<BoundedTerminalReason, string> = {
  stop: 'stop',
  length: 'length',
  tool_calls: 'tool calls',
  content_filter: 'content filter',
  other: 'other terminal',
};

/**
 * Resolve the failure REASON for ONE failed flow — ALWAYS a BOUNDED taxonomic key (never a free-form
 * string), so a group key can never blow up cardinality (review HIGH). Order:
 *  1. the LAST failed attempt's bounded gap-03 `error_class` (`class:<x>`) — the precise, structured
 *     failure taxonomy (connect/http_status/timeout/stream/terminal/other); this is the PRIMARY
 *     signal for a failed flow and the one gap-03 exists to provide.
 *  2. else `terminal_reason` ONLY IF it is a recognized BOUNDED `TerminalReason` code (the closed
 *     {@link BOUNDED_TERMINAL_REASONS} whitelist — e.g. `content_filter`). Any UNRECOGNIZED/free-form
 *     value (raw upstream text, an error-body-derived message) is REJECTED here.
 *  3. else `__unclassified__` (an explicit bounded bucket) — NEVER the arbitrary string itself.
 * Every branch yields a bounded key; no caller ever sees a free-form `terminal_reason` as a key.
 */
function reasonOf(flow: FlowSummary): { key: string; label: string; source: FailureReasonRow['source'] } {
  // 1. Bounded gap-03 error class FIRST (the structured failure taxonomy).
  const cls = failedAttemptClass(flow.attempts);
  if (cls) return { key: `class:${cls}`, label: ERROR_CLASS_LABEL[cls], source: 'error_class' };
  // 2. A WHITELISTED bounded terminal reason ONLY (a free-form string is rejected → falls through).
  const terminal = text(flow.terminal_reason);
  if (terminal && isBoundedTerminalReason(terminal)) {
    return { key: `terminal:${terminal}`, label: TERMINAL_REASON_LABEL[terminal], source: 'terminal_reason' };
  }
  // 3. Anything else ⇒ the explicit unclassified bucket (NEVER the arbitrary string as a key).
  return { key: UNCLASSIFIED_REASON_KEY, label: UNCLASSIFIED_REASON_LABEL, source: 'unclassified' };
}

/** The error class of the LAST failed attempt (the leaf that terminated the turn), or null. */
function failedAttemptClass(attempts: Attempt[] | null | undefined): AttemptErrorClass | null {
  if (!attempts || attempts.length === 0) return null;
  for (let i = attempts.length - 1; i >= 0; i -= 1) {
    const a = attempts[i]!;
    if (a.status === 'failed' && a.error_class && isErrorClass(a.error_class)) return a.error_class;
  }
  return null;
}

/** Guard a wire `error_class` against the bounded gap-03 taxonomy (don't trust the wire). */
function isErrorClass(v: unknown): v is AttemptErrorClass {
  return typeof v === 'string' && (ATTEMPT_ERROR_CLASSES as readonly string[]).includes(v);
}

/** Guard a `terminal_reason` against the CLOSED bounded `TerminalReason` vocabulary (rejects free-form). */
function isBoundedTerminalReason(v: string): v is BoundedTerminalReason {
  return (BOUNDED_TERMINAL_REASONS as readonly string[]).includes(v);
}

/** The label for an UNCLASSIFIED failure (failed flow with no terminal reason / attempt class). */
export const UNCLASSIFIED_REASON_KEY = '__unclassified__';
const UNCLASSIFIED_REASON_LABEL = 'unclassified';

interface GroupAccum {
  provider: string;
  model: string;
  total: number;
  failed: number;
  reasons: Map<string, FailureReasonRow>;
}

/**
 * Build the aggregate failure taxonomy from the observed flow rows. EVERY row counts toward its
 * group's `total` (the error-rate denominator); a FAILED row additionally bumps `failed` and adds its
 * reason. Groups with no failures still inform the OVERALL rate (their `total` is in the denominator)
 * but are NOT listed (the panel surfaces what is FAILING). Empty input ⇒ `available: false` (every
 * figure `—`; don't-lie-with-zeros — never a fabricated `0%`).
 */
export function failureTaxonomy(flows: readonly FlowSummary[] | null | undefined): FailureTaxonomy {
  const rows = flows ?? [];
  if (rows.length === 0) {
    return {
      available: false,
      totalFlows: 0,
      totalFailed: 0,
      overallErrorRatePct: 0,
      overallErrorRateText: UNAVAILABLE,
      overallQuality: 'unavailable',
      groups: [],
    };
  }

  const groups = new Map<string, GroupAccum>();
  let totalFlows = 0;
  let totalFailed = 0;

  for (const flow of rows) {
    const { provider, model } = dimsOf(flow);
    const key = `${provider}|${model}`;
    let g = groups.get(key);
    if (!g) {
      g = { provider, model, total: 0, failed: 0, reasons: new Map() };
      groups.set(key, g);
    }
    g.total += 1;
    totalFlows += 1;
    if (flow.status === 'failed') {
      g.failed += 1;
      totalFailed += 1;
      // ALWAYS a BOUNDED reason key (gap-03 class / whitelisted terminal code / `__unclassified__`) —
      // a free-form `terminal_reason` can NEVER become a group key (review HIGH).
      const reason = reasonOf(flow);
      const existing = g.reasons.get(reason.key);
      if (existing) existing.count += 1;
      else g.reasons.set(reason.key, { key: reason.key, label: reason.label, source: reason.source, count: 1 });
    }
  }

  const built: FailureGroup[] = [];
  for (const g of groups.values()) {
    // Only surface groups that ACTUALLY failed (the panel answers "what is failing"); a no-failure
    // group's `total` still counted toward the overall denominator above.
    if (g.failed === 0) continue;
    const errorRatePct = (g.failed / g.total) * 100;
    const reasons = [...g.reasons.values()].sort(byCountThenLabel);
    built.push({
      key: `${g.provider}|${g.model}`,
      provider: g.provider,
      model: g.model,
      total: g.total,
      failed: g.failed,
      errorRatePct,
      errorRateText: fmtFailureRate(errorRatePct),
      reasons,
    });
  }
  built.sort((a, b) => b.failed - a.failed || b.errorRatePct - a.errorRatePct || a.key.localeCompare(b.key));

  const overallErrorRatePct = totalFlows > 0 ? (totalFailed / totalFlows) * 100 : 0;
  return {
    available: true,
    totalFlows,
    totalFailed,
    overallErrorRatePct,
    overallErrorRateText: fmtFailureRate(overallErrorRatePct),
    overallQuality: 'derived',
    groups: built,
  };
}

/** Sort reasons by descending count, then label (stable, readable order). */
function byCountThenLabel(a: FailureReasonRow, b: FailureReasonRow): number {
  return b.count - a.count || a.label.localeCompare(b.label);
}

// ---------------------------------------------------------------------------
// Captured upstream error BODY (the inspector ErrorTab) — gap 05 `upstream_response`.
// ---------------------------------------------------------------------------

/**
 * The state of the captured upstream error body for ONE flow's ErrorTab:
 *  - `captured`    — the body is PRESENT (capture armed AND a body was recorded). `measured`; the UI
 *                    renders it (truncation flagged via `truncated`). An EMPTY body (`""`) is still
 *                    `captured` (distinct from `unavailable`).
 *  - `unavailable` — the body is ABSENT (capture DISABLED, or no body recorded, or evicted). The UI
 *                    shows an explicit "capture disabled" state — NEVER a blank that implies "no error"
 *                    (spec 14 don't-lie-with-zeros / capture-disabled-vs-no-body distinction).
 */
export type CaptureState = 'captured' | 'unavailable';

/** The render model for the ErrorTab's captured-error-body section. */
export interface CapturedErrorBody {
  state: CaptureState;
  /** Provenance — `measured` when captured, `unavailable` when absent. */
  quality: Quality;
  /** The parsed captured body (JSON value or a string) when `captured`; `undefined` when unavailable. */
  body: unknown;
  /** Whether the captured body was TRUNCATED by the cap (flag it honestly). `false` when unavailable. */
  truncated: boolean;
  /** A human note for the title/explanation line (capture-disabled wording vs the captured-body note). */
  detail: string;
}

/**
 * Resolve the captured-error-body model from a flow's (live-detail) `upstream_response`. ABSENT/malformed
 * ⇒ `unavailable` ("capture disabled / no captured body" — never "no error"); a valid present body ⇒
 * `captured` (the operator-facing body is shown; truncation flagged). The detail wording makes the
 * capture-disabled-vs-no-body distinction explicit for the operator.
 */
export function capturedErrorBody(upstream: FlowUpstreamResponse | null | undefined): CapturedErrorBody {
  if (!isFlowUpstreamResponse(upstream)) {
    return {
      state: 'unavailable',
      quality: 'unavailable',
      body: undefined,
      truncated: false,
      detail:
        'upstream error-body capture is OFF (or no body was captured / it was evicted). ' +
        'Enable LLMCONDUIT_DASHBOARD_CAPTURE_UPSTREAM_RESPONSE=1 to capture upstream error bodies. ' +
        'This is "capture disabled / unavailable" — NOT "no error".',
    };
  }
  return {
    state: 'captured',
    quality: 'measured',
    body: upstream.body,
    truncated: upstream.truncated === true,
    detail: upstream.truncated
      ? 'captured upstream error body (TRUNCATED by the capture cap — a prefix of the full body)'
      : 'captured upstream error body (redacted, as received from the upstream)',
  };
}
