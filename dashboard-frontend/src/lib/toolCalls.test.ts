import { describe, it, expect } from 'vitest';
import { TOOL_CALL_BOUNDARY, splitToolCallText } from './toolCalls';

/**
 * The shared per-tool-call boundary rule (D10 finding 5 / D12 R6): both the inspector's `DeltasPanel`
 * and the theater's `riverModel` split a coalesced `tool` run on the backend's `tool arguments <id>:`
 * header into one entry per distinct call. These lock that rule so the two consumers can't drift.
 */
describe('TOOL_CALL_BOUNDARY — the backend per-call header line', () => {
  it('matches a `tool arguments <id>:` line and ignores anything else', () => {
    expect(TOOL_CALL_BOUNDARY.test('tool arguments call_abc:')).toBe(true);
    expect(TOOL_CALL_BOUNDARY.test('tool arguments call_abc: ')).toBe(true); // trailing ws allowed
    expect(TOOL_CALL_BOUNDARY.test('{"name":"get_weather"}')).toBe(false);
    expect(TOOL_CALL_BOUNDARY.test('  tool arguments call_abc:')).toBe(false); // not at line start
  });
});

describe('splitToolCallText — split a coalesced tool run on the boundary marker', () => {
  it('splits BACK-TO-BACK calls into one entry per call (header kept as the entry head)', () => {
    const text = [
      'tool arguments call_aaa:',
      '{"name":"get_weather","arguments":{"city":"SF"}}',
      'tool arguments call_bbb:',
      '{"name":"get_time","arguments":{"tz":"PT"}}',
    ].join('\n');
    expect(splitToolCallText(text)).toEqual([
      'tool arguments call_aaa:\n{"name":"get_weather","arguments":{"city":"SF"}}',
      'tool arguments call_bbb:\n{"name":"get_time","arguments":{"tz":"PT"}}',
    ]);
  });

  it('keeps a single call (one header) as ONE entry — no over-split', () => {
    const text = 'tool arguments call_aaa:\n{"name":"get_weather","arguments":{"city":"SF"}}';
    expect(splitToolCallText(text)).toEqual([text]);
  });

  it('keeps a MARKERLESS run (lone streamed call, no header) as ONE entry (finding 2)', () => {
    expect(splitToolCallText('{"name":"only_one"}')).toEqual(['{"name":"only_one"}']);
  });

  it('treats text BEFORE the first boundary as its own entry', () => {
    const text = 'leading{"a":1}\ntool arguments call_aaa:\n{"name":"b"}';
    expect(splitToolCallText(text)).toEqual(['leading{"a":1}', 'tool arguments call_aaa:\n{"name":"b"}']);
  });

  it('never silently drops an all-whitespace run (returns it as a single entry)', () => {
    expect(splitToolCallText('   ')).toEqual(['   ']);
  });
});
