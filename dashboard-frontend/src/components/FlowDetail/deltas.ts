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
 * A `DebugSegment` tagged with its MonitorHub SEQUENCE (`monitor_seq` / `DebugUpdate.sequence`) —
 * the authoritative cross-source merge cursor (finding 2). Both sources express position in the one
 * stream as this seq: the REST replay carries it as `FlowDelta.sequence`, the live ring as the
 * per-message `monitorSeqs` the socket stamps. `seq` is `null` only when a source omitted it (an old
 * replay, or a live segment with no stamp), which forces the degraded append-verbatim fallback.
 */
export interface SeqSegment {
  segment: DebugSegment;
  seq: number | null;
}

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
 * Normalizes the REST replay (`FlowDelta[]`) into ordered `SeqSegment`s. Sorts by `sequence` (the
 * authoritative replay order), maps each kind/payload, carries the delta's `sequence` as the merge
 * cursor (finding 2), and DROPS deltas with no textual content (pure lifecycle events) so they don't
 * render as empty blocks. `ts_ms` seeds the segment timestamp for display ONLY (0 when absent — and
 * unreliable as a cursor since MonitorHub coalescing keeps the FIRST timestamp).
 */
export function normalizeRestDeltas(deltas: FlowDelta[] | undefined): SeqSegment[] {
  if (!deltas || deltas.length === 0) return [];
  return [...deltas]
    .sort((a, b) => a.sequence - b.sequence)
    .map((d): SeqSegment | null => {
      const text = extractText(d.payload);
      if (text === '') return null;
      return { segment: { timestamp_ms: d.ts_ms ?? 0, kind: classifyKind(d.kind), text }, seq: d.sequence };
    })
    .filter((s): s is SeqSegment => s !== null);
}

/**
 * Merges the REST replay (base) with the live monitor segments (appended). The replay anchors the
 * stream for a reloaded/completed flow; live segments continue it. The two sources OVERLAP at the
 * seam: the live ring retains the recent history the replay already holds.
 *
 * We de-dup by each segment's MonitorHub SEQUENCE (`seq`), NOT by `timestamp_ms` and NOT by text
 * (finding 2). `timestamp_ms` is unusable as a cursor: MonitorHub COALESCES adjacent same-kind
 * segments but keeps the FIRST timestamp, so a tail and its coalesced sibling can share a millisecond
 * (a strict `>` over timestamps then drops a real same-millisecond delta, or duplicates a coalesced
 * tail). The seq is the per-message MonitorHub cursor carried identically by BOTH sources (the REST
 * `FlowDelta.sequence` and the live `monitorSeqs`), so it is the one monotonic watermark of stream
 * position. The replay COVERS the stream up to its MAX seq, and a live segment belongs to the
 * un-replayed tail iff its seq is strictly GREATER than that watermark. This:
 *   - removes a partial/multi-segment seam overlap precisely (the live head at/under the watermark
 *     is already in the replay and is dropped);
 *   - preserves legitimately-repeated identical segments (same kind+text — DISTINCT seqs), because
 *     identity is the seq, not the text;
 *   - never drops a genuine same-millisecond delta (the seq, not the clock, places the seam).
 * When the watermark is unusable (the replay carries NO seq on any segment — e.g. an old replay that
 * omitted `sequence`), we cannot place the seam, so we APPEND the whole live run verbatim:
 * over-keeping a duplicate is safer than silently dropping a real segment. Order is preserved:
 * replay first, then the live tail past the watermark.
 */
export function mergeDeltas(rest: SeqSegment[], live: SeqSegment[]): DebugSegment[] {
  if (rest.length === 0) return live.map((s) => s.segment);
  if (live.length === 0) return rest.map((s) => s.segment);
  // The replay's coverage watermark = the max seq it carries. A replay with no usable seq on ANY
  // segment yields `null` → no watermark (see below). (`-Infinity` start cleanly handles an
  // all-null replay; a single real seq lifts it to a concrete watermark.)
  const restWatermark = rest.reduce<number | null>(
    (max, s) => (s.seq === null ? max : Math.max(max ?? Number.NEGATIVE_INFINITY, s.seq)),
    null,
  );
  if (restWatermark === null) {
    // No seq watermark on the replay: we can't locate the seam, so append the live run verbatim.
    return [...rest, ...live].map((s) => s.segment);
  }
  // Keep only the live segments AFTER the replay's coverage (the genuinely newer tail). A live
  // segment at-or-before the watermark is already in the replay (the seam) and is dropped. A live
  // segment with NO seq cannot be placed against the watermark, so it is kept (append-not-drop).
  const tail = live.filter((s) => s.seq === null || s.seq > restWatermark);
  return [...rest, ...tail].map((s) => s.segment);
}
