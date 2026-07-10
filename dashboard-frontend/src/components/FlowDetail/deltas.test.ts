import { describe, it, expect } from 'vitest';
import type { DebugSegment, FlowDelta } from '../../api/types';
import { mergeDeltas, normalizeRestDeltas, type MonitorSegment } from './deltas';

/** A plain rendered segment (the merge output and normalized REST shape). */
function seg(kind: DebugSegment['kind'], text: string, ts = 0): DebugSegment {
  return { timestamp_ms: ts, kind, text };
}

/** A live segment tagged with its real MonitorHub sequence. */
function mseg(
  kind: DebugSegment['kind'],
  text: string,
  monitorSeq: number | null,
  ts = 0,
): MonitorSegment {
  return { segment: seg(kind, text, ts), monitorSeq };
}

describe('normalizeRestDeltas — FlowDelta ordinal orders replay only', () => {
  it('orders by the per-flow ordinal, maps text, and drops lifecycle-only deltas', () => {
    const deltas: FlowDelta[] = [
      { sequence: 30, kind: 'response.output_text.delta', payload: { text: ', world' }, ts_ms: 400 },
      { sequence: 10, kind: 'response.created', payload: {}, ts_ms: 100 },
      { sequence: 20, kind: 'response.output_text.delta', payload: { text: 'Hello' }, ts_ms: 200 },
    ];

    const out = normalizeRestDeltas(deltas);

    expect(out).toEqual([seg('output', 'Hello', 200), seg('output', ', world', 400)]);
    expect(out.every((segment) => !('sequence' in segment) && !('monitorSeq' in segment))).toBe(true);
  });

  it('preserves repeated, same-millisecond replay content with distinct ordinals', () => {
    expect(normalizeRestDeltas([
      { sequence: 0, kind: 'segment.output', payload: { text: '.' }, ts_ms: 100 },
      { sequence: 1, kind: 'segment.output', payload: { text: '.' }, ts_ms: 100 },
    ])).toEqual([seg('output', '.', 100), seg('output', '.', 100)]);
  });

  it('classifies reasoning and tool/function-call deltas by kind', () => {
    expect(normalizeRestDeltas([
      { sequence: 1, kind: 'response.reasoning_summary.delta', payload: { text: 'thinking' } },
      { sequence: 2, kind: 'response.function_call_arguments.delta', payload: { arguments: '{"a":1}' } },
    ])).toEqual([seg('reasoning', 'thinking'), seg('tool', '{"a":1}')]);
  });

  it('returns [] for undefined or empty deltas', () => {
    expect(normalizeRestDeltas(undefined)).toEqual([]);
    expect(normalizeRestDeltas([])).toEqual([]);
  });
});

describe('mergeDeltas — explicit monitor watermark places the replay/live seam', () => {
  it('does not compare the replay ordinal with the unrelated monitor sequence clock', () => {
    const rest = normalizeRestDeltas([
      { sequence: 0, kind: 'segment.output', payload: { text: 'A' }, ts_ms: 10 },
      { sequence: 1, kind: 'segment.output', payload: { text: 'B' }, ts_ms: 20 },
    ]);
    const live = [
      mseg('output', 'A', 40, 10),
      mseg('output', 'B', 40, 20),
      mseg('output', 'C', 41, 30),
    ];

    expect(mergeDeltas(rest, live, 40)).toEqual([
      seg('output', 'A', 10),
      seg('output', 'B', 20),
      seg('output', 'C', 30),
    ]);
  });

  it('preserves legitimately repeated identical content after the watermark', () => {
    const rest = normalizeRestDeltas([
      { sequence: 0, kind: 'segment.output', payload: { text: '.' }, ts_ms: 100 },
      { sequence: 1, kind: 'segment.output', payload: { text: '.' }, ts_ms: 100 },
    ]);
    const live = [
      mseg('output', '.', 77, 100), // snapshot overlap
      mseg('output', '.', 78, 100), // genuinely new identical token
    ];

    expect(mergeDeltas(rest, live, 77)).toEqual([
      seg('output', '.', 100),
      seg('output', '.', 100),
      seg('output', '.', 100),
    ]);
  });

  it('preserves a genuine same-millisecond live tail', () => {
    const rest = [seg('output', 'A', 100)];
    const live = [mseg('output', 'A', 9, 100), mseg('output', 'B', 10, 100)];

    expect(mergeDeltas(rest, live, 9)).toEqual([
      seg('output', 'A', 100),
      seg('output', 'B', 100),
    ]);
  });

  it('applies the watermark even when the textual replay is empty', () => {
    const live = [mseg('output', 'covered', 5), mseg('output', 'new', 6)];
    expect(mergeDeltas([], live, 5)).toEqual([seg('output', 'new')]);
  });

  it('appends all live segments when an older host omitted the additive watermark', () => {
    const rest = [seg('output', 'replay')];
    const live = [mseg('output', 'possibly-overlapping', 5), mseg('output', 'unstamped', null)];
    expect(mergeDeltas(rest, live)).toEqual([
      seg('output', 'replay'),
      seg('output', 'possibly-overlapping'),
      seg('output', 'unstamped'),
    ]);
  });

  it('excludes an unstamped live segment when a watermark makes newness mandatory', () => {
    expect(mergeDeltas(
      [seg('output', 'replay')],
      [mseg('output', 'unknown-position', null), mseg('output', 'new', 8)],
      7,
    )).toEqual([seg('output', 'replay'), seg('output', 'new')]);
  });
});
