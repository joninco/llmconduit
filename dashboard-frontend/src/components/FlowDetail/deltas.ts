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
 * stream for a reloaded/completed flow; live segments continue it. When the live ring re-streams
 * the same history the replay already holds, the live stream STARTS by repeating the replayed
 * segments — so we drop the live prefix that matches the replay head (by kind+text, since the two
 * sources may carry different timestamps) and append only the genuinely newer tail. Order is
 * preserved: replay first, then the un-duplicated live remainder.
 */
export function mergeDeltas(rest: DebugSegment[], live: DebugSegment[]): DebugSegment[] {
  if (rest.length === 0) return live;
  if (live.length === 0) return rest;
  const sameSeg = (a: DebugSegment, b: DebugSegment) => a.kind === b.kind && a.text === b.text;
  // Skip the leading live segments that duplicate the replay (live re-sends history-then-live).
  let skip = 0;
  while (skip < live.length && skip < rest.length && sameSeg(live[skip]!, rest[skip]!)) {
    skip += 1;
  }
  return [...rest, ...live.slice(skip)];
}
