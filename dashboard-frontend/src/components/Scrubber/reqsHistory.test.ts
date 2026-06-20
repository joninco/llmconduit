import { describe, it, expect } from 'vitest';
import {
  appendReqs,
  REQS_MAX_SAMPLES,
  REQS_WINDOW_MS,
  reqsBounds,
  reqsPeak,
  sampleAt,
  xToTime,
  type ReqsSample,
} from './reqsHistory';

describe('reqsHistory', () => {
  it('appends samples in order and reports bounds', () => {
    let r: ReqsSample[] = [];
    r = appendReqs(r, 1000, 2);
    r = appendReqs(r, 2000, 5);
    r = appendReqs(r, 3000, 1);
    expect(r).toHaveLength(3);
    expect(reqsBounds(r)).toEqual({ t0: 1000, tEnd: 3000 });
  });

  it('evicts samples older than the ~30min window relative to now', () => {
    const now = 100_000_000;
    let r: ReqsSample[] = [];
    // A very old sample, then a current one: the old one is evicted.
    r = appendReqs(r, now - REQS_WINDOW_MS - 5000, 9, now - REQS_WINDOW_MS - 5000);
    r = appendReqs(r, now, 3, now);
    expect(r).toHaveLength(1);
    expect(r[0]!.t).toBe(now);
  });

  it('caps total samples defensively', () => {
    let r: ReqsSample[] = [];
    // All within the window (same t) so only the hard cap evicts.
    for (let i = 0; i < REQS_MAX_SAMPLES + 50; i++) r = appendReqs(r, 5000, i, 5000);
    expect(r.length).toBeLessThanOrEqual(REQS_MAX_SAMPLES);
    // The newest survives.
    expect(r[r.length - 1]!.reqs).toBe(REQS_MAX_SAMPLES + 49);
  });

  it('reqsPeak returns the max with a floor of 1', () => {
    expect(reqsPeak([])).toBe(1);
    expect(reqsPeak([{ t: 1, reqs: 0 }])).toBe(1);
    expect(reqsPeak([{ t: 1, reqs: 2 }, { t: 2, reqs: 7 }, { t: 3, reqs: 3 }])).toBe(7);
  });

  it('sampleAt finds the nearest sample to a time', () => {
    const r: ReqsSample[] = [{ t: 1000, reqs: 1 }, { t: 2000, reqs: 2 }, { t: 3000, reqs: 3 }];
    expect(sampleAt(r, 2100)?.reqs).toBe(2);
    expect(sampleAt(r, 2900)?.reqs).toBe(3);
    expect(sampleAt([], 1000)).toBeNull();
  });

  it('xToTime maps a fraction across the span (clamped)', () => {
    const r: ReqsSample[] = [{ t: 1000, reqs: 1 }, { t: 3000, reqs: 3 }];
    expect(xToTime(r, 0)).toBe(1000);
    expect(xToTime(r, 1)).toBe(3000);
    expect(xToTime(r, 0.5)).toBe(2000);
    expect(xToTime(r, 2)).toBe(3000); // clamps above 1
    expect(xToTime(r, -1)).toBe(1000); // clamps below 0
    expect(xToTime([], 0.5)).toBeNull();
  });
});
