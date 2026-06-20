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

describe('mergeDeltas — REST replay (base) + live (appended)', () => {
  it('uses the replay as the base and appends live segments', () => {
    const rest = [seg('output', 'Hello')];
    const live = [seg('output', ' more')];
    expect(mergeDeltas(rest, live)).toEqual([seg('output', 'Hello'), seg('output', ' more')]);
  });

  it('drops the live prefix that duplicates the replay head (no doubling)', () => {
    // The live ring re-streams history then continues: the leading duplicate is skipped.
    const rest = [seg('output', 'A'), seg('output', 'B')];
    const live = [seg('output', 'A'), seg('output', 'B'), seg('output', 'C')];
    expect(mergeDeltas(rest, live)).toEqual([seg('output', 'A'), seg('output', 'B'), seg('output', 'C')]);
  });

  it('falls back cleanly when only one source has content', () => {
    expect(mergeDeltas([], [seg('output', 'live')])).toEqual([seg('output', 'live')]);
    expect(mergeDeltas([seg('output', 'rest')], [])).toEqual([seg('output', 'rest')]);
  });
});
