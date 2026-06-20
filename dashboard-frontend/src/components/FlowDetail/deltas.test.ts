import { describe, it, expect } from 'vitest';
import type { DebugSegment, FlowDelta } from '../../api/types';
import { mergeDeltas, normalizeRestDeltas } from './deltas';

/**
 * The deltas bridge (finding 5): a reloaded/completed flow must replay its `detail.deltas` (the
 * REST snapshot), and live monitor segments must continue from there. These lock the kind/text
 * normalization and the replay-base / live-append merge (incl. de-dup of a re-streamed prefix).
 */

function seg(kind: DebugSegment['kind'], text: string, ts = 0): DebugSegment {
  return { timestamp_ms: ts, kind, text };
}

describe('normalizeRestDeltas — REST FlowDelta[] → DebugSegment[]', () => {
  it('orders by sequence, maps kind/text, and DROPS lifecycle-only deltas', () => {
    const deltas: FlowDelta[] = [
      { sequence: 3, kind: 'response.output_text.delta', payload: { text: ', world' }, ts_ms: 400 },
      { sequence: 1, kind: 'response.created', payload: {}, ts_ms: 100 }, // no text → dropped
      { sequence: 2, kind: 'response.output_text.delta', payload: { text: 'Hello' }, ts_ms: 200 },
    ];
    const out = normalizeRestDeltas(deltas);
    // Sorted by sequence; the lifecycle (text-less) delta is gone.
    expect(out).toEqual([seg('output', 'Hello', 200), seg('output', ', world', 400)]);
  });

  it('classifies reasoning and tool/function-call deltas by their kind string', () => {
    const out = normalizeRestDeltas([
      { sequence: 1, kind: 'response.reasoning_summary.delta', payload: { text: 'thinking' } },
      { sequence: 2, kind: 'response.function_call_arguments.delta', payload: { arguments: '{"a":1}' } },
    ]);
    expect(out[0]).toEqual(seg('reasoning', 'thinking', 0));
    expect(out[1]).toEqual(seg('tool', '{"a":1}', 0));
  });

  it('returns [] for undefined / empty deltas', () => {
    expect(normalizeRestDeltas(undefined)).toEqual([]);
    expect(normalizeRestDeltas([])).toEqual([]);
  });
});

describe('mergeDeltas — REST replay (base) + live (appended), de-duped by TEMPORAL CURSOR (finding 3)', () => {
  it('uses the replay as the base and appends the live tail past the replay cursor', () => {
    const rest = [seg('output', 'Hello', 100)];
    const live = [seg('output', ' more', 200)];
    expect(mergeDeltas(rest, live)).toEqual([seg('output', 'Hello', 100), seg('output', ' more', 200)]);
  });

  it('drops a live head already covered by the replay (no doubling), keeping the newer tail', () => {
    // The live ring re-streams history (ts 100,200) then continues (ts 300): the replay covers up
    // to ts 200, so only ts>200 (C) is appended.
    const rest = [seg('output', 'A', 100), seg('output', 'B', 200)];
    const live = [seg('output', 'A', 100), seg('output', 'B', 200), seg('output', 'C', 300)];
    expect(mergeDeltas(rest, live)).toEqual([seg('output', 'A', 100), seg('output', 'B', 200), seg('output', 'C', 300)]);
  });

  it('falls back cleanly when only one source has content', () => {
    expect(mergeDeltas([], [seg('output', 'live')])).toEqual([seg('output', 'live')]);
    expect(mergeDeltas([seg('output', 'rest')], [])).toEqual([seg('output', 'rest')]);
  });

  it('joins a PARTIAL seam overlap by cursor without duplicating the shared segment', () => {
    // REST ends at ts 200 (B); the live ring re-emits B (ts 200) then continues to C (ts 300). The
    // replay cursor is 200, so the shared B is dropped and the seam reads [A, B, C].
    const rest = [seg('output', 'A', 100), seg('output', 'B', 200)];
    const live = [seg('output', 'B', 200), seg('output', 'C', 300)];
    expect(mergeDeltas(rest, live)).toEqual([seg('output', 'A', 100), seg('output', 'B', 200), seg('output', 'C', 300)]);
  });

  it('PRESERVES legitimately-repeated identical segments (same kind+text, distinct timestamps)', () => {
    // A model streaming two identical "." tokens at different instants is NOT an overlap. Text
    // equality would have collapsed them; the cursor keeps both, then appends the live tail.
    const rest = [seg('output', '.', 100), seg('output', '.', 200)];
    const live = [seg('output', '.', 200), seg('output', '!', 300)];
    // Replay cursor 200 → the live '.'@200 (the seam) is dropped, '!'@300 is appended; BOTH replay
    // dots survive.
    expect(mergeDeltas(rest, live)).toEqual([
      seg('output', '.', 100), seg('output', '.', 200), seg('output', '!', 300),
    ]);
  });

  it('removes a multi-segment seam overlap (live re-sends several tail segments)', () => {
    const rest = [seg('output', 'A', 100), seg('output', 'B', 200), seg('output', 'C', 300)];
    const live = [seg('output', 'B', 200), seg('output', 'C', 300), seg('output', 'D', 400)];
    expect(mergeDeltas(rest, live)).toEqual([
      seg('output', 'A', 100), seg('output', 'B', 200), seg('output', 'C', 300), seg('output', 'D', 400),
    ]);
  });

  it('appends the whole live run when it is entirely past the replay cursor', () => {
    const rest = [seg('output', 'A', 100)];
    const live = [seg('output', 'B', 200), seg('output', 'C', 300)];
    expect(mergeDeltas(rest, live)).toEqual([seg('output', 'A', 100), seg('output', 'B', 200), seg('output', 'C', 300)]);
  });

  it('falls back to appending verbatim when the replay carries no cursor (all ts=0)', () => {
    // A replay that omitted ts_ms cannot be temporally placed; we append the live run rather than
    // risk dropping a real segment by text-collapsing.
    const rest = [seg('output', 'A'), seg('output', 'B')];
    const live = [seg('output', 'C'), seg('output', 'D')];
    expect(mergeDeltas(rest, live)).toEqual([
      seg('output', 'A'), seg('output', 'B'), seg('output', 'C'), seg('output', 'D'),
    ]);
  });
});
