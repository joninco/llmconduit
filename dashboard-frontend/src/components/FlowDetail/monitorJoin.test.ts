import { describe, it, expect } from 'vitest';
import type { DebugWsMessage } from '../../api/types';
import { joinMonitor } from './monitorJoin';

function segment(responseId: string, kind: 'output' | 'reasoning' | 'tool', text: string, ts = 1): DebugWsMessage {
  return { type: 'segment_append', response_id: responseId, segment: { timestamp_ms: ts, kind, text } };
}

describe('joinMonitor — filters the ring to one response_id', () => {
  const ring: DebugWsMessage[] = [
    segment('resp_001', 'output', 'Hello'),
    segment('resp_999', 'output', 'OTHER FLOW'),
    segment('resp_001', 'output', ', world'),
    { type: 'event_append', response_id: 'resp_001', event: { timestamp_ms: 2, kind: 'response.created', summary: 'created', images: [] } },
    { type: 'request_status', response_id: 'resp_001', status: 'completed', completed_at_ms: 3, error: null },
  ];

  it('selects only this response_id segments, in order', () => {
    const j = joinMonitor(ring, 'resp_001');
    expect(j.segments.map((s) => s.text)).toEqual(['Hello', ', world']);
    expect(j.events.map((e) => e.kind)).toEqual(['response.created']);
    expect(j.status).toBe('completed');
  });

  it('returns an empty join for a null response_id (flow not yet linked)', () => {
    const j = joinMonitor(ring, null);
    expect(j.segments).toHaveLength(0);
    expect(j.events).toHaveLength(0);
    expect(j.status).toBeNull();
  });

  it('captures the latest error from request_status', () => {
    const j = joinMonitor([{ type: 'request_status', response_id: 'r', status: 'failed', completed_at_ms: 1, error: 'boom' }], 'r');
    expect(j.error).toBe('boom');
    expect(j.status).toBe('failed');
  });

  it('bounds the join to a seek cut: excludes messages past maxSeq (finding 1)', () => {
    const ringSeek: DebugWsMessage[] = [
      segment('resp_001', 'output', 'in-cut-1'),
      segment('resp_001', 'output', 'in-cut-2'),
      segment('resp_001', 'output', 'post-cut'),
    ];
    // Lockstep arrival seqs: first two at the cut (seq 2), the third after (seq 3).
    const seqs = [2, 2, 3];
    const j = joinMonitor(ringSeek, 'resp_001', { seqs, maxSeq: 2 });
    expect(j.segments.map((s) => s.text)).toEqual(['in-cut-1', 'in-cut-2']);
  });

  it('applies no bound when maxSeq is null (LIVE — whole ring is current)', () => {
    const j = joinMonitor(ring, 'resp_001', { seqs: [1, 1, 1, 1, 1], maxSeq: null });
    expect(j.segments.map((s) => s.text)).toEqual(['Hello', ', world']);
  });
});
