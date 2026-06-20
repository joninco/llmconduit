/**
 * SnapshotController — the rAF-throttled, LRU-cached `/snapshot?at=` fetch coordinator behind the
 * scrubber drag. This is the piece the acceptance pins: "rapid drags coalesce to bounded fetches"
 * (no request storm on drag).
 *
 * Contract:
 *  - `requestAt(tsMs)` is called on EVERY pointer move during a drag. The timestamp is bucketed to
 *    the second (the LRU key — the snapshot mechanism is 5 s-granular per D5, so sub-second
 *    precision is meaningless and would defeat the cache). A bucket already in the LRU delivers
 *    SYNCHRONOUSLY from cache — zero fetch.
 *  - A bucket NOT in the cache schedules a SINGLE rAF-coalesced fetch. Many `requestAt` calls
 *    within one frame collapse to ONE fetch of the LATEST requested bucket (intermediate buckets
 *    are skipped — you only care where the playhead landed). So N drag events ⇒ ≤1 fetch/frame.
 *  - One in-flight fetch at a time; a newer target supersedes a pending frame. A resolved fetch
 *    is cached (LRU, capacity-bounded) and, IF it is still the latest request, delivered.
 *  - `onSnapshot(resp)` broadcasts the frozen cut to the store (the caller wires `applySnapshot`).
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
   * Request the snapshot at `tsMs`. Cache hit → deliver synchronously (no fetch). Miss → record
   * as the latest target and schedule a single rAF-coalesced fetch (collapsing this frame's
   * requests to one fetch of the LATEST bucket).
   */
  requestAt(tsMs: number): void {
    const bucket = bucketOf(tsMs);
    const cached = this.cache.get(bucket);
    if (cached) {
      this.touch(bucket, cached);
      this.onSnapshot(cached);
      return;
    }
    this.pendingBucket = bucket;
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
      this.onSnapshot(cached);
      return;
    }
    // One fetch in flight at a time; if the same bucket is already fetching, wait for it.
    if (this.inFlight === bucket) return;
    this.inFlight = bucket;
    this.fetches += 1;
    this.fetchSnapshot(bucket)
      .then((resp) => {
        this.touch(bucket, resp);
        // Deliver only if this is STILL the latest requested bucket (else a newer drag won).
        if (this.pendingBucket === bucket) this.onSnapshot(resp);
      })
      .catch((err) => this.onError(err))
      .finally(() => {
        this.inFlight = null;
        // A newer bucket may have been requested while this was in flight → schedule it.
        if (this.pendingBucket !== null && this.pendingBucket !== bucket) this.scheduleFrame();
      });
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

  /** Cancel any scheduled frame and clear the pending target (called on resume/unmount). */
  cancel(): void {
    if (this.frame !== null) {
      this.cancelRaf(this.frame);
      this.frame = null;
    }
    this.pendingBucket = null;
  }
}
