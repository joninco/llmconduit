/**
 * SnapshotController — the rAF-throttled, LRU-cached `/snapshot?at=` fetch coordinator behind the
 * scrubber drag. This is the piece the acceptance pins: "rapid drags coalesce to bounded fetches"
 * (no request storm on drag).
 *
 * Contract:
 *  - `requestAt(tsMs)` is called on EVERY pointer move during a drag. The timestamp is bucketed to
 *    the second (the LRU key — the snapshot mechanism is 5 s-granular per D5, so sub-second
 *    precision is meaningless and would defeat the cache). The bucket is recorded as the LATEST
 *    requested target BEFORE anything else (cache check included), so every delivery — cached OR
 *    fetched — can verify it is still the latest before applying (finding 3: no stale overwrite). A
 *    bucket already in the LRU delivers SYNCHRONOUSLY from cache — zero fetch.
 *  - A bucket NOT in the cache schedules a SINGLE rAF-coalesced fetch. Many `requestAt` calls
 *    within one frame collapse to ONE fetch of the LATEST requested bucket (intermediate buckets
 *    are skipped — you only care where the playhead landed). So N drag events ⇒ ≤1 fetch/frame.
 *  - STRICTLY ONE in-flight fetch at a time (finding 2): while a request is in flight NO new fetch
 *    starts, regardless of bucket. The latest requested bucket is recorded; the in-flight request's
 *    `finally` fires EXACTLY that latest bucket (if it still needs fetching), coalescing every
 *    intermediate drag. A slow backend therefore sees ≤1 concurrent request even under a 60 Hz drag.
 *  - Every resolved fetch is cached (LRU, capacity-bounded) and delivered ONLY if its bucket is
 *    still the latest requested (else a newer drag won — the stale response is dropped, finding 3).
 *  - `onSnapshot(resp)` broadcasts the frozen cut to the store (the caller wires `applySeekCut`).
 *
 * All side-effecting seams (`fetchSnapshot`, `raf`/`cancelRaf`, `now`) are injected so the unit
 * test drives frames deterministically and asserts the fetch count without real timers.
 */
import type { SnapshotResponse } from '../../api/types';

/** Snapshot bucket granularity (ms). D5 snapshots are 5 s-coordinated; 1 s keys are ample. */
export const BUCKET_MS = 1000;

export interface SnapshotControllerOptions {
  /** Fetch a snapshot as of `atMs`. Injected (real: `client.snapshot`). */
  fetchSnapshot: (atMs: number) => Promise<SnapshotResponse>;
  /** Deliver a resolved (or cache-hit) snapshot. Real: broadcast into the store. */
  onSnapshot: (resp: SnapshotResponse) => void;
  /** Optional error sink (swallowed by default so a drag never throws). */
  onError?: (err: unknown) => void;
  /** rAF seam. Real: `requestAnimationFrame`. Returns a handle. */
  raf?: (cb: () => void) => number;
  /** cancel-rAF seam. Real: `cancelAnimationFrame`. */
  cancelRaf?: (handle: number) => void;
  /** LRU capacity (distinct second-buckets retained). */
  cacheCapacity?: number;
}

/** Bucket a timestamp to the controller's granularity (the LRU key). */
export function bucketOf(tsMs: number): number {
  return Math.floor(tsMs / BUCKET_MS) * BUCKET_MS;
}

export class SnapshotController {
  private readonly fetchSnapshot: (atMs: number) => Promise<SnapshotResponse>;
  private readonly onSnapshot: (resp: SnapshotResponse) => void;
  private readonly onError: (err: unknown) => void;
  private readonly raf: (cb: () => void) => number;
  private readonly cancelRaf: (handle: number) => void;
  private readonly cacheCapacity: number;

  /** LRU of bucket → snapshot (insertion-ordered Map; re-insert on hit to mark MRU). */
  private readonly cache = new Map<number, SnapshotResponse>();
  /** The latest requested bucket (the one a coalesced frame will fetch + deliver). */
  private pendingBucket: number | null = null;
  /** The rAF handle for the scheduled coalesced fetch (null when none scheduled). */
  private frame: number | null = null;
  /** Bucket currently being fetched (one in flight at a time). */
  private inFlight: number | null = null;
  /** Observable fetch count (the storm guard the test asserts). */
  private fetches = 0;

  constructor(opts: SnapshotControllerOptions) {
    this.fetchSnapshot = opts.fetchSnapshot;
    this.onSnapshot = opts.onSnapshot;
    this.onError = opts.onError ?? (() => {});
    this.raf = opts.raf ?? ((cb) => (typeof requestAnimationFrame !== 'undefined' ? requestAnimationFrame(cb) : (setTimeout(cb, 16) as unknown as number)));
    this.cancelRaf = opts.cancelRaf ?? ((h) => (typeof cancelAnimationFrame !== 'undefined' ? cancelAnimationFrame(h) : clearTimeout(h)));
    this.cacheCapacity = opts.cacheCapacity ?? 64;
  }

  /** Total fetches issued (test seam: asserts rapid drags coalesce, NO per-event storm). */
  fetchCount(): number {
    return this.fetches;
  }

  /** True if a bucket is cached (test/diagnostic). */
  has(tsMs: number): boolean {
    return this.cache.has(bucketOf(tsMs));
  }

  /**
   * Request the snapshot at `tsMs`. The bucket is recorded as the LATEST target FIRST (before the
   * cache check), so any later-resolving in-flight fetch for an older bucket can detect it is no
   * longer latest and drop itself (finding 3). Cache hit → deliver synchronously (no fetch). Miss →
   * schedule a single rAF-coalesced fetch (collapsing this frame's requests to one fetch of the
   * latest bucket).
   */
  requestAt(tsMs: number): void {
    const bucket = bucketOf(tsMs);
    // (finding 3) Mark latest BEFORE the cache check: a cache HIT must move the marker too, else a
    // slower in-flight OLDER fetch would still see itself as latest and overwrite this newer cut.
    this.pendingBucket = bucket;
    const cached = this.cache.get(bucket);
    if (cached) {
      this.touch(bucket, cached);
      this.deliverIfLatest(bucket, cached);
      return;
    }
    this.scheduleFrame();
  }

  /** Schedule (once) the coalesced fetch for the end of the current animation frame. */
  private scheduleFrame(): void {
    if (this.frame !== null) return; // already scheduled this frame
    this.frame = this.raf(() => {
      this.frame = null;
      this.runPending();
    });
  }

  /** Fire the fetch for the latest pending bucket (skips intermediate buckets entirely). */
  private runPending(): void {
    const bucket = this.pendingBucket;
    if (bucket === null) return;
    // If the latest target became cached (a prior fetch landed on it) deliver it without refetch.
    const cached = this.cache.get(bucket);
    if (cached) {
      this.touch(bucket, cached);
      this.deliverIfLatest(bucket, cached);
      return;
    }
    // (finding 2) STRICTLY one in flight: while ANY request is outstanding, do not start another —
    // the running request's `finally` will pick up the latest pending bucket. This caps concurrency
    // at 1 even when rapid drags cross many buckets against a slow backend.
    if (this.inFlight !== null) return;
    this.startFetch(bucket);
  }

  /** Issue exactly one fetch for `bucket`, delivering on resolve only if still the latest target. */
  private startFetch(bucket: number): void {
    this.inFlight = bucket;
    this.fetches += 1;
    this.fetchSnapshot(bucket)
      .then((resp) => {
        this.touch(bucket, resp);
        // (finding 3) Deliver only if this bucket is STILL the latest requested; a newer drag (or a
        // newer cached delivery) that moved `pendingBucket` wins, and this stale response is dropped.
        this.deliverIfLatest(bucket, resp);
        this.settle(bucket);
      })
      .catch((err) => {
        this.onError(err);
        this.settle(bucket);
      });
  }

  /**
   * Clear the in-flight marker and, if a NEWER bucket was requested while this fetch was running,
   * schedule exactly ONE follow-up frame for it (finding 2: every intermediate drag is skipped —
   * only the latest pending bucket runs). The marker is cleared HERE (in the resolve/reject handler,
   * not a deferred `.finally`) so the very next `runPending` already sees `inFlight === null` and is
   * not blocked by a stale in-flight value. The follow-up goes through `runPending`, whose
   * one-in-flight guard keeps concurrency at 1.
   */
  private settle(bucket: number): void {
    this.inFlight = null;
    if (this.pendingBucket !== null && this.pendingBucket !== bucket) this.scheduleFrame();
  }

  /** Broadcast a cut ONLY if `bucket` is still the latest requested target (finding 3). */
  private deliverIfLatest(bucket: number, resp: SnapshotResponse): void {
    if (this.pendingBucket === bucket) this.onSnapshot(resp);
  }

  /** Insert/refresh a cache entry as MRU and evict the LRU beyond capacity. */
  private touch(bucket: number, resp: SnapshotResponse): void {
    if (this.cache.has(bucket)) this.cache.delete(bucket);
    this.cache.set(bucket, resp);
    while (this.cache.size > this.cacheCapacity) {
      const oldest = this.cache.keys().next().value as number | undefined;
      if (oldest === undefined) break;
      this.cache.delete(oldest);
    }
  }

  /**
   * Cancel any scheduled frame and clear the pending target (called on resume/unmount). Clearing
   * `pendingBucket` also DISARMS any still-in-flight fetch: its `deliverIfLatest` will see no
   * matching latest target and drop the response — so a LIVE resume during an in-flight seek can't
   * be clobbered by a late cut landing after the user already went live.
   */
  cancel(): void {
    if (this.frame !== null) {
      this.cancelRaf(this.frame);
      this.frame = null;
    }
    this.pendingBucket = null;
  }
}
