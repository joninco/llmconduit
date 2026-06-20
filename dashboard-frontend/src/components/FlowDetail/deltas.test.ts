import { describe, it, expect } from 'vitest';
import type { DebugSegment, FlowDelta } from '../../api/types';
import { mergeDeltas, normalizeRestDeltas, type SeqSegment } from './deltas';

/**
 * The deltas bridge (finding 5): a reloaded/completed flow must replay its `detail.deltas` (the
 * REST snapshot), and live monitor segments must continue from there. These lock the kind/text
 * normalization and the replay-base / live-append merge — de-duped by the MonitorHub SEQUENCE
 * watermark, NOT `timestamp_ms` (finding 2: coalescing keeps the first timestamp, so a coalesced
 * tail or same-millisecond delta would dup/drop under a timestamp cursor).
 */

/** A plain rendered segment (the merge OUTPUT shape). */
function seg(kind: DebugSegment['kind'], text: string, ts = 0): DebugSegment {
  return { timestamp_ms: ts, kind, text };
}
/** A seq-tagged segment (the merge INPUT shape): `seq` is the MonitorHub merge cursor. */
function sseg(kind: DebugSegment['kind'], text: string, seq: number | null, ts = 0): SeqSegment {
  return { segment: seg(kind, text, ts), seq };
}

describe('normalizeRestDeltas — REST FlowDelta[] → SeqSegment[]', () => {
  it('orders by sequence, maps kind/text, carries the seq cursor, and DROPS lifecycle-only deltas', () => {
    const deltas: FlowDelta[] = [
      { sequence: 3, kind: 'response.output_text.delta', payload: { text: ', world' }, ts_ms: 400 },
      { sequence: 1, kind: 'response.created', payload: {}, ts_ms: 100 }, // no text → dropped
      { sequence: 2, kind: 'response.output_text.delta', payload: { text: 'Hello' }, ts_ms: 200 },
    ];
    const out = normalizeRestDeltas(deltas);
    // Sorted by sequence; the lifecycle (text-less) delta is gone; each carries its `sequence`.
    expect(out).toEqual([sseg('output', 'Hello', 2, 200), sseg('output', ', world', 3, 400)]);
  });

  it('classifies reasoning and tool/function-call deltas by their kind string', () => {
    const out = normalizeRestDeltas([
      { sequence: 1, kind: 'response.reasoning_summary.delta', payload: { text: 'thinking' } },
      { sequence: 2, kind: 'response.function_call_arguments.delta', payload: { arguments: '{"a":1}' } },
    ]);
    expect(out[0]).toEqual(sseg('reasoning', 'thinking', 1, 0));
    expect(out[1]).toEqual(sseg('tool', '{"a":1}', 2, 0));
  });

  it('returns [] for undefined / empty deltas', () => {
    expect(normalizeRestDeltas(undefined)).toEqual([]);
    expect(normalizeRestDeltas([])).toEqual([]);
  });
});

describe('mergeDeltas — REST replay (base) + live (appended), de-duped by SEQ WATERMARK (finding 2)', () => {
  it('returns plain DebugSegment[] (the seq cursor is stripped from the rendered output)', () => {
    const out = mergeDeltas([sseg('output', 'Hello', 2, 200)], [sseg('output', ' more', 3, 300)]);
    expect(out).toEqual([seg('output', 'Hello', 200), seg('output', ' more', 300)]);
    // No `seq` field leaks into the panel's segments.
    expect(out.every((s) => !('seq' in s))).toBe(true);
  });

  it('uses the replay as the base and appends the live tail past the replay watermark', () => {
    const rest = [sseg('output', 'Hello', 2)];
    const live = [sseg('output', ' more', 3)];
    expect(mergeDeltas(rest, live)).toEqual([seg('output', 'Hello'), seg('output', ' more')]);
  });

  it('drops a live head already covered by the replay (no doubling), keeping the newer tail', () => {
    // The live ring re-streams history (seq 1,2) then continues (seq 3): the replay covers up to
    // seq 2, so only seq>2 (C) is appended.
    const rest = [sseg('output', 'A', 1), sseg('output', 'B', 2)];
    const live = [sseg('output', 'A', 1), sseg('output', 'B', 2), sseg('output', 'C', 3)];
    expect(mergeDeltas(rest, live)).toEqual([seg('output', 'A'), seg('output', 'B'), seg('output', 'C')]);
  });

  it('falls back cleanly when only one source has content', () => {
    expect(mergeDeltas([], [sseg('output', 'live', 1)])).toEqual([seg('output', 'live')]);
    expect(mergeDeltas([sseg('output', 'rest', 1)], [])).toEqual([seg('output', 'rest')]);
  });

  it('joins a PARTIAL seam overlap by seq without duplicating the shared segment', () => {
    // REST ends at seq 2 (B); the live ring re-emits B (seq 2) then continues to C (seq 3). The
    // watermark is 2, so the shared B is dropped and the seam reads [A, B, C].
    const rest = [sseg('output', 'A', 1), sseg('output', 'B', 2)];
    const live = [sseg('output', 'B', 2), sseg('output', 'C', 3)];
    expect(mergeDeltas(rest, live)).toEqual([seg('output', 'A'), seg('output', 'B'), seg('output', 'C')]);
  });

  it('PRESERVES legitimately-repeated identical segments (same kind+text, distinct seqs)', () => {
    // A model streaming two identical "." tokens at different positions is NOT an overlap. Text
    // equality would have collapsed them; the seq keeps both, then appends the live tail.
    const rest = [sseg('output', '.', 1), sseg('output', '.', 2)];
    const live = [sseg('output', '.', 2), sseg('output', '!', 3)];
    // Watermark 2 → the live '.'@seq2 (the seam) is dropped, '!'@seq3 is appended; BOTH replay dots survive.
    expect(mergeDeltas(rest, live)).toEqual([seg('output', '.'), seg('output', '.'), seg('output', '!')]);
  });

  it('SAME-MILLISECOND deltas merge with no dup and no drop (finding 2: timestamps are not a cursor)', () => {
    // MonitorHub keeps the FIRST timestamp under coalescing, so two distinct deltas can share ts.
    // A timestamp cursor would drop the second same-ms delta; the seq watermark keeps both. REST
    // covers seq 1 (ts 100); live re-sends seq 1 (the seam) then seq 2 at the SAME ts 100.
    const rest = [sseg('output', 'A', 1, 100)];
    const live = [sseg('output', 'A', 1, 100), sseg('output', 'B', 2, 100)];
    // Watermark 1 → seam A dropped, B (same ts, seq 2) appended exactly once.
    expect(mergeDeltas(rest, live)).toEqual([seg('output', 'A', 100), seg('output', 'B', 100)]);
  });

  it('a COALESCED replay tail (first-timestamp, higher seq) still merges by seq', () => {
    // MonitorHub coalesced the replay's tail so its ts (200) is the FIRST of the run while its seq
    // (5) is the LAST. A live segment at seq 6 must append even though its ts (200) equals the
    // coalesced tail's — a timestamp cursor would wrongly drop it.
    const rest = [sseg('output', 'head', 4, 100), sseg('output', 'coalesced-tail', 5, 200)];
    const live = [sseg('output', 'coalesced-tail', 5, 200), sseg('output', 'next', 6, 200)];
    expect(mergeDeltas(rest, live)).toEqual([
      seg('output', 'head', 100), seg('output', 'coalesced-tail', 200), seg('output', 'next', 200),
    ]);
  });

  it('removes a multi-segment seam overlap (live re-sends several tail segments)', () => {
    const rest = [sseg('output', 'A', 1), sseg('output', 'B', 2), sseg('output', 'C', 3)];
    const live = [sseg('output', 'B', 2), sseg('output', 'C', 3), sseg('output', 'D', 4)];
    expect(mergeDeltas(rest, live)).toEqual([
      seg('output', 'A'), seg('output', 'B'), seg('output', 'C'), seg('output', 'D'),
    ]);
  });

  it('appends the whole live run when it is entirely past the replay watermark', () => {
    const rest = [sseg('output', 'A', 1)];
    const live = [sseg('output', 'B', 2), sseg('output', 'C', 3)];
    expect(mergeDeltas(rest, live)).toEqual([seg('output', 'A'), seg('output', 'B'), seg('output', 'C')]);
  });

  it('falls back to appending verbatim when the replay carries NO seq (all null)', () => {
    // A replay that omitted `sequence` cannot be placed by watermark; we append the live run rather
    // than risk dropping a real segment.
    const rest = [sseg('output', 'A', null), sseg('output', 'B', null)];
    const live = [sseg('output', 'C', 3), sseg('output', 'D', 4)];
    expect(mergeDeltas(rest, live)).toEqual([
      seg('output', 'A'), seg('output', 'B'), seg('output', 'C'), seg('output', 'D'),
    ]);
  });

  it('keeps a live segment that carries NO seq (cannot be placed against the watermark → append)', () => {
    const rest = [sseg('output', 'A', 1)];
    const live = [sseg('output', 'B', null)];
    expect(mergeDeltas(rest, live)).toEqual([seg('output', 'A'), seg('output', 'B')]);
  });
});
