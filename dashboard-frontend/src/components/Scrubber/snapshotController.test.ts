import { describe, it, expect, vi } from 'vitest';
import type { SnapshotResponse } from '../../api/types';
import { SnapshotController, bucketOf, BUCKET_MS } from './snapshotController';

/** A snapshot whose `at_ms` echoes the requested bucket (so we can assert which one delivered). */
function snap(atMs: number): SnapshotResponse {
  return { cursors: { flow_seq: 0, metrics_seq: 0, topology_seq: 0, monitor_seq: 0 }, at_ms: atMs, summaries: [], metrics: null, topology: null };
}

/** A manual rAF: callbacks queue, `flush()` runs them (one frame). */
function manualRaf() {
  let queue: Array<() => void> = [];
  const raf = (cb: () => void) => {
    queue.push(cb);
    return queue.length;
  };
  const cancelRaf = () => { queue = []; };
  const flush = () => {
    const q = queue;
    queue = [];
    for (const cb of q) cb();
  };
  return { raf, cancelRaf, flush, pending: () => queue.length };
}

describe('SnapshotController — rAF coalescing (no fetch storm)', () => {
  it('buckets timestamps to the second', () => {
    expect(bucketOf(1234)).toBe(1000);
    expect(bucketOf(1999)).toBe(1000);
    expect(bucketOf(2000)).toBe(2000);
    expect(BUCKET_MS).toBe(1000);
  });

  it('coalesces many rapid drags within one frame into ONE fetch of the latest bucket', async () => {
    const { raf, cancelRaf, flush } = manualRaf();
    const delivered: number[] = [];
    const fetchSnapshot = vi.fn(async (atMs: number) => snap(atMs));
    const c = new SnapshotController({ fetchSnapshot, onSnapshot: (r) => delivered.push(r.at_ms), raf, cancelRaf });

    // 50 rapid drag events across distinct buckets — the storm scenario.
    for (let i = 0; i < 50; i++) c.requestAt(5000 + i * 137);
    // Before the frame fires, NOTHING has fetched (all coalesced).
    expect(fetchSnapshot).toHaveBeenCalledTimes(0);

    flush();
    await Promise.resolve(); // let the fetch promise resolve
    await Promise.resolve();

    // Exactly ONE fetch — for the LATEST requested bucket — despite 50 events.
    expect(c.fetchCount()).toBe(1);
    expect(fetchSnapshot).toHaveBeenCalledTimes(1);
    const lastBucket = bucketOf(5000 + 49 * 137);
    expect(fetchSnapshot).toHaveBeenCalledWith(lastBucket);
    expect(delivered).toEqual([lastBucket]);
  });

  it('a cached bucket delivers synchronously with ZERO fetch', async () => {
    const { raf, cancelRaf, flush } = manualRaf();
    const delivered: number[] = [];
    const fetchSnapshot = vi.fn(async (atMs: number) => snap(atMs));
    const c = new SnapshotController({ fetchSnapshot, onSnapshot: (r) => delivered.push(r.at_ms), raf, cancelRaf });

    c.requestAt(7300); // bucket 7000
    flush();
    await Promise.resolve();
    await Promise.resolve();
    expect(c.fetchCount()).toBe(1);
    expect(c.has(7300)).toBe(true);

    // Re-request the SAME bucket (e.g. dragging back over it): cache hit, no new fetch.
    delivered.length = 0;
    c.requestAt(7800); // still bucket 7000
    expect(c.fetchCount()).toBe(1); // unchanged
    expect(delivered).toEqual([7000]); // delivered synchronously from cache
  });

  it('issues separate fetches across distinct frames (one per landed bucket)', async () => {
    const { raf, cancelRaf, flush } = manualRaf();
    const fetchSnapshot = vi.fn(async (atMs: number) => snap(atMs));
    const c = new SnapshotController({ fetchSnapshot, onSnapshot: () => {}, raf, cancelRaf });

    c.requestAt(1000);
    flush();
    await Promise.resolve(); await Promise.resolve();
    c.requestAt(20_000);
    flush();
    await Promise.resolve(); await Promise.resolve();

    expect(c.fetchCount()).toBe(2);
    expect(fetchSnapshot).toHaveBeenNthCalledWith(1, 1000);
    expect(fetchSnapshot).toHaveBeenNthCalledWith(2, 20_000);
  });

  it('LRU-evicts beyond capacity', async () => {
    const { raf, cancelRaf, flush } = manualRaf();
    const fetchSnapshot = vi.fn(async (atMs: number) => snap(atMs));
    const c = new SnapshotController({ fetchSnapshot, onSnapshot: () => {}, raf, cancelRaf, cacheCapacity: 3 });

    for (const t of [1000, 2000, 3000, 4000]) {
      c.requestAt(t);
      flush();
      await Promise.resolve(); await Promise.resolve();
    }
    // Capacity 3 → the oldest (1000) was evicted; 2000-4000 remain.
    expect(c.has(1000)).toBe(false);
    expect(c.has(2000)).toBe(true);
    expect(c.has(4000)).toBe(true);

    // Re-requesting the evicted bucket refetches (cache miss).
    const before = c.fetchCount();
    c.requestAt(1000);
    flush();
    await Promise.resolve(); await Promise.resolve();
    expect(c.fetchCount()).toBe(before + 1);
  });

  it('cancel() drops a scheduled frame so no late fetch fires', async () => {
    const { raf, cancelRaf, flush, pending } = manualRaf();
    const fetchSnapshot = vi.fn(async (atMs: number) => snap(atMs));
    const c = new SnapshotController({ fetchSnapshot, onSnapshot: () => {}, raf, cancelRaf });

    c.requestAt(5000);
    expect(pending()).toBe(1);
    c.cancel();
    flush();
    await Promise.resolve();
    expect(c.fetchCount()).toBe(0);
  });
});
