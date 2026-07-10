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
 * ones: the REST replay is the BASE (chronological, ordinal-ordered) and live segments whose
 * monitor sequence exceeds the replay's explicit coverage watermark are APPENDED. The two clocks
 * stay separate: `FlowDelta.sequence` orders the replay only, while `monitorSeq` places live data.
 */
import type { DebugSegment, DebugSegmentKind, FlowDelta } from '../../api/types';

/**
 * A LIVE `DebugSegment` tagged with its MonitorHub sequence (`monitor_seq` /
 * `DebugUpdate.sequence`). REST replay entries deliberately do not use this type: their
 * `FlowDelta.sequence` is a per-flow ordinal, not a monitor cursor.
 */
export interface MonitorSegment {
  segment: DebugSegment;
  monitorSeq: number | null;
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
 * Normalizes the REST replay (`FlowDelta[]`) into ordered `DebugSegment`s. `sequence` is used ONLY
 * for this sort: Rust documents it as a per-flow replay ordinal, so it is never carried forward as
 * a live monitor cursor. Deltas with no textual content are dropped. `ts_ms` seeds the display
 * timestamp only (0 when absent); it is also not a cursor because coalescing keeps the first time.
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
    .filter((segment): segment is DebugSegment => segment !== null);
}

/**
 * Merges the REST replay (base) with the live monitor segments (appended). The replay anchors the
 * stream for a reloaded/completed flow; live segments continue it. The two sources OVERLAP at the
 * seam: the live ring retains the recent history the replay already holds.
 *
 * `deltasThroughMonitorSeq` is captured by Rust from the SAME MonitorHub snapshot that produced the
 * replay. It is the only valid cross-source cursor. A live segment extends the replay iff its
 * `monitorSeq` is strictly greater than that watermark. We never compare the replay's per-flow
 * ordinal or its timestamp with live monitor sequence values. This preserves repeated identical
 * content and same-millisecond deltas while removing the snapshot/live overlap exactly.
 *
 * An older host may omit the additive watermark. In that degraded case the seam cannot be placed,
 * so all live segments are appended rather than risking silent data loss. With a watermark present,
 * a live segment lacking a monitor sequence is not provably newer and is therefore excluded.
 */
export function mergeDeltas(
  rest: DebugSegment[],
  live: MonitorSegment[],
  deltasThroughMonitorSeq?: number | null,
): DebugSegment[] {
  if (live.length === 0) return rest;
  const tail = deltasThroughMonitorSeq == null
    ? live
    : live.filter((entry) => entry.monitorSeq !== null && entry.monitorSeq > deltasThroughMonitorSeq);
  return [...rest, ...tail.map((entry) => entry.segment)];
}
