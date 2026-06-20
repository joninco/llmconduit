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
 * seam: the live ring retains the recent history the replay already holds.
 *
 * We de-dup by each segment's TEMPORAL CURSOR (`timestamp_ms`), NOT by text (finding 3). Both
 * sources stamp segments from the SAME engine clock (`monitor.rs`): the REST `ts_ms` and the live
 * `timestamp_ms` are the segment's position in the one stream. So the replay COVERS the stream up
 * to its last segment's timestamp, and a live segment belongs to the un-replayed tail iff its
 * timestamp is strictly AFTER that coverage. This fixes both text-equality failures:
 *   - a partial seam overlap is removed precisely (the live head at/under the cursor is dropped);
 *   - legitimately-REPEATED identical segments (same kind+text, DISTINCT timestamps) are PRESERVED,
 *     because identity is the cursor, not the text.
 * When a side carries no usable cursor (all `timestamp_ms === 0` — e.g. a replay that omitted
 * `ts_ms`), we cannot place the seam temporally, so we fall back to APPENDING the whole live run
 * (no text-collapsing): over-keeping a duplicate is safer than silently dropping a real segment.
 * Order is preserved: replay first, then the live tail past the cursor.
 */
export function mergeDeltas(rest: DebugSegment[], live: DebugSegment[]): DebugSegment[] {
  if (rest.length === 0) return live;
  if (live.length === 0) return rest;
  // The replay's coverage cursor = the max timestamp it carries. 0 ⇒ no usable cursor (see below).
  const restCursor = rest.reduce((max, s) => Math.max(max, s.timestamp_ms), 0);
  if (restCursor === 0) {
    // No temporal cursor on the replay: we can't locate the seam, so append the live run verbatim.
    return [...rest, ...live];
  }
  // Keep only the live segments AFTER the replay's coverage (the genuinely newer tail). A live
  // segment at-or-before the cursor is already in the replay (the seam) and is dropped.
  const tail = live.filter((s) => s.timestamp_ms > restCursor);
  return [...rest, ...tail];
}
