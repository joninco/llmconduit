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

  it('holds STRICTLY ONE fetch in flight against a slow backend, then fires only the latest bucket (finding 2)', async () => {
    const { raf, cancelRaf, flush } = manualRaf();
    // A deferred backend: each call parks a resolver so we control completion order (a slow server).
    const resolvers: Array<{ bucket: number; resolve: () => void }> = [];
    const fetchSnapshot = vi.fn(
      (atMs: number) =>
        new Promise<SnapshotResponse>((res) => {
          resolvers.push({ bucket: atMs, resolve: () => res(snap(atMs)) });
        }),
    );
    const delivered: number[] = [];
    const c = new SnapshotController({ fetchSnapshot, onSnapshot: (r) => delivered.push(r.at_ms), raf, cancelRaf });

    // 60 rapid drags across distinct buckets, each in its OWN frame (flush per move) — the storm.
    // The backend never resolves during the drag, so the one-in-flight guard must hold.
    for (let i = 0; i < 60; i++) {
      c.requestAt(10_000 + i * 1000);
      flush();
      await Promise.resolve();
      // At most ONE request is ever outstanding despite 60 drags against the slow backend.
      expect(resolvers.length).toBeLessThanOrEqual(1);
    }
    const firstBucket = bucketOf(10_000);
    const latestBucket = bucketOf(10_000 + 59 * 1000);
    // Exactly one fetch started (the first); the other 59 coalesced behind it.
    expect(c.fetchCount()).toBe(1);
    expect(resolvers).toHaveLength(1);
    expect(resolvers[0]!.bucket).toBe(firstBucket);

    // The slow first request finally resolves. It is NO LONGER the latest → it must NOT deliver
    // (finding 3), and its `settle` must fire EXACTLY the latest pending bucket (finding 2).
    resolvers[0]!.resolve();
    await Promise.resolve(); await Promise.resolve();
    flush(); // the follow-up frame for the latest bucket
    await Promise.resolve();
    expect(resolvers).toHaveLength(2);
    expect(resolvers[1]!.bucket).toBe(latestBucket);

    resolvers[1]!.resolve();
    await Promise.resolve(); await Promise.resolve();
    // Only the LATEST bucket was ever delivered — the stale first response was dropped.
    expect(delivered).toEqual([latestBucket]);
    expect(c.fetchCount()).toBe(2);
  });

  it('cancel() disarms an in-flight fetch so a late cut does not deliver after LIVE resume', async () => {
    const { raf, cancelRaf, flush } = manualRaf();
    const resolvers: Array<{ bucket: number; resolve: () => void }> = [];
    const fetchSnapshot = vi.fn(
      (atMs: number) =>
        new Promise<SnapshotResponse>((res) => {
          resolvers.push({ bucket: atMs, resolve: () => res(snap(atMs)) });
        }),
    );
    const delivered: number[] = [];
    const c = new SnapshotController({ fetchSnapshot, onSnapshot: (r) => delivered.push(r.at_ms), raf, cancelRaf });

    // Start a seek fetch and hold it in flight (slow backend).
    c.requestAt(4000);
    flush();
    await Promise.resolve();
    expect(resolvers).toHaveLength(1);

    // User hits LIVE before it lands → cancel() clears the latest-target marker.
    c.cancel();

    // The slow fetch finally resolves AFTER cancel — it must NOT deliver (no seek cut re-installed).
    resolvers[0]!.resolve();
    await Promise.resolve(); await Promise.resolve();
    expect(delivered).toEqual([]);
  });

  it('cancel() frees the in-flight slot so a subsequent seek starts a NEW fetch immediately (R2 finding 2)', async () => {
    const { raf, cancelRaf, flush } = manualRaf();
    const resolvers: Array<{ bucket: number; resolve: () => void }> = [];
    const fetchSnapshot = vi.fn(
      (atMs: number) =>
        new Promise<SnapshotResponse>((res) => {
          resolvers.push({ bucket: atMs, resolve: () => res(snap(atMs)) });
        }),
    );
    const delivered: number[] = [];
    const c = new SnapshotController({ fetchSnapshot, onSnapshot: (r) => delivered.push(r.at_ms), raf, cancelRaf });

    // Start a seek fetch and hold it in flight (a HUNG/slow backend — it never resolves).
    c.requestAt(4000);
    flush();
    await Promise.resolve();
    expect(resolvers).toHaveLength(1);
    expect(resolvers[0]!.bucket).toBe(4000);

    // User hits LIVE → cancel(). The old fetch is still outstanding (never settled).
    c.cancel();

    // A SUBSEQUENT seek to a NEW bucket must start a NEW fetch right away — not be blocked forever by
    // the still-hung first request (the bug: `inFlight` stayed set until the old fetch settled).
    c.requestAt(8000);
    flush();
    await Promise.resolve();
    expect(c.fetchCount()).toBe(2);
    expect(resolvers).toHaveLength(2);
    expect(resolvers[1]!.bucket).toBe(8000);

    // The NEW fetch delivers normally.
    resolvers[1]!.resolve();
    await Promise.resolve(); await Promise.resolve();
    expect(delivered).toEqual([8000]);

    // The aborted FIRST request's late response, arriving after cancel, is ignored (no extra deliver,
    // and it does not clobber the slot the new fetch owns).
    resolvers[0]!.resolve();
    await Promise.resolve(); await Promise.resolve();
    expect(delivered).toEqual([8000]); // unchanged — the orphaned 4000 response was dropped
  });

  it('a stale in-flight fetch does NOT overwrite a newer CACHED seek (finding 3)', async () => {
    const { raf, cancelRaf, flush } = manualRaf();
    const resolvers: Array<{ bucket: number; resolve: () => void }> = [];
    const fetchSnapshot = vi.fn(
      (atMs: number) =>
        new Promise<SnapshotResponse>((res) => {
          resolvers.push({ bucket: atMs, resolve: () => res(snap(atMs)) });
        }),
    );
    const delivered: number[] = [];
    const c = new SnapshotController({ fetchSnapshot, onSnapshot: (r) => delivered.push(r.at_ms), raf, cancelRaf });

    // Prime the cache with bucket 9000 (a newer instant the user will drag back to), via a fetch.
    c.requestAt(9000);
    flush();
    await Promise.resolve();
    resolvers[0]!.resolve();
    await Promise.resolve(); await Promise.resolve();
    expect(c.has(9000)).toBe(true);
    delivered.length = 0;

    // Start an OLD fetch (bucket 3000) and hold it in flight (slow backend).
    c.requestAt(3000);
    flush();
    await Promise.resolve();
    expect(resolvers).toHaveLength(2);
    expect(resolvers[1]!.bucket).toBe(3000);

    // Now the user drags to the newer CACHED bucket 9000 → delivered synchronously from cache. This
    // moves the latest-request marker to 9000 (finding 3: marker updates on a cache hit too).
    c.requestAt(9000);
    expect(delivered).toEqual([9000]);

    // The slow OLD fetch (3000) finally resolves. Because 9000 is now the latest, the stale 3000
    // response must be DROPPED — it must not overwrite the newer cached 9000 cut.
    resolvers[1]!.resolve();
    await Promise.resolve(); await Promise.resolve();
    expect(delivered).toEqual([9000]); // unchanged — 3000 did NOT deliver
  });
});
