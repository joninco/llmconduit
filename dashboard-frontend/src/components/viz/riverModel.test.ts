import { describe, it, expect } from 'vitest';
import {
  buildRivers,
  createRiverFold,
  finalizeLastTerminalRiver,
  finalizeRivers,
  foldRiverMessage,
  gridColumns,
  MAX_RIVERS,
  RIVER_CHANNEL_CHAR_CAP,
  RIVER_ERROR_CHAR_CAP,
} from './riverModel';
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

  it('coalesces ADJACENT tool-argument fragments into ONE card (streamed args of a single call) — D12 R5 MED', () => {
    const monitor: DebugWsMessage[] = [
      upsert('r1', 'gpt-4o'),
      // One tool call whose arguments stream as three fragments.
      seg('r1', 'tool', 'search({"q":"weath', 1300),
      seg('r1', 'tool', 'er in ', 1310),
      seg('r1', 'tool', 'Paris"})', 1320),
    ];
    const [river] = buildRivers(monitor);
    expect(river?.tools).toEqual(['search({"q":"weather in Paris"})']);
  });

  it('a non-tool segment between tool runs SPLITS them into separate cards (distinct tool calls)', () => {
    const monitor: DebugWsMessage[] = [
      upsert('r1', 'gpt-4o'),
      seg('r1', 'tool', 'search(', 1300),
      seg('r1', 'tool', 'a)', 1310),
      // An output (or reasoning) segment marks the boundary between two distinct tool calls.
      seg('r1', 'output', 'thinking', 1320),
      seg('r1', 'tool', 'lookup(', 1330),
      seg('r1', 'tool', 'b)', 1340),
    ];
    const [river] = buildRivers(monitor);
    expect(river?.tools).toEqual(['search(a)', 'lookup(b)']);
  });

  it('splits BACK-TO-BACK distinct tool calls on the backend boundary marker (no interleaving kind) — D12 R6', () => {
    // Two distinct calls stream consecutively with NO output/reasoning between them, so they coalesce
    // into ONE tool run here. The backend (monitor.rs) prefixes each call with a `tool arguments <id>:`
    // header; splitting on that marker yields one card PER call (sharing DeltasPanel's boundary rule).
    const monitor: DebugWsMessage[] = [
      upsert('r1', 'gpt-4o'),
      // First call: header + fragmented argument JSON.
      seg('r1', 'tool', 'tool arguments call_aaa:\n{"name":"get_weather",', 1300),
      seg('r1', 'tool', '"arguments":{"city":"SF"}}\n', 1310),
      // Second call arrives back-to-back (still kind: tool) — its own boundary header opens a new card.
      seg('r1', 'tool', 'tool arguments call_bbb:\n{"name":"get_time","arguments":{"tz":"PT"}}', 1320),
    ];
    const [river] = buildRivers(monitor);
    expect(river?.tools).toEqual([
      'tool arguments call_aaa:\n{"name":"get_weather","arguments":{"city":"SF"}}',
      'tool arguments call_bbb:\n{"name":"get_time","arguments":{"tz":"PT"}}',
    ]);
  });

  it('keeps a single call (one boundary header) as ONE card even when its arguments are fragmented — D12 R6', () => {
    // A lone call carries exactly one `tool arguments <id>:` header; its fragments must stay ONE card
    // (the boundary split must not over-split a single call's streamed arguments).
    const monitor: DebugWsMessage[] = [
      upsert('r1', 'gpt-4o'),
      seg('r1', 'tool', 'tool arguments call_aaa:\n{"name":"get_', 1300),
      seg('r1', 'tool', 'weather","arguments":{"city":"SF"}}', 1310),
    ];
    const [river] = buildRivers(monitor);
    expect(river?.tools).toEqual([
      'tool arguments call_aaa:\n{"name":"get_weather","arguments":{"city":"SF"}}',
    ]);
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

  it('retains request timing + bounded failure detail and clears the error on recovery', () => {
    const hugeError = `upstream rejected: ${'x'.repeat(RIVER_ERROR_CHAR_CAP + 100)}`;
    let fold = createRiverFold();
    fold = foldRiverMessage(fold, upsert('r1', 'm'));
    fold = foldRiverMessage(fold, {
      type: 'request_status', response_id: 'r1', status: 'failed', completed_at_ms: 2_500, error: hugeError,
    });

    let [river] = finalizeRivers(fold);
    expect(river).toMatchObject({ startedAtMs: 1_000, status: 'failed', terminalAtMs: 2_500 });
    expect(river?.error).toHaveLength(RIVER_ERROR_CHAR_CAP);
    expect(river?.error?.startsWith('upstream rejected:')).toBe(true);
    expect(river?.error?.endsWith('…')).toBe(true);

    fold = foldRiverMessage(fold, {
      type: 'request_status', response_id: 'r1', status: 'running', completed_at_ms: null, error: null,
    });
    [river] = finalizeRivers(fold);
    expect(river).toMatchObject({ status: 'running', error: null, terminalAtMs: null });
  });

  it('preserves first-seen order across multiple rivers', () => {
    const rivers = buildRivers([upsert('a', 'm'), upsert('b', 'm'), upsert('c', 'm')]);
    expect(rivers.map((r) => r.id)).toEqual(['a', 'b', 'c']);
  });
});

describe('incremental fold — memory caps + immutability (theater ring-eviction fix)', () => {
  it('folds one message at a time into the SAME rivers buildRivers produces', () => {
    const msgs: DebugWsMessage[] = [
      upsert('r1', 'gpt-4o'),
      seg('r1', 'reasoning', 'why', 1000),
      seg('r1', 'output', 'text', 1100),
    ];
    let fold = createRiverFold();
    for (const m of msgs) fold = foldRiverMessage(fold, m);
    expect(finalizeRivers(fold)).toEqual(buildRivers(msgs));
  });

  it('fold updates are immutable: a captured fold reference is frozen against later messages', () => {
    let fold = createRiverFold();
    fold = foldRiverMessage(fold, upsert('r1', 'm'));
    fold = foldRiverMessage(fold, seg('r1', 'output', 'before', 1000));
    const captured = {
      rivers: new Map(fold.rivers),
      order: [...fold.order],
      lastTerminal: fold.lastTerminal,
    }; // the baseline copy
    fold = foldRiverMessage(fold, seg('r1', 'output', ' after', 1100));
    expect(finalizeRivers(captured)[0]?.output).toBe('before'); // capture unchanged
    expect(finalizeRivers(fold)[0]?.output).toBe('before after');
  });

  it('non-river messages return the SAME fold reference (cheap no-change detection)', () => {
    let fold = createRiverFold();
    fold = foldRiverMessage(fold, upsert('r1', 'm'));
    const next = foldRiverMessage(fold, { type: 'snapshot_done' });
    expect(next).toBe(fold);
  });

  it('retains exactly the newest terminal response after monitor removal', () => {
    let fold = createRiverFold();
    fold = foldRiverMessage(fold, upsert('r1', 'first'));
    fold = foldRiverMessage(fold, seg('r1', 'output', 'first answer', 1_000));
    fold = foldRiverMessage(fold, {
      type: 'request_status', response_id: 'r1', status: 'completed', completed_at_ms: 2_000, error: null,
    });
    expect(finalizeLastTerminalRiver(fold)?.output).toBe('first answer');

    // The normal active map obeys the monitor removal, while the one-response cache survives.
    fold = foldRiverMessage(fold, { type: 'request_remove', response_id: 'r1', reason: 'evicted' });
    expect(finalizeRivers(fold)).toEqual([]);
    expect(finalizeLastTerminalRiver(fold)).toMatchObject({ id: 'r1', terminalAtMs: 2_000 });

    // A later terminal response replaces the cache; this never grows into a completed archive.
    fold = foldRiverMessage(fold, upsert('r2', 'second'));
    fold = foldRiverMessage(fold, seg('r2', 'output', 'second answer', 3_000));
    fold = foldRiverMessage(fold, {
      type: 'request_status', response_id: 'r2', status: 'failed', completed_at_ms: 4_000, error: 'boom',
    });
    expect(finalizeLastTerminalRiver(fold)).toMatchObject({ id: 'r2', output: 'second answer', status: 'failed' });
  });

  it('head-trims a channel past RIVER_CHANNEL_CHAR_CAP and flags `truncated` (honest cap, not silent)', () => {
    let fold = createRiverFold();
    fold = foldRiverMessage(fold, upsert('r1', 'm'));
    fold = foldRiverMessage(fold, seg('r1', 'output', 'HEAD-'.repeat(1) + 'x'.repeat(RIVER_CHANNEL_CHAR_CAP - 5), 1000));
    let [river] = finalizeRivers(fold);
    expect(river?.truncated).toBe(false); // exactly at cap — untouched
    fold = foldRiverMessage(fold, seg('r1', 'output', 'y'.repeat(10), 1100));
    [river] = finalizeRivers(fold);
    expect(river?.truncated).toBe(true);
    expect(river?.output.length).toBeLessThanOrEqual(RIVER_CHANNEL_CHAR_CAP);
    expect(river?.output.startsWith('HEAD-')).toBe(false); // trimmed from the TOP
    expect(river?.output.endsWith('y'.repeat(10))).toBe(true); // tail intact
  });

  it('caps tracked rivers at MAX_RIVERS, evicting the oldest TERMINAL river first (never a running one)', () => {
    let fold = createRiverFold();
    for (let i = 0; i < MAX_RIVERS; i++) fold = foldRiverMessage(fold, upsert(`r${i}`, 'm'));
    // r0 running, r1 completed → creating one more evicts r1 (oldest terminal), not r0.
    fold = foldRiverMessage(fold, { type: 'request_status', response_id: 'r1', status: 'completed', completed_at_ms: 2000, error: null });
    fold = foldRiverMessage(fold, upsert('new', 'm'));
    const ids = finalizeRivers(fold).map((r) => r.id);
    expect(ids).toHaveLength(MAX_RIVERS);
    expect(ids).toContain('r0');
    expect(ids).toContain('new');
    expect(ids).not.toContain('r1');
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
