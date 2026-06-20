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
  /**
   * The bucket most recently DELIVERED to `onSnapshot` (D11 R4 finding 2). A cut delivers at most
   * once: a cache-served bucket records itself here, so a stale in-flight fetch settling afterwards
   * does NOT re-deliver the same already-served cut (which would re-install the frozen cut and
   * double-bump the store's `connEpoch`). Reset by `cancel()` so a fresh seek can re-deliver a
   * bucket it landed on before the resume.
   */
  private lastDelivered: number | null = null;
  /** The rAF handle for the scheduled coalesced fetch (null when none scheduled). */
  private frame: number | null = null;
  /** Bucket currently being fetched (one in flight at a time). */
  private inFlight: number | null = null;
  /**
   * Monotonic cancellation generation (D11 R2 finding 2). Each started fetch captures the current
   * value; `cancel()` bumps it. A resolving fetch whose captured generation is stale (a `cancel()`
   * intervened) drops its response AND skips `settle` — so it can NOT clear the in-flight slot a
   * newer cycle now owns. `cancel()` frees `inFlight` immediately (not when the abandoned request
   * settles), so a subsequent seek can start a NEW fetch right away even if the old one hangs.
   */
  private generation = 0;
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
      // (D11 R4 finding 2) This coalesced frame is reached via `settle()` after a stale in-flight
      // fetch resolves. If the latest target was ALREADY delivered (e.g. a `requestAt` cache hit
      // served it while the stale fetch was outstanding), do NOT re-deliver — re-firing the same
      // cut re-installs the frozen seek view and double-bumps the store's `connEpoch`. A genuinely
      // new target (not yet delivered) still delivers here.
      if (this.lastDelivered !== bucket) this.deliverIfLatest(bucket, cached);
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
    // Tag this fetch with the current generation; a `cancel()` bumps it and orphans this request.
    const gen = this.generation;
    this.fetchSnapshot(bucket)
      .then((resp) => {
        // (R2 finding 2) A canceled request is fully orphaned: it must NOT deliver and must NOT
        // `settle` (which would clear the in-flight slot a newer cycle now owns). It is still cached
        // (a future drag back can reuse it) but otherwise dropped.
        if (gen !== this.generation) {
          this.touch(bucket, resp);
          return;
        }
        this.touch(bucket, resp);
        // (finding 3) Deliver only if this bucket is STILL the latest requested; a newer drag (or a
        // newer cached delivery) that moved `pendingBucket` wins, and this stale response is dropped.
        this.deliverIfLatest(bucket, resp);
        this.settle(bucket);
      })
      .catch((err) => {
        // Orphaned (canceled) rejection: swallow without touching the in-flight slot.
        if (gen !== this.generation) return;
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
    const next = this.pendingBucket;
    if (next === null || next === bucket) return;
    // (D11 R4 finding 2) If the newer pending bucket is ALREADY cached AND already delivered, there
    // is nothing left to do — scheduling a follow-up frame would only re-run `runPending` to
    // re-deliver the same cached cut (double-bumping the store's `connEpoch`). Skip it.
    if (this.lastDelivered === next && this.cache.has(next)) return;
    this.scheduleFrame();
  }

  /**
   * Broadcast a cut ONLY if `bucket` is still the latest requested target (finding 3). Records it as
   * `lastDelivered` so the `settle()`/`runPending` follow-up path can detect an already-served cut
   * and skip re-delivering it (D11 R4 finding 2). A direct `requestAt` cache hit and a fresh fetched
   * resolve always deliver (real user/network progress); only the coalesced follow-up frame guards
   * against re-firing an already-delivered cached cut.
   */
  private deliverIfLatest(bucket: number, resp: SnapshotResponse): void {
    if (this.pendingBucket !== bucket) return;
    this.lastDelivered = bucket;
    this.onSnapshot(resp);
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
   * Cancel any scheduled frame and clear the pending target (called on resume/unmount). Bumping the
   * generation ORPHANS any still-in-flight fetch (its late response is dropped, not delivered) — so a
   * LIVE resume during an in-flight seek can't be clobbered by a late cut landing after the user
   * already went live. Critically, the in-flight slot is freed HERE (R2 finding 2), not when the
   * abandoned request eventually settles: a subsequent seek can therefore start a NEW fetch
   * immediately even if the canceled request hangs (otherwise `runPending` would refuse forever while
   * `inFlight !== null`). Clearing `pendingBucket` additionally disarms delivery of any in-flight
   * fetch that somehow shares the generation.
   */
  cancel(): void {
    if (this.frame !== null) {
      this.cancelRaf(this.frame);
      this.frame = null;
    }
    // Invalidate the in-flight fetch's generation and free the slot so the next seek isn't blocked.
    this.generation += 1;
    this.inFlight = null;
    this.pendingBucket = null;
    // Clear the delivered marker so a fresh seek after a LIVE resume can re-deliver a bucket it
    // landed on before — the resume discarded the frozen cut, so re-seeking it is a real delivery
    // (D11 R4 finding 2).
    this.lastDelivered = null;
  }
}
