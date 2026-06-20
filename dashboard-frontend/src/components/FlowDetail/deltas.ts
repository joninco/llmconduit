/**
 * Bridges the two delta sources the inspector shows in the deltas sub-panel:
 *
 *  - LIVE monitor segments (`DebugSegment[]`, from `segment_append` frames joined by
 *    `response_id`) — present while the flow streams.
 *  - REPLAYED REST deltas (`FlowDelta[]` on `GET /flows/:id`) — the MonitorHub snapshot the
 *    backend persists so a reloaded or already-completed flow still shows its streamed output.
 *
 * D10 originally rendered ONLY the live segments, so a flow loaded fresh via REST (no live frames
 * in the ring) showed an empty deltas panel even though its replay was sitting in `detail.deltas`
 * (finding 5). This normalizes the REST deltas into `DebugSegment`s and merges them with the live
 * ones: the REST replay is the BASE (chronological, sequence-ordered) and live segments are
 * APPENDED (newer arrivals continue the stream). De-dup keeps it idempotent if the same content
 * is present in both sources.
 */
import type { DebugSegment, DebugSegmentKind, FlowDelta } from '../../api/types';

/**
 * Classifies a REST delta's freeform `kind` string into a `DebugSegment` kind. The engine emits
 * dotted event names (`response.output_text.delta`, `response.reasoning_summary.delta`,
 * `response.function_call_arguments.delta`); we key off substrings so variants map without an
 * exhaustive table. Anything textual that is not reasoning/tool is `output` (the default stream).
 */
function classifyKind(kind: string): DebugSegmentKind {
  const k = kind.toLowerCase();
  if (k.includes('reasoning')) return 'reasoning';
  if (k.includes('function_call') || k.includes('tool')) return 'tool';
  return 'output';
}

/**
 * Extracts the human-visible text from a delta payload. Covers the shapes the engine uses:
 * `{ text }` (output/reasoning deltas), `{ delta }` (raw delta string), `{ arguments }` (tool-call
 * argument fragments). A string payload is taken verbatim. Returns `''` when there is no textual
 * content (a lifecycle-only delta like `response.created`), which the caller drops.
 */
function extractText(payload: unknown): string {
  if (typeof payload === 'string') return payload;
  if (payload && typeof payload === 'object') {
    const p = payload as Record<string, unknown>;
    for (const field of ['text', 'delta', 'arguments', 'output', 'content'] as const) {
      if (typeof p[field] === 'string') return p[field] as string;
    }
  }
  return '';
}

/**
 * Normalizes the REST replay (`FlowDelta[]`) into ordered `DebugSegment`s. Sorts by `sequence`
 * (the authoritative replay order), maps each kind/payload, and DROPS deltas with no textual
 * content (pure lifecycle events) so they don't render as empty blocks. `ts_ms` seeds the
 * segment timestamp (0 when absent — replayed deltas may omit it).
 */
export function normalizeRestDeltas(deltas: FlowDelta[] | undefined): DebugSegment[] {
  if (!deltas || deltas.length === 0) return [];
  return [...deltas]
    .sort((a, b) => a.sequence - b.sequence)
    .map((d): DebugSegment | null => {
      const text = extractText(d.payload);
      if (text === '') return null;
      return { timestamp_ms: d.ts_ms ?? 0, kind: classifyKind(d.kind), text };
    })
    .filter((s): s is DebugSegment => s !== null);
}

/**
 * Merges the REST replay (base) with the live monitor segments (appended). The replay anchors the
 * stream for a reloaded/completed flow; live segments continue it. The two sources OVERLAP at the
 * seam: the live ring retains the recent history the replay already holds, so the END of `rest`
 * and the START of `live` repeat the same segments. We remove that overlap by finding the LONGEST
 * suffix of `rest` that equals a prefix of `live` (by kind+text — timestamps may differ between
 * sources) and appending only the live tail past it. So REST `[A,B]` + live `[B,C]` joins to
 * `[A,B,C]` (the shared `B` is not duplicated), not `[A,B,B,C]` (finding 6). A prefix-only compare
 * missed a PARTIAL overlap like this — it only caught a live head that duplicated the replay HEAD.
 * Order is preserved: replay first, then the un-duplicated live remainder.
 */
export function mergeDeltas(rest: DebugSegment[], live: DebugSegment[]): DebugSegment[] {
  if (rest.length === 0) return live;
  if (live.length === 0) return rest;
  const sameSeg = (a: DebugSegment, b: DebugSegment) => a.kind === b.kind && a.text === b.text;
  // Longest k such that rest's last k segments === live's first k segments. Start from the largest
  // feasible overlap and shrink, so the seam joins cleanly even when live re-sends many segments.
  const maxOverlap = Math.min(rest.length, live.length);
  let overlap = 0;
  for (let k = maxOverlap; k > 0; k -= 1) {
    let matches = true;
    for (let i = 0; i < k; i += 1) {
      if (!sameSeg(rest[rest.length - k + i]!, live[i]!)) {
        matches = false;
        break;
      }
    }
    if (matches) {
      overlap = k;
      break;
    }
  }
  return [...rest, ...live.slice(overlap)];
}
