import { describe, it, expect } from 'vitest';
import { buildRivers, gridColumns } from './riverModel';
import type { DebugWsMessage, DebugRequestStatus } from '../../api/types';

function upsert(id: string, model: string, status: DebugRequestStatus = 'running'): DebugWsMessage {
  return {
    type: 'request_upsert',
    request: {
      response_id: id, model, started_at_ms: 1000, updated_at_ms: 1000, completed_at_ms: null, status,
      stats: { input_items: 0, tool_count: 0, turn_count: 0, user_messages: 0, assistant_messages: 0, system_messages: 0, developer_messages: 0, reasoning_items: 0, function_calls: 0, function_outputs: 0, tool_items: 0, input_chars: 0, instructions_chars: 0 },
      error: null,
    },
  };
}
function seg(id: string, kind: 'output' | 'reasoning' | 'tool', text: string, ts: number): DebugWsMessage {
  return { type: 'segment_append', response_id: id, segment: { timestamp_ms: ts, kind, text } };
}

describe('buildRivers — folds the monitor ring into per-stream rivers', () => {
  it('groups output/reasoning/tool deltas by response_id with the model from upsert', () => {
    const monitor: DebugWsMessage[] = [
      upsert('r1', 'gpt-4o'),
      seg('r1', 'output', 'Hello', 1000),
      seg('r1', 'output', ', world', 1200),
      seg('r1', 'reasoning', 'thinking…', 1100),
      seg('r1', 'tool', 'search(q)', 1300),
    ];
    const [river] = buildRivers(monitor);
    expect(river?.id).toBe('r1');
    expect(river?.model).toBe('gpt-4o');
    expect(river?.output).toBe('Hello, world');
    expect(river?.reasoning).toBe('thinking…');
    expect(river?.tools).toEqual(['search(q)']);
  });

  it('derives tokens/sec from the segment timestamp window (≈ chars/4 over elapsed)', () => {
    const monitor: DebugWsMessage[] = [
      upsert('r1', 'm'),
      // 40 output chars over 2s → ≈10 tokens / 2s = 5 tok/s.
      seg('r1', 'output', 'x'.repeat(40), 1000),
      seg('r1', 'output', '', 3000),
    ];
    const [river] = buildRivers(monitor);
    expect(river?.tokensPerSec).toBeCloseTo(5, 5);
  });

  it('a single-timestamp river has 0 tok/s (no measurable rate yet)', () => {
    const [river] = buildRivers([upsert('r1', 'm'), seg('r1', 'output', 'hi', 1000)]);
    expect(river?.tokensPerSec).toBe(0);
  });

  it('request_status updates the river status; request_remove drops it', () => {
    const completed = buildRivers([
      upsert('r1', 'm'),
      seg('r1', 'output', 'done', 1000),
      { type: 'request_status', response_id: 'r1', status: 'completed', completed_at_ms: 2000, error: null },
    ]);
    expect(completed[0]?.status).toBe('completed');

    const removed = buildRivers([
      upsert('r1', 'm'),
      upsert('r2', 'm'),
      { type: 'request_remove', response_id: 'r1', reason: 'evicted' },
    ]);
    expect(removed.map((r) => r.id)).toEqual(['r2']);
  });

  it('preserves first-seen order across multiple rivers', () => {
    const rivers = buildRivers([upsert('a', 'm'), upsert('b', 'm'), upsert('c', 'm')]);
    expect(rivers.map((r) => r.id)).toEqual(['a', 'b', 'c']);
  });
});

describe('gridColumns — auto-grid 1 / 2 / 3-6', () => {
  it('1 river → 1 col, 2 → 2 cols, 3-6 → 3 cols', () => {
    expect(gridColumns(1)).toBe(1);
    expect(gridColumns(2)).toBe(2);
    expect(gridColumns(3)).toBe(3);
    expect(gridColumns(6)).toBe(3);
  });
});
