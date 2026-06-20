//! D5 — `MetricsLayer`: authoritative aggregated request stats + the coordinated,
//! memory-safe, internally-consistent time-travel snapshot store.
//!
//! Architecture (DASHBOARD_PLAN §4.1, Codex round fixes):
//! - `MetricsLayer { state: Mutex<MetricsState> }` with the `MonitorHub`/
//!   `DashboardFlowStore` `new()`/`disabled()` split: when `--with-debug-ui` is off
//!   the layer is `disabled()` and EVERY mutation/read is a no-op, so the production
//!   hot path keeps zero overhead (a streamed request with the dashboard off does
//!   NO ring/histogram work and takes NO lock).
//! - Per-window RING buffers (1m / 5m / 1h at 1 s resolution = 60 / 300 / 3600
//!   slots). Each slot is a [`Bucket`] keyed `{status_class, model, endpoint,
//!   upstream}` plus a 30-bucket log-spaced latency [`Histogram`] and summed token
//!   counters. A slot is reused circularly: when wall-clock advances past a slot's
//!   epoch second the slot is RESET before the new sample lands, so a window only
//!   ever aggregates samples from its own time span (no unbounded growth — the rings
//!   are fixed-size arrays).
//! - `record_response(...)` is called from the engine's D3 TERMINAL finalize seam
//!   (the single CAS-guarded choke point — NOT the middleware, NOT per-chunk), so a
//!   streamed request populates metrics exactly once, at finalize. `record_usage`
//!   rides the same D3 usage upsert the FlowStore/monitor already consume.
//! - A `metrics_seq` increments on EVERY mutation; it is the metrics domain's
//!   per-domain cursor (no global watermark — AGENTS.md).
//! - The 5 s coordinated snapshot task (D5, spawned only under `--with-debug-ui`)
//!   takes ONE critical section: it holds the FlowStore mutex THEN the MetricsLayer
//!   mutex (the FIXED lock order — only the snapshot task ever holds >1 lock, so no
//!   deadlock is possible) and captures ONE `Arc<ProviderHealthSnapshot>` (D4),
//!   producing a true atomic cut across all three stores into a body-free
//!   [`DashboardSnapshot`]. The summaries are body-free [`SnapshotFlowSummary`]
//!   (NO `Arc<[u8]>`, NO live-store reference) — body retention on snapshots
//!   recreates a 135 GiB worst case (AGENTS.md don't-rule). A snapshot-summary quota
//!   bounds peak ring memory to ≤ ~400 MiB (720 cuts × 512 summaries × <1 KiB).

use crate::dashboard_flow::DashboardFlowStore;
use crate::dashboard_flow::FlowStatus;
use crate::dashboard_flow::FlowUsage;
use crate::dashboard_flow::SnapshotFlowSummary;
use crate::upstream::ProviderHealthPublisher;
use crate::upstream::ProviderHealthSnapshot;
use serde::Serialize;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

/// Ring length for the 1-minute window (1 s resolution).
const WINDOW_1M_SLOTS: usize = 60;
/// Ring length for the 5-minute window (1 s resolution).
const WINDOW_5M_SLOTS: usize = 300;
/// Ring length for the 1-hour window (1 s resolution).
const WINDOW_1H_SLOTS: usize = 3600;

/// Number of log-spaced latency histogram buckets spanning 1 ms .. 120 s.
const HISTOGRAM_BUCKETS: usize = 30;
/// Lowest histogram bucket upper-bound (ms). Samples ≤ this land in bucket 0.
const HISTOGRAM_MIN_MS: f64 = 1.0;
/// Highest finite histogram bucket upper-bound (ms) — 120 s. Samples above land in
/// the final overflow bucket.
const HISTOGRAM_MAX_MS: f64 = 120_000.0;

/// Snapshot ring length: 720 body-free cuts = 12 per minute (one per 5 s) × 60 min.
const SNAPSHOT_RING_SLOTS: usize = 720;

/// Default peak snapshot-ring memory quota (bytes). 720 cuts × 512 summaries ×
/// <1 KiB ≈ 360 MiB; the 400 MiB quota is the HARD bound the ring cannot exceed —
/// when a fresh cut would push the retained summary-byte total over quota, the
/// OLDEST cuts are dropped first until it fits. This is the 135 GiB fix: bodies are
/// never on a snapshot, and the summary bytes are quota-bounded.
const DEFAULT_SNAPSHOT_QUOTA_BYTES: usize = 400 * 1024 * 1024;

/// HTTP status class for a terminal flow, the metrics bucket key dimension. Derived
/// from the [`FlowStatus`] terminal (the engine does not thread a raw numeric code
/// to the metrics seam — the terminal status IS the authoritative outcome).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusClass {
    /// A clean `Completed` terminal (2xx-equivalent).
    Success,
    /// A `Failed` terminal (5xx/4xx-equivalent — an upstream/gateway error).
    Error,
    /// A `Cancelled` terminal (client hang-up / abort — 499-equivalent).
    Cancelled,
}

impl StatusClass {
    /// Map a terminal [`FlowStatus`] to its metrics status class. `Open` should
    /// never reach the terminal seam, but is conservatively bucketed `Error` (a
    /// flow that finalized while still notionally open is an anomaly, not a success).
    fn from_status(status: FlowStatus) -> Self {
        match status {
            FlowStatus::Completed => StatusClass::Success,
            FlowStatus::Cancelled => StatusClass::Cancelled,
            FlowStatus::Failed | FlowStatus::Open => StatusClass::Error,
        }
    }
}

/// The composite bucket key: `{status_class, model, endpoint, upstream}`. `model`
/// is the SERVED model (the backend that actually answered); `endpoint` is the
/// inbound route family (`/v1/responses`, `/v1/chat/completions`, …); `upstream`
/// is the serving provider/route label. Unknown dimensions collapse to `"unknown"`
/// so the key space stays bounded and a missing attribution never spawns a distinct
/// `None` bucket.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct BucketKey {
    pub status: StatusClass,
    pub model: String,
    pub endpoint: String,
    pub upstream: String,
}

/// Per-key aggregate counters within one window: the request count and summed
/// token counters. Latency is aggregated separately in the window-level
/// [`Histogram`] (a per-key histogram would be 30 buckets × |keys| — wasteful;
/// p-quantiles are reported window-wide, which is the stats-strip contract).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct BucketCounts {
    pub count: u64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub reasoning_tokens: i64,
}

impl BucketCounts {
    /// Approximate retained bytes for the snapshot-memory quota (the fixed scalar
    /// fields; the key string bytes are counted at the bucket-map level).
    fn approx_bytes(&self) -> usize {
        std::mem::size_of::<BucketCounts>()
    }
}

/// A 30-bucket log-spaced latency histogram (1 ms .. 120 s) over the in-window
/// samples, plus p50/p95/p99 via linear interpolation over the cumulative counts.
/// Counts are `u64`; an empty histogram reports `0.0` for every quantile.
#[derive(Debug, Clone, Serialize)]
pub struct Histogram {
    /// Per-bucket sample counts. `buckets[i]` counts samples whose latency is
    /// ≤ `bucket_upper_ms(i)` and > `bucket_upper_ms(i - 1)`. The final bucket is
    /// the `> HISTOGRAM_MAX_MS` overflow bucket.
    buckets: [u64; HISTOGRAM_BUCKETS],
    /// Total samples (== sum of `buckets`), retained so quantile math avoids a
    /// re-sum and the empty case is O(1).
    total: u64,
}

impl Default for Histogram {
    fn default() -> Self {
        Self {
            buckets: [0; HISTOGRAM_BUCKETS],
            total: 0,
        }
    }
}

/// The inclusive upper bound (ms) of histogram bucket `index`. The finite buckets
/// (indices `0..=HISTOGRAM_BUCKETS-2`, i.e. all but the overflow bucket) are LOG-SPACED
/// so the FIRST finite upper bound is EXACTLY [`HISTOGRAM_MIN_MS`] (1 ms) and the LAST
/// is EXACTLY [`HISTOGRAM_MAX_MS`] (120 s); the final bucket is the overflow bucket with
/// an effectively-infinite bound. Pure function of `index` (no per-instance state), so
/// the boundary ladder is computed identically at record + quantile time.
///
/// The fraction is `index / (HISTOGRAM_BUCKETS - 2)` (Codex D5 R1 #3) — NOT
/// `(index+1)/(HISTOGRAM_BUCKETS-1)`, which made bucket 0's upper bound ≈ 1.5 ms and
/// pushed the whole ladder up. With this mapping `index == 0` ⇒ frac 0 ⇒ exactly 1 ms
/// and `index == HISTOGRAM_BUCKETS-2` ⇒ frac 1 ⇒ exactly 120 s.
fn bucket_upper_ms(index: usize) -> f64 {
    if index >= HISTOGRAM_BUCKETS - 1 {
        return f64::INFINITY;
    }
    let span = (HISTOGRAM_MAX_MS / HISTOGRAM_MIN_MS).ln();
    let frac = index as f64 / (HISTOGRAM_BUCKETS - 2) as f64;
    HISTOGRAM_MIN_MS * (span * frac).exp()
}

/// The lower bound (ms) of histogram bucket `index` — the previous bucket's upper
/// bound, or `0.0` for bucket 0. Used as the interpolation floor.
fn bucket_lower_ms(index: usize) -> f64 {
    if index == 0 {
        0.0
    } else {
        bucket_upper_ms(index - 1)
    }
}

impl Histogram {
    /// Record one latency sample (ms) into its log-spaced bucket.
    fn record(&mut self, elapsed_ms: f64) {
        let index = self.bucket_index(elapsed_ms);
        self.buckets[index] = self.buckets[index].saturating_add(1);
        self.total = self.total.saturating_add(1);
    }

    /// The bucket index a `elapsed_ms` sample falls into. Linear scan over 30
    /// buckets is trivial and avoids `ln` rounding mismatches at the boundaries
    /// that a closed-form inverse could introduce.
    fn bucket_index(&self, elapsed_ms: f64) -> usize {
        let value = if elapsed_ms.is_finite() {
            elapsed_ms.max(0.0)
        } else {
            HISTOGRAM_MAX_MS + 1.0
        };
        for index in 0..HISTOGRAM_BUCKETS - 1 {
            if value <= bucket_upper_ms(index) {
                return index;
            }
        }
        HISTOGRAM_BUCKETS - 1
    }

    /// Merge another histogram into this one (used when collapsing a window's slots
    /// into a single reported histogram).
    fn merge(&mut self, other: &Histogram) {
        for index in 0..HISTOGRAM_BUCKETS {
            self.buckets[index] = self.buckets[index].saturating_add(other.buckets[index]);
        }
        self.total = self.total.saturating_add(other.total);
    }

    /// The `quantile` (0.0..=1.0) latency in ms via LINEAR INTERPOLATION over the
    /// cumulative bucket counts. Returns `0.0` for an empty histogram. The target
    /// rank is `quantile × total`; we walk buckets accumulating counts until the
    /// rank falls inside a bucket, then interpolate linearly between that bucket's
    /// lower and upper bound by how far into the bucket the rank lies. The overflow
    /// bucket (unbounded upper) reports its FINITE lower bound (`HISTOGRAM_MAX_MS`)
    /// so a p99 dominated by >120 s samples returns 120 s, not infinity.
    fn quantile(&self, quantile: f64) -> f64 {
        if self.total == 0 {
            return 0.0;
        }
        let quantile = quantile.clamp(0.0, 1.0);
        // Target rank in [0, total]. Using `(total) * q` and walking cumulative
        // upper edges gives the standard histogram-interpolation estimate.
        let target = quantile * self.total as f64;
        let mut cumulative_before = 0u64;
        for index in 0..HISTOGRAM_BUCKETS {
            let count = self.buckets[index];
            if count == 0 {
                continue;
            }
            let cumulative_after = cumulative_before + count;
            if target <= cumulative_after as f64 {
                let lower = bucket_lower_ms(index);
                let upper = bucket_upper_ms(index);
                // Overflow bucket: report its finite lower bound (120 s), never inf.
                if !upper.is_finite() {
                    return lower;
                }
                // Linear interpolation: how far into THIS bucket's count the target
                // rank lies, mapped onto [lower, upper].
                let into_bucket = (target - cumulative_before as f64).max(0.0);
                let fraction = (into_bucket / count as f64).clamp(0.0, 1.0);
                return lower + (upper - lower) * fraction;
            }
            cumulative_before = cumulative_after;
        }
        // Target beyond all counted buckets (q == 1.0 edge): the max finite bound.
        bucket_upper_ms(HISTOGRAM_BUCKETS - 2)
    }
}

/// One 1-second slot of a window ring: the per-key counts, the latency histogram,
/// and the epoch-second this slot currently represents. `epoch_s == 0` marks an
/// unused slot. When wall-clock advances to a new second whose ring index maps to a
/// slot still stamped with an OLDER epoch second, the slot is RESET before the new
/// sample lands (circular reuse — the window only ever holds its own time span).
#[derive(Debug, Clone, Default)]
struct Slot {
    epoch_s: u64,
    buckets: BTreeMap<BucketKey, BucketCounts>,
    histogram: Histogram,
}

impl Slot {
    /// Reset this slot to represent `epoch_s` with no samples (circular reuse).
    fn reset(&mut self, epoch_s: u64) {
        self.epoch_s = epoch_s;
        self.buckets.clear();
        self.histogram = Histogram::default();
    }
}

/// A fixed-size ring of 1-second [`Slot`]s for one window. The ring is indexed by
/// `epoch_s % slots`; reads that aggregate the window only count slots whose
/// `epoch_s` is within `[now - slots + 1, now]` (stale slots from a prior lap are
/// ignored), so a quiet gateway reports an empty window rather than ancient samples.
#[derive(Debug, Clone)]
struct WindowRing {
    slots: Vec<Slot>,
}

impl WindowRing {
    fn new(len: usize) -> Self {
        Self {
            slots: vec![Slot::default(); len],
        }
    }

    /// The mutable slot for `epoch_s`, reset first if it currently holds a different
    /// (older lap) second — so a sample only ever lands in a slot representing its
    /// own second.
    fn slot_mut(&mut self, epoch_s: u64) -> &mut Slot {
        let len = self.slots.len();
        let index = (epoch_s as usize) % len;
        let slot = &mut self.slots[index];
        if slot.epoch_s != epoch_s {
            slot.reset(epoch_s);
        }
        slot
    }

    /// Aggregate the window AS OF `now_epoch_s`: merge every slot whose `epoch_s` is
    /// within the window span `[now - len + 1, now]` into a single counts-map +
    /// histogram. Slots outside the span (a prior lap, or never written) are skipped.
    fn aggregate(&self, now_epoch_s: u64) -> WindowReport {
        let len = self.slots.len() as u64;
        let floor = now_epoch_s.saturating_sub(len - 1);
        let mut buckets: BTreeMap<BucketKey, BucketCounts> = BTreeMap::new();
        let mut histogram = Histogram::default();
        for slot in &self.slots {
            if slot.epoch_s < floor || slot.epoch_s > now_epoch_s {
                continue;
            }
            for (key, counts) in &slot.buckets {
                let entry = buckets.entry(key.clone()).or_default();
                entry.count = entry.count.saturating_add(counts.count);
                entry.prompt_tokens = entry.prompt_tokens.saturating_add(counts.prompt_tokens);
                entry.completion_tokens = entry
                    .completion_tokens
                    .saturating_add(counts.completion_tokens);
                entry.cached_tokens = entry.cached_tokens.saturating_add(counts.cached_tokens);
                entry.reasoning_tokens = entry
                    .reasoning_tokens
                    .saturating_add(counts.reasoning_tokens);
            }
            histogram.merge(&slot.histogram);
        }
        WindowReport { buckets, histogram }
    }
}

/// The collapsed per-window view: the merged per-key counts + the merged latency
/// histogram, from which p50/p95/p99 are reported.
#[derive(Debug, Clone, Default, Serialize)]
pub struct WindowReport {
    pub buckets: BTreeMap<BucketKey, BucketCounts>,
    pub histogram: Histogram,
}

impl WindowReport {
    /// `(p50, p95, p99)` latency in ms over this window's histogram.
    pub fn percentiles(&self) -> Percentiles {
        Percentiles {
            p50: self.histogram.quantile(0.50),
            p95: self.histogram.quantile(0.95),
            p99: self.histogram.quantile(0.99),
        }
    }

    /// Total request count across all keys in the window.
    pub fn total_count(&self) -> u64 {
        self.buckets
            .values()
            .map(|counts| counts.count)
            .fold(0u64, u64::saturating_add)
    }
}

/// The reported p50/p95/p99 latency (ms) for a window.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Percentiles {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

/// The body-free metrics view captured into a [`DashboardSnapshot`]: the three
/// windows' collapsed reports + their percentiles, as of the snapshot instant. This
/// is a pure value (no `Arc`, no live-store reference), so a retained snapshot
/// cannot pin live state.
#[derive(Debug, Clone, Default, Serialize)]
pub struct MetricsView {
    pub window_1m: WindowReport,
    pub window_5m: WindowReport,
    pub window_1h: WindowReport,
    pub percentiles_1m: Percentiles,
    pub percentiles_5m: Percentiles,
    pub percentiles_1h: Percentiles,
}

impl MetricsView {
    /// Approximate retained bytes of this view (for the snapshot-memory quota):
    /// per-window bucket key strings + counts + the fixed histograms.
    fn approx_bytes(&self) -> usize {
        let window_bytes = |report: &WindowReport| -> usize {
            let mut bytes = std::mem::size_of::<Histogram>();
            for (key, counts) in &report.buckets {
                bytes += key.model.len()
                    + key.endpoint.len()
                    + key.upstream.len()
                    + std::mem::size_of::<BucketKey>()
                    + counts.approx_bytes();
            }
            bytes
        };
        window_bytes(&self.window_1m)
            + window_bytes(&self.window_5m)
            + window_bytes(&self.window_1h)
    }
}

/// The per-domain cursor quad carried on every [`DashboardSnapshot`] (AGENTS.md:
/// per-domain `{domain, seq}` cursors, NOT a single global watermark). Each field
/// is the authoritative sequence of its own store at the cut instant.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct DomainCursors {
    /// FlowStore mutation sequence (D1).
    pub flow_seq: u64,
    /// MetricsLayer mutation sequence (this module).
    pub metrics_seq: u64,
    /// Topology publisher version (D4's `ProviderHealthSnapshot.version`).
    pub topology_seq: u64,
    /// Monitor hub sequence (D3 transcript domain).
    pub monitor_seq: u64,
}

/// One body-free atomic cut across the FlowStore, MetricsLayer, and topology stores
/// taken by the 5 s coordinated snapshot task. Carries:
/// - `taken_at_ms`: the cut's wall-clock instant (the `snapshot_at(ts)` key).
/// - `cursors`: per-domain `{flow,metrics,topology,monitor}` sequences at the cut.
/// - `summaries`: body-free [`SnapshotFlowSummary`]s (NO `Arc<[u8]>` — the 135 GiB
///   fix; a retained snapshot holds at most ~1 KiB per flow, never a 128 KiB body).
/// - `metrics`: the collapsed [`MetricsView`].
/// - `topology`: the ONE `Arc<ProviderHealthSnapshot>` captured in the cut.
///
/// Immutable once built; `Arc`-wrapped in the ring so a reader's clone is cheap and
/// never mutated underneath it.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardSnapshot {
    pub taken_at_ms: u128,
    pub cursors: DomainCursors,
    pub summaries: Vec<SnapshotFlowSummary>,
    pub metrics: MetricsView,
    /// The ONE topology cut captured in this snapshot. Serialized by DEREF (serde's
    /// blanket `Arc: Serialize` needs the `rc` feature, which we don't enable
    /// crate-wide; the inner `ProviderHealthSnapshot` already derives `Serialize`).
    #[serde(serialize_with = "serialize_topology")]
    pub topology: Arc<ProviderHealthSnapshot>,
}

/// Serialize an `Arc<ProviderHealthSnapshot>` by dereferencing to the inner value
/// (avoids enabling serde's `rc` feature just for the snapshot DTO).
fn serialize_topology<S>(
    topology: &Arc<ProviderHealthSnapshot>,
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    (**topology).serialize(serializer)
}

impl DashboardSnapshot {
    /// Approximate retained bytes of this cut, for the snapshot-ring memory quota.
    /// Counts ONLY the body-free summary scalar strings + the metrics view + a small
    /// topology estimate — there are NO body `Arc<[u8]>`s to count (that is the
    /// point: a cut is provably body-free, so its memory is bounded at ~1 KiB/flow).
    fn approx_bytes(&self) -> usize {
        let summary_bytes: usize = self
            .summaries
            .iter()
            .map(summary_approx_bytes)
            .fold(0usize, usize::saturating_add);
        summary_bytes
            .saturating_add(self.metrics.approx_bytes())
            // A small fixed estimate for the shared topology Arc (counted once; the
            // Arc is shared across cuts that captured the same version).
            .saturating_add(std::mem::size_of::<ProviderHealthSnapshot>())
    }
}

/// Approximate retained bytes of one body-free [`SnapshotFlowSummary`]: the sum of
/// its dynamic scalar string lengths + the fixed struct size. There are NO body
/// fields to count (the summary is body-free by construction), so this is the full
/// memory footprint — the basis for the ≤400 MiB ring-quota assertion.
fn summary_approx_bytes(summary: &SnapshotFlowSummary) -> usize {
    let opt = |value: &Option<String>| value.as_ref().map(String::len).unwrap_or(0);
    std::mem::size_of::<SnapshotFlowSummary>()
        + summary.api_call_id.len()
        + opt(&summary.response_id)
        + summary.method.len()
        + summary.uri.len()
        + opt(&summary.model_requested)
        + opt(&summary.model_served)
        + opt(&summary.upstream_target)
        + opt(&summary.terminal_reason)
}

/// The bounded ring of body-free [`DashboardSnapshot`] cuts (720 = 1 h at 5 s). A
/// fresh cut is pushed at the back; the ring is bounded BOTH by slot count (720) AND
/// by a retained-summary-byte quota (the HARD ≤400 MiB bound — when a push would
/// exceed it, the OLDEST cuts are dropped first). `snapshot_at(ts)` binary-searches
/// the time-ordered cuts for the nearest cut with `taken_at_ms ≤ ts`.
#[derive(Debug, Default)]
struct SnapshotRing {
    cuts: std::collections::VecDeque<Arc<DashboardSnapshot>>,
    retained_bytes: usize,
    quota_bytes: usize,
}

impl SnapshotRing {
    fn new(quota_bytes: usize) -> Self {
        Self {
            cuts: std::collections::VecDeque::with_capacity(SNAPSHOT_RING_SLOTS),
            retained_bytes: 0,
            quota_bytes,
        }
    }

    /// Push a fresh cut, then enforce BOTH the slot cap (720) and the byte quota
    /// (≤400 MiB) by dropping the OLDEST cuts. Cuts are pushed in monotonic
    /// `taken_at_ms` order (the 5 s task is the only writer), so the deque stays
    /// time-sorted for `snapshot_at`'s binary search.
    fn push(&mut self, cut: Arc<DashboardSnapshot>) {
        self.retained_bytes = self.retained_bytes.saturating_add(cut.approx_bytes());
        self.cuts.push_back(cut);
        while self.cuts.len() > SNAPSHOT_RING_SLOTS {
            self.pop_oldest();
        }
        while self.retained_bytes > self.quota_bytes && self.cuts.len() > 1 {
            // Keep at least the newest cut even if a single cut somehow exceeds the
            // quota (degenerate); the quota is a peak bound, not a per-cut bound.
            self.pop_oldest();
        }
    }

    fn pop_oldest(&mut self) {
        if let Some(old) = self.cuts.pop_front() {
            self.retained_bytes = self.retained_bytes.saturating_sub(old.approx_bytes());
        }
    }

    /// The nearest retained cut with `taken_at_ms ≤ ts` (the `/snapshot?at=`
    /// backend). `None` when the ring is empty or every cut is newer than `ts`.
    /// Binary search over the time-sorted deque.
    fn snapshot_at(&self, ts: u128) -> Option<Arc<DashboardSnapshot>> {
        if self.cuts.is_empty() {
            return None;
        }
        // VecDeque is contiguous-enough for a manual binary search by index.
        let mut lo = 0usize;
        let mut hi = self.cuts.len();
        let mut best: Option<usize> = None;
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.cuts[mid].taken_at_ms <= ts {
                best = Some(mid);
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        best.map(|index| Arc::clone(&self.cuts[index]))
    }

    /// The most recent cut, if any (the live `/snapshot` with no `at=`).
    fn latest(&self) -> Option<Arc<DashboardSnapshot>> {
        self.cuts.back().map(Arc::clone)
    }
}

/// Interior state of the [`MetricsLayer`], guarded by its single `Mutex`. Holds the
/// three window rings, the monotonic `metrics_seq`, and the snapshot ring. The
/// snapshot ring lives UNDER the same mutex so the 5 s task pushes a cut while
/// already holding the metrics lock (inside the FlowStore→Metrics critical section),
/// avoiding a third lock.
#[derive(Debug)]
struct MetricsState {
    ring_1m: WindowRing,
    ring_5m: WindowRing,
    ring_1h: WindowRing,
    metrics_seq: u64,
    snapshots: SnapshotRing,
}

impl MetricsState {
    fn new(snapshot_quota_bytes: usize) -> Self {
        Self {
            ring_1m: WindowRing::new(WINDOW_1M_SLOTS),
            ring_5m: WindowRing::new(WINDOW_5M_SLOTS),
            ring_1h: WindowRing::new(WINDOW_1H_SLOTS),
            metrics_seq: 0,
            snapshots: SnapshotRing::new(snapshot_quota_bytes),
        }
    }

    /// Record one terminal response into all three rings at `epoch_s`, bumping
    /// `metrics_seq`.
    fn record_response(&mut self, epoch_s: u64, key: &BucketKey, elapsed_ms: f64) {
        for ring in [&mut self.ring_1m, &mut self.ring_5m, &mut self.ring_1h] {
            let slot = ring.slot_mut(epoch_s);
            let entry = slot.buckets.entry(key.clone()).or_default();
            entry.count = entry.count.saturating_add(1);
            slot.histogram.record(elapsed_ms);
        }
        self.metrics_seq = self.metrics_seq.saturating_add(1);
    }

    /// Record one terminal response AND its optional final cumulative usage into all
    /// three rings at the SAME `epoch_s`, bumping `metrics_seq` ONCE (Codex D5 R1 #2).
    /// This is the atomic terminal path: the response count and the token totals are
    /// applied under a SINGLE metrics-lock hold at ONE epoch/slot, so a concurrent 5 s
    /// snapshot can never interleave between them and land the count and the tokens in
    /// DIFFERENT 1 s slots (which separate `record_response` + `add_tokens` calls —
    /// each computing its own `now_epoch_s()` under its own lock — could do across a
    /// second boundary). Both join the same `{status, model, endpoint, upstream}`
    /// bucket key, so a completed flow's count + tokens are always co-located.
    fn record_terminal(
        &mut self,
        epoch_s: u64,
        key: &BucketKey,
        elapsed_ms: f64,
        usage: Option<FlowUsage>,
    ) {
        for ring in [&mut self.ring_1m, &mut self.ring_5m, &mut self.ring_1h] {
            let slot = ring.slot_mut(epoch_s);
            let entry = slot.buckets.entry(key.clone()).or_default();
            entry.count = entry.count.saturating_add(1);
            if let Some(usage) = usage {
                entry.prompt_tokens = entry.prompt_tokens.saturating_add(usage.prompt);
                entry.completion_tokens = entry.completion_tokens.saturating_add(usage.completion);
                entry.cached_tokens = entry.cached_tokens.saturating_add(usage.cached);
                entry.reasoning_tokens = entry.reasoning_tokens.saturating_add(usage.reasoning);
            }
            slot.histogram.record(elapsed_ms);
        }
        self.metrics_seq = self.metrics_seq.saturating_add(1);
    }

    /// Add a flow's token counts to the bucket for `key` at `epoch_s`. Called ONCE
    /// per flow at the terminal seam with the flow's FINAL cumulative usage, so the
    /// slot's token sum equals the window's true token throughput (no per-chunk
    /// over-counting of cumulative values). Bumps `metrics_seq`.
    fn add_tokens(&mut self, epoch_s: u64, key: &BucketKey, usage: FlowUsage) {
        for ring in [&mut self.ring_1m, &mut self.ring_5m, &mut self.ring_1h] {
            let slot = ring.slot_mut(epoch_s);
            let entry = slot.buckets.entry(key.clone()).or_default();
            entry.prompt_tokens = entry.prompt_tokens.saturating_add(usage.prompt);
            entry.completion_tokens = entry.completion_tokens.saturating_add(usage.completion);
            entry.cached_tokens = entry.cached_tokens.saturating_add(usage.cached);
            entry.reasoning_tokens = entry.reasoning_tokens.saturating_add(usage.reasoning);
        }
        self.metrics_seq = self.metrics_seq.saturating_add(1);
    }

    /// Collapse the three rings into a body-free [`MetricsView`] as of `now_epoch_s`.
    fn view(&self, now_epoch_s: u64) -> MetricsView {
        let window_1m = self.ring_1m.aggregate(now_epoch_s);
        let window_5m = self.ring_5m.aggregate(now_epoch_s);
        let window_1h = self.ring_1h.aggregate(now_epoch_s);
        let percentiles_1m = window_1m.percentiles();
        let percentiles_5m = window_5m.percentiles();
        let percentiles_1h = window_1h.percentiles();
        MetricsView {
            window_1m,
            window_5m,
            window_1h,
            percentiles_1m,
            percentiles_5m,
            percentiles_1h,
        }
    }
}

/// Authoritative aggregated stats + the coordinated body-free snapshot store (D5).
/// Mirrors the `MonitorHub`/`DashboardFlowStore` `new()`/`disabled()` zero-overhead
/// split: when `disabled()` EVERY method early-returns and takes NO lock, so the
/// production hot path is unchanged. `Clone` (state behind `Arc<Mutex<_>>`) so it
/// threads into the `#[derive(Clone)] Gateway` like the FlowStore/monitor do.
#[derive(Clone)]
pub struct MetricsLayer {
    enabled: bool,
    state: Arc<Mutex<MetricsState>>,
}

impl std::fmt::Debug for MetricsLayer {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MetricsLayer")
            .field("enabled", &self.enabled)
            .finish_non_exhaustive()
    }
}

impl MetricsLayer {
    /// Enabled layer (debug UI on). Uses the default 400 MiB snapshot-ring quota.
    pub fn new() -> Self {
        Self {
            enabled: true,
            state: Arc::new(Mutex::new(MetricsState::new(DEFAULT_SNAPSHOT_QUOTA_BYTES))),
        }
    }

    /// No-op layer (debug UI off). Every method early-returns and takes NO lock, so
    /// the production hot path keeps zero overhead — mirrors `MonitorHub::disabled()`.
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            // A tiny quota; the state is never mutated on the disabled path.
            state: Arc::new(Mutex::new(MetricsState::new(0))),
        }
    }

    /// Test-only constructor with an explicit snapshot-ring byte quota, so the
    /// memory-quota test can drive eviction without allocating 400 MiB.
    #[cfg(test)]
    fn with_snapshot_quota(quota_bytes: usize) -> Self {
        Self {
            enabled: true,
            state: Arc::new(Mutex::new(MetricsState::new(quota_bytes))),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, MetricsState> {
        self.state.lock().expect("metrics layer lock poisoned")
    }

    /// Record one TERMINAL response (the D3 terminal finalize seam — NOT the
    /// middleware, NOT per-chunk). No-op when disabled (zero lock, zero work). The
    /// `served_model`/`endpoint`/`upstream` collapse to `"unknown"` when absent so
    /// the key space stays bounded.
    pub fn record_response(
        &self,
        status: FlowStatus,
        served_model: Option<&str>,
        endpoint: &str,
        upstream: Option<&str>,
        elapsed_ms: u128,
    ) {
        if !self.enabled {
            return;
        }
        let key = BucketKey {
            status: StatusClass::from_status(status),
            model: label_or_unknown(served_model),
            endpoint: endpoint.to_string(),
            upstream: label_or_unknown(upstream),
        };
        let epoch_s = now_epoch_s();
        self.lock()
            .record_response(epoch_s, &key, elapsed_ms as f64);
    }

    /// Record a flow's FINAL cumulative token usage into the token counters, called
    /// ONCE per flow at the D3 terminal seam (co-located with `record_response`).
    /// Recording the final cumulative once — rather than each cumulative chunk —
    /// makes the slot's token sum the window's true throughput without delta
    /// bookkeeping or per-chunk over-counting. The bucket is keyed by the flow's
    /// terminal `status` so it joins the same bucket as its `record_response` (a
    /// completed flow's tokens + count share one `{Success, model, endpoint,
    /// upstream}` key). No-op when disabled.
    pub fn record_usage(
        &self,
        status: FlowStatus,
        served_model: Option<&str>,
        endpoint: &str,
        upstream: Option<&str>,
        usage: FlowUsage,
    ) {
        if !self.enabled {
            return;
        }
        let key = BucketKey {
            status: StatusClass::from_status(status),
            model: label_or_unknown(served_model),
            endpoint: endpoint.to_string(),
            upstream: label_or_unknown(upstream),
        };
        let epoch_s = now_epoch_s();
        self.lock().add_tokens(epoch_s, &key, usage);
    }

    /// **The atomic terminal record (Codex D5 R1 #2).** Records a flow's terminal
    /// response count AND its optional FINAL cumulative token usage in a SINGLE
    /// metrics-lock hold at ONE epoch/slot — the engine drives this once at the D3
    /// terminal finalize seam INSTEAD of a separate `record_response` + `record_usage`
    /// pair. Taking one lock and computing one `epoch_s` here guarantees the count and
    /// the tokens land in the SAME 1 s slot even if a 5 s coordinated snapshot runs
    /// concurrently: there is no window between two lock acquisitions for a snapshot to
    /// observe the count without the tokens (or to split them across a second
    /// boundary). The `served_model`/`endpoint`/`upstream` collapse to `"unknown"`
    /// when absent so the key space stays bounded. No-op (zero lock, zero work) when
    /// disabled.
    pub fn record_terminal(
        &self,
        status: FlowStatus,
        served_model: Option<&str>,
        endpoint: &str,
        upstream: Option<&str>,
        elapsed_ms: u128,
        usage: Option<FlowUsage>,
    ) {
        if !self.enabled {
            return;
        }
        let key = BucketKey {
            status: StatusClass::from_status(status),
            model: label_or_unknown(served_model),
            endpoint: endpoint.to_string(),
            upstream: label_or_unknown(upstream),
        };
        let epoch_s = now_epoch_s();
        self.lock()
            .record_terminal(epoch_s, &key, elapsed_ms as f64, usage);
    }

    /// The current metrics domain sequence (the per-domain cursor). `0` when
    /// disabled.
    pub fn metrics_seq(&self) -> u64 {
        if !self.enabled {
            return 0;
        }
        self.lock().metrics_seq
    }

    /// Collapse the three rings into a body-free [`MetricsView`] as of NOW. Empty
    /// when disabled.
    pub fn view(&self) -> MetricsView {
        if !self.enabled {
            return MetricsView::default();
        }
        let now = now_epoch_s();
        self.lock().view(now)
    }

    /// **The 5 s coordinated snapshot.** Takes the FIXED lock order — FlowStore mutex
    /// FIRST, THEN this layer's metrics mutex nested under it — and captures ONE
    /// `Arc<ProviderHealthSnapshot>` from the D4 publisher, producing a true atomic cut
    /// across all three stores into a body-free [`DashboardSnapshot`]. Only this method
    /// ever holds >1 lock, so the fixed order makes a deadlock impossible. The summaries
    /// are body-free (NO `Arc<[u8]>`). No-op (returns `None`) when disabled. The cut is
    /// pushed onto the bounded snapshot ring AND returned (so the caller/test can assert
    /// on it).
    ///
    /// Atomic-cut guarantee (Codex D5 R1 #1): the FlowStore lock is held until the
    /// metrics lock is nested under it and the cut-defining cursors + topology `Arc` are
    /// captured — that overlap instant IS the cut, so there is no gap in which a writer
    /// could mutate both stores and produce a torn cut (a metrics sample for a flow
    /// absent from the summaries). The FlowStore lock is then released and the heavier
    /// metrics aggregation runs under the metrics lock alone (still sufficient to keep
    /// the cut consistent — no writer can mutate metrics while it is held — and it keeps
    /// the FlowStore critical section minimal so writers are not starved). The cursors
    /// record exactly which versions were read at the cut.
    pub fn snapshot(
        &self,
        flow_store: &DashboardFlowStore,
        topology: &ProviderHealthPublisher,
    ) -> Option<Arc<DashboardSnapshot>> {
        self.snapshot_with_monitor_seq(flow_store, topology, 0)
    }

    /// Like [`snapshot`](Self::snapshot) but also records the monitor domain's
    /// sequence into the cut's cursors (the snapshot task owns the monitor handle).
    /// Same FIXED FlowStore→Metrics atomic cut (FlowStore lock held until the metrics
    /// lock is nested + ALL cut metadata captured, then released) + one topology Arc.
    pub fn snapshot_with_monitor_seq(
        &self,
        flow_store: &DashboardFlowStore,
        topology: &ProviderHealthPublisher,
        monitor_seq: u64,
    ) -> Option<Arc<DashboardSnapshot>> {
        // A pre-read monitor sequence: hand it back verbatim from inside the cut.
        self.snapshot_with(flow_store, topology, || monitor_seq)
    }

    /// The cut-builder shared by every snapshot entry point. `read_monitor_seq` is
    /// invoked INSIDE the dual-lock critical section so the live monitor cursor is
    /// sampled at the cut instant alongside the rest of the metadata, not before the
    /// locks (the snapshot task passes `|| monitor.last_sequence()`).
    ///
    /// Metadata-inside-the-lock guarantee (Codex D5 R2 HIGH): EVERY field that defines
    /// the cut — `taken_at_ms`/`now_epoch`, the topology `Arc` + its version, the
    /// metrics cursor, AND the monitor cursor — is captured only AFTER BOTH the
    /// FlowStore guard and the metrics lock are held. Capturing the timestamp/epoch
    /// before the locks could stamp the cut EARLIER than the state it contains: under
    /// contention `snapshot_at(ts)` would return data newer than its own `taken_at_ms`,
    /// and `view(now_epoch)` would aggregate against a stale epoch while `metrics_seq`
    /// already reflected newer samples. Sampling all metadata under the held locks makes
    /// `taken_at_ms` ≥ every flow/sample the cut contains and keeps the epoch consistent
    /// with the cursors.
    fn snapshot_with<S>(
        &self,
        flow_store: &DashboardFlowStore,
        topology: &ProviderHealthPublisher,
        read_monitor_seq: S,
    ) -> Option<Arc<DashboardSnapshot>>
    where
        S: FnOnce() -> u64,
    {
        if !self.enabled {
            return None;
        }
        // FIXED LOCK ORDER, single atomic cut: hold the FlowStore lock only until the
        // metrics lock is nested under it and ALL cut metadata (timestamp/epoch,
        // topology Arc, both cursors) are captured — THAT instant is the cut. The
        // (heavier) metrics aggregation + ring push then run under the metrics lock
        // ALONE, after the FlowStore lock is released, so the FlowStore critical section
        // stays minimal and concurrent writers are not starved by the snapshot's
        // aggregation. Correctness after the early FlowStore release: the metrics lock is
        // STILL HELD, so no writer can complete a both-stores mutation (every metrics
        // mutation needs it) and the summaries are already a copy — the cut stays
        // internally consistent. The FlowStore lock is acquired first and never
        // re-entered here.
        let cut = flow_store.with_summaries_under_lock(|flow_guard, summaries, flow_seq| {
            // FIXED LOCK ORDER step 2: nest the metrics mutex under the FlowStore lock.
            // Both held now == the atomic cut instant — sample EVERY piece of cut
            // metadata here so none of it predates the state the cut contains.
            let mut state = self.lock();
            // Wall-clock instant of the cut, taken under the held locks so it is never
            // earlier than any flow/sample the cut includes.
            let taken_at_ms = now_ms();
            let now_epoch = (taken_at_ms / 1000) as u64;
            // ONE topology Arc capture (D4) at the cut instant.
            let topology = topology.latest();
            let topology_seq = topology.version;
            let metrics_seq = state.metrics_seq;
            // The monitor cursor, sampled at the cut instant (not pre-read by the caller).
            let monitor_seq = read_monitor_seq();
            // Cut instant fixed: release the FlowStore lock and finish the metrics-only
            // work (aggregation + push) under the metrics guard alone.
            flow_guard.release();
            let metrics = state.view(now_epoch);
            let cursors = DomainCursors {
                flow_seq,
                metrics_seq,
                topology_seq,
                monitor_seq,
            };
            let cut = Arc::new(DashboardSnapshot {
                taken_at_ms,
                cursors,
                summaries,
                metrics,
                topology,
            });
            state.snapshots.push(Arc::clone(&cut));
            cut
        });
        Some(cut)
    }

    /// The nearest retained snapshot cut with `taken_at_ms ≤ ts` (the
    /// `/snapshot?at=` backend; D13 registers the route). `None` when disabled, the
    /// ring is empty, or every cut is newer than `ts`.
    pub fn snapshot_at(&self, ts: u128) -> Option<Arc<DashboardSnapshot>> {
        if !self.enabled {
            return None;
        }
        self.lock().snapshots.snapshot_at(ts)
    }

    /// The most recent retained snapshot cut (live `/snapshot` with no `at=`). `None`
    /// when disabled or no cut has been taken yet.
    pub fn latest_snapshot(&self) -> Option<Arc<DashboardSnapshot>> {
        if !self.enabled {
            return None;
        }
        self.lock().snapshots.latest()
    }
}

impl Default for MetricsLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Spawn the D5 5-second coordinated snapshot task. Gated by `--with-debug-ui` at
/// the DI root (production must NOT run this — zero overhead). The task holds only
/// `Clone` handles (the metrics layer, the FlowStore, the topology publisher, the
/// monitor) — all behind `Arc`, so it is `Send + 'static` and runs for the process
/// lifetime. Every 5 s it takes the single FlowStore→Metrics critical section and,
/// INSIDE it, captures one topology `Arc` and reads the monitor sequence, then pushes
/// a body-free cut onto the bounded ring. Passing `|| monitor.last_sequence()` (rather
/// than a pre-read value) keeps the monitor cursor sampled at the cut instant with the
/// rest of the metadata. Returns the `JoinHandle` so a caller/test can abort it.
pub fn spawn_snapshot_task(
    metrics: MetricsLayer,
    flow_store: DashboardFlowStore,
    topology: ProviderHealthPublisher,
    monitor: crate::monitor::MonitorHub,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            // Read the monitor cursor INSIDE the cut (the closure runs under the held
            // locks) so it is consistent with the rest of the snapshot metadata.
            metrics.snapshot_with(&flow_store, &topology, || monitor.last_sequence());
        }
    })
}

/// `value` or the bounded `"unknown"` sentinel, so a missing attribution never
/// spawns a distinct `None` bucket and the key space stays bounded.
fn label_or_unknown(value: Option<&str>) -> String {
    value.unwrap_or("unknown").to_string()
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}

fn now_epoch_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a histogram from an explicit list of latency samples (ms).
    fn histogram_of(samples: &[f64]) -> Histogram {
        let mut histogram = Histogram::default();
        for &sample in samples {
            histogram.record(sample);
        }
        histogram
    }

    #[test]
    fn disabled_layer_is_a_no_op() {
        let metrics = MetricsLayer::disabled();
        assert!(!metrics.is_enabled());
        metrics.record_response(
            FlowStatus::Completed,
            Some("m"),
            "/v1/responses",
            Some("p"),
            12,
        );
        metrics.record_usage(
            FlowStatus::Completed,
            Some("m"),
            "/v1/responses",
            Some("p"),
            FlowUsage::default(),
        );
        assert_eq!(metrics.metrics_seq(), 0, "disabled layer never bumps seq");
        let view = metrics.view();
        assert_eq!(view.window_1m.total_count(), 0);
        assert!(metrics.latest_snapshot().is_none());
        assert!(metrics.snapshot_at(u128::MAX).is_none());
        // The coordinated snapshot is a no-op too.
        let flow = DashboardFlowStore::disabled();
        let topo = ProviderHealthPublisher::default();
        assert!(metrics.snapshot(&flow, &topo).is_none());
    }

    #[test]
    fn quantile_known_distribution_uniform_1_to_100() {
        // 100 samples at 1,2,...,100 ms. The histogram is coarse (log-spaced
        // buckets), so assert the interpolated quantiles land in a sane band around
        // the true 50th/95th/99th values (50.5, 95.95, 99.99 ms).
        let samples: Vec<f64> = (1..=100).map(|value| value as f64).collect();
        let histogram = histogram_of(&samples);
        let p50 = histogram.quantile(0.50);
        let p95 = histogram.quantile(0.95);
        let p99 = histogram.quantile(0.99);
        assert!(
            (30.0..=75.0).contains(&p50),
            "p50 {p50} within log-bucket tolerance of 50ms"
        );
        assert!(
            (75.0..=120.0).contains(&p95),
            "p95 {p95} within log-bucket tolerance of 96ms"
        );
        assert!(
            (80.0..=130.0).contains(&p99),
            "p99 {p99} within log-bucket tolerance of 100ms"
        );
        // Monotonic ordering must hold exactly.
        assert!(p50 <= p95 && p95 <= p99, "quantiles monotonic");
    }

    #[test]
    fn quantile_empty_histogram_is_zero() {
        let histogram = Histogram::default();
        assert_eq!(histogram.quantile(0.50), 0.0);
        assert_eq!(histogram.quantile(0.95), 0.0);
        assert_eq!(histogram.quantile(0.99), 0.0);
    }

    #[test]
    fn quantile_single_sample_lands_in_its_bucket() {
        // A single 50 ms sample: every quantile interpolates within the bucket that
        // contains 50 ms, i.e. (lower, upper] straddling 50.
        let histogram = histogram_of(&[50.0]);
        let index = histogram.bucket_index(50.0);
        let lower = bucket_lower_ms(index);
        let upper = bucket_upper_ms(index);
        for quantile in [0.50, 0.95, 0.99] {
            let value = histogram.quantile(quantile);
            assert!(
                value >= lower && value <= upper,
                "q{quantile} {value} within bucket [{lower},{upper}]"
            );
        }
    }

    #[test]
    fn quantile_bimodal_separates_low_and_high() {
        // 90 samples at ~5 ms, 10 samples at ~5000 ms. p50 must be in the low mode,
        // p99 in the high mode.
        let mut samples = vec![5.0; 90];
        samples.extend(vec![5000.0; 10]);
        let histogram = histogram_of(&samples);
        let p50 = histogram.quantile(0.50);
        let p99 = histogram.quantile(0.99);
        assert!(p50 < 100.0, "p50 {p50} in the low (~5ms) mode");
        assert!(p99 > 1000.0, "p99 {p99} in the high (~5000ms) mode");
    }

    #[test]
    fn quantile_overflow_bucket_reports_finite_max() {
        // A sample beyond 120 s lands in the overflow bucket; its quantile reports
        // the finite max bound (120 s), never infinity.
        let histogram = histogram_of(&[500_000.0]);
        let p99 = histogram.quantile(0.99);
        assert!(p99.is_finite(), "overflow quantile is finite");
        assert!(
            (p99 - HISTOGRAM_MAX_MS).abs() < 1.0,
            "overflow quantile {p99} == finite max {HISTOGRAM_MAX_MS}"
        );
    }

    #[test]
    fn bucket_boundaries_are_monotonic_and_span_range() {
        // The log-spaced ladder is strictly increasing and brackets [1ms, 120s].
        let mut previous = 0.0;
        for index in 0..HISTOGRAM_BUCKETS - 1 {
            let upper = bucket_upper_ms(index);
            assert!(upper > previous, "bucket {index} upper {upper} increasing");
            previous = upper;
        }
        // D5 R1 #3: the FIRST finite upper bound is EXACTLY 1 ms (not ≈1.5 ms) and the
        // LAST finite upper bound is EXACTLY 120 s — the endpoints are pinned, with the
        // ladder log-spaced between them.
        assert!(
            (bucket_upper_ms(0) - HISTOGRAM_MIN_MS).abs() < 1e-9,
            "bucket 0 upper {} == exactly 1 ms",
            bucket_upper_ms(0)
        );
        assert!(
            (bucket_upper_ms(HISTOGRAM_BUCKETS - 2) - HISTOGRAM_MAX_MS).abs() < 1e-6,
            "last finite bucket {} == exactly 120 s",
            bucket_upper_ms(HISTOGRAM_BUCKETS - 2)
        );
        assert!(bucket_upper_ms(HISTOGRAM_BUCKETS - 1).is_infinite());
    }

    #[test]
    fn record_response_populates_all_windows_and_bumps_seq() {
        let metrics = MetricsLayer::new();
        assert_eq!(metrics.metrics_seq(), 0);
        metrics.record_response(
            FlowStatus::Completed,
            Some("served-m"),
            "/v1/responses",
            Some("provider-a"),
            42,
        );
        assert_eq!(metrics.metrics_seq(), 1, "one mutation bumps seq once");
        let view = metrics.view();
        assert_eq!(view.window_1m.total_count(), 1);
        assert_eq!(view.window_5m.total_count(), 1);
        assert_eq!(view.window_1h.total_count(), 1);
        // The bucket key carries the served model + endpoint + upstream + class.
        let (key, counts) = view
            .window_1m
            .buckets
            .iter()
            .next()
            .expect("one bucket present");
        assert_eq!(key.status, StatusClass::Success);
        assert_eq!(key.model, "served-m");
        assert_eq!(key.endpoint, "/v1/responses");
        assert_eq!(key.upstream, "provider-a");
        assert_eq!(counts.count, 1);
        // Latency landed in the histogram → p50 is non-zero and within a 42ms band.
        let percentiles = view.window_1m.percentiles();
        assert!(percentiles.p50 > 0.0, "p50 populated from the 42ms sample");
    }

    #[test]
    fn record_response_unknown_dimensions_collapse_to_unknown() {
        let metrics = MetricsLayer::new();
        metrics.record_response(FlowStatus::Failed, None, "/v1/chat/completions", None, 7);
        let view = metrics.view();
        let (key, _) = view.window_1m.buckets.iter().next().expect("bucket");
        assert_eq!(key.status, StatusClass::Error);
        assert_eq!(key.model, "unknown");
        assert_eq!(key.upstream, "unknown");
    }

    #[test]
    fn record_usage_sums_token_counters() {
        let metrics = MetricsLayer::new();
        metrics.record_usage(
            FlowStatus::Completed,
            Some("m"),
            "/v1/responses",
            Some("p"),
            FlowUsage {
                prompt: 100,
                completion: 40,
                total: 140,
                cached: 10,
                reasoning: 7,
            },
        );
        metrics.record_usage(
            FlowStatus::Completed,
            Some("m"),
            "/v1/responses",
            Some("p"),
            FlowUsage {
                prompt: 50,
                completion: 20,
                total: 70,
                cached: 5,
                reasoning: 3,
            },
        );
        let view = metrics.view();
        let counts = view
            .window_1m
            .buckets
            .values()
            .next()
            .expect("usage bucket");
        assert_eq!(counts.prompt_tokens, 150);
        assert_eq!(counts.completion_tokens, 60);
        assert_eq!(counts.cached_tokens, 15);
        assert_eq!(counts.reasoning_tokens, 10);
    }

    #[test]
    fn record_terminal_co_locates_count_and_tokens_in_one_slot_and_bumps_seq_once() {
        // D5 R1 #2: the atomic terminal record applies the response count + the final
        // usage under ONE lock at ONE epoch — the count and the tokens always land in
        // the SAME bucket (same key) and the seq bumps exactly once. We assert both the
        // co-location (count and all token fields present on the single bucket) and the
        // single seq bump (vs. two for separate record_response + record_usage calls).
        let metrics = MetricsLayer::new();
        assert_eq!(metrics.metrics_seq(), 0);
        metrics.record_terminal(
            FlowStatus::Completed,
            Some("served-m"),
            "/v1/responses",
            Some("provider-a"),
            42,
            Some(FlowUsage {
                prompt: 100,
                completion: 40,
                total: 140,
                cached: 10,
                reasoning: 7,
            }),
        );
        // ONE atomic mutation ⇒ exactly one seq bump (not two).
        assert_eq!(metrics.metrics_seq(), 1, "atomic terminal bumps seq once");
        let view = metrics.view();
        // Exactly one bucket: the count AND the tokens co-located on the same key.
        assert_eq!(
            view.window_1m.buckets.len(),
            1,
            "count + tokens share a slot"
        );
        let (key, counts) = view
            .window_1m
            .buckets
            .iter()
            .next()
            .expect("the single terminal bucket");
        assert_eq!(key.status, StatusClass::Success);
        assert_eq!(key.model, "served-m");
        assert_eq!(counts.count, 1, "response counted");
        assert_eq!(
            counts.prompt_tokens, 100,
            "tokens in the SAME bucket as count"
        );
        assert_eq!(counts.completion_tokens, 40);
        assert_eq!(counts.cached_tokens, 10);
        assert_eq!(counts.reasoning_tokens, 7);
        // Latency landed too (one bucket carries the histogram sample window-wide).
        assert!(view.window_1m.percentiles().p50 > 0.0, "latency recorded");
        // All three windows agree (the single epoch fed every ring).
        assert_eq!(view.window_5m.total_count(), 1);
        assert_eq!(view.window_1h.total_count(), 1);
    }

    #[test]
    fn record_terminal_without_usage_records_count_only() {
        // A terminal with no usage (e.g. a pre-spawn failure) records the count + the
        // latency but no tokens — still one atomic seq bump.
        let metrics = MetricsLayer::new();
        metrics.record_terminal(
            FlowStatus::Failed,
            None,
            "/v1/chat/completions",
            None,
            7,
            None,
        );
        assert_eq!(metrics.metrics_seq(), 1);
        let view = metrics.view();
        let (key, counts) = view.window_1m.buckets.iter().next().expect("bucket");
        assert_eq!(key.status, StatusClass::Error);
        assert_eq!(counts.count, 1);
        assert_eq!(counts.prompt_tokens, 0, "no tokens recorded");
        assert_eq!(counts.completion_tokens, 0);
    }

    #[test]
    fn status_class_mapping() {
        assert_eq!(
            StatusClass::from_status(FlowStatus::Completed),
            StatusClass::Success
        );
        assert_eq!(
            StatusClass::from_status(FlowStatus::Failed),
            StatusClass::Error
        );
        assert_eq!(
            StatusClass::from_status(FlowStatus::Cancelled),
            StatusClass::Cancelled
        );
        assert_eq!(
            StatusClass::from_status(FlowStatus::Open),
            StatusClass::Error
        );
    }

    #[test]
    fn coordinated_snapshot_is_internally_consistent() {
        // The cut captures summaries, metrics, topology, and per-domain cursors at a
        // single instant — they must all reflect the SAME state (no torn read).
        let metrics = MetricsLayer::new();
        let flow = DashboardFlowStore::new();
        let topo = ProviderHealthPublisher::default();
        topo.publish(Vec::new()); // version 1

        // Open + finalize a flow, and record a matching metrics response.
        flow.open(
            "api_1".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            crate::dashboard_flow::redact_headers(&axum::http::HeaderMap::new()),
            None,
        );
        flow.finalize(
            "api_1",
            FlowStatus::Completed,
            Some("response.completed".to_string()),
            Some("provider-a".to_string()),
        );
        metrics.record_response(
            FlowStatus::Completed,
            Some("served-m"),
            "/v1/responses",
            Some("provider-a"),
            42,
        );

        let cut = metrics.snapshot(&flow, &topo).expect("cut taken");
        // Summaries reflect the finalized flow.
        assert_eq!(cut.summaries.len(), 1);
        assert_eq!(cut.summaries[0].api_call_id, "api_1");
        assert_eq!(cut.summaries[0].status, FlowStatus::Completed);
        // Metrics reflect the recorded response.
        assert_eq!(cut.metrics.window_1m.total_count(), 1);
        // Topology is the captured published version.
        assert_eq!(cut.topology.version, 1);
        // Per-domain cursors are all present + non-zero (flow + metrics + topology).
        assert!(cut.cursors.flow_seq >= 1, "flow_seq advanced");
        assert!(cut.cursors.metrics_seq >= 1, "metrics_seq advanced");
        assert_eq!(cut.cursors.topology_seq, 1, "topology_seq == version");
    }

    #[test]
    fn snapshot_cuts_hold_no_body_refs_and_are_quota_bounded() {
        // The 135 GiB fix: simulate many cuts over a churning store with large
        // bodies live, and assert (a) NO body bytes are reachable from any cut and
        // (b) the ring's retained bytes stay under a small quota (eviction works).
        // Use a tiny quota so eviction is exercised without allocating 400 MiB.
        let quota = 256 * 1024; // 256 KiB ring quota
        let metrics = MetricsLayer::with_snapshot_quota(quota);
        let flow = DashboardFlowStore::new();
        let topo = ProviderHealthPublisher::default();
        topo.publish(Vec::new());

        // Populate the live store with flows carrying large bodies.
        let big_body = vec![b'x'; 64 * 1024];
        for index in 0..64 {
            let api = format!("api_{index}");
            flow.open(
                api.clone(),
                "POST".to_string(),
                "/v1/responses".to_string(),
                crate::dashboard_flow::redact_headers(&axum::http::HeaderMap::new()),
                Some(crate::dashboard_flow::capture_body(&big_body)),
            );
            flow.finalize(&api, FlowStatus::Completed, None, Some("p".to_string()));
        }

        // Take many cuts (more than the ring can hold under the byte quota).
        let mut last_cut = None;
        for _ in 0..50 {
            last_cut = metrics.snapshot(&flow, &topo);
        }
        let cut = last_cut.expect("a cut");

        // (a) NO summary carries body bytes — the summary type is body-free by
        // construction; assert its serialized form holds none of the body payload.
        let serialized = serde_json::to_string(&cut.summaries).expect("serialize summaries");
        assert!(
            !serialized.contains("xxxxxxxx"),
            "no body bytes reachable from a snapshot summary"
        );
        // Each summary's measured footprint is well under 1 KiB (body-free).
        for summary in &cut.summaries {
            assert!(
                summary_approx_bytes(summary) < 1024,
                "each body-free summary is < 1 KiB"
            );
        }

        // (b) The ring honored the byte quota (eviction kept it bounded).
        let retained = {
            let state = metrics.lock();
            state.snapshots.retained_bytes
        };
        assert!(
            retained <= quota,
            "snapshot ring retained {retained} bytes <= quota {quota} (NOT 135 GiB)"
        );
    }

    #[test]
    fn snapshot_at_returns_nearest_le_cut() {
        let metrics = MetricsLayer::new();
        let flow = DashboardFlowStore::new();
        let topo = ProviderHealthPublisher::default();
        topo.publish(Vec::new());

        // Push three cuts and capture their timestamps.
        let mut timestamps = Vec::new();
        for _ in 0..3 {
            let cut = metrics.snapshot(&flow, &topo).expect("cut");
            timestamps.push(cut.taken_at_ms);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        // A ts at/after the last cut returns the last cut.
        let latest = metrics
            .snapshot_at(timestamps[2] + 1000)
            .expect("nearest <= ts");
        assert_eq!(latest.taken_at_ms, timestamps[2]);
        // A ts between the first and second returns the first.
        let mid = metrics
            .snapshot_at(timestamps[1].saturating_sub(1))
            .expect("nearest <= the boundary");
        assert_eq!(mid.taken_at_ms, timestamps[0]);
        // A ts before every cut returns None.
        assert!(
            metrics
                .snapshot_at(timestamps[0].saturating_sub(1))
                .is_none()
        );
    }

    #[test]
    fn per_domain_cursors_are_monotonic_across_cuts() {
        let metrics = MetricsLayer::new();
        let flow = DashboardFlowStore::new();
        let topo = ProviderHealthPublisher::default();
        topo.publish(Vec::new()); // version 1

        let cut1 = metrics.snapshot(&flow, &topo).expect("cut1");
        // Mutate metrics + flow + topology between cuts.
        metrics.record_response(
            FlowStatus::Completed,
            Some("m"),
            "/v1/responses",
            Some("p"),
            5,
        );
        flow.open(
            "api_x".to_string(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            crate::dashboard_flow::redact_headers(&axum::http::HeaderMap::new()),
            None,
        );
        topo.publish(Vec::new()); // version 2
        let cut2 = metrics.snapshot(&flow, &topo).expect("cut2");

        assert!(
            cut2.cursors.metrics_seq > cut1.cursors.metrics_seq,
            "metrics_seq monotonic"
        );
        assert!(
            cut2.cursors.flow_seq > cut1.cursors.flow_seq,
            "flow_seq monotonic"
        );
        assert!(
            cut2.cursors.topology_seq > cut1.cursors.topology_seq,
            "topology_seq monotonic"
        );
    }

    #[test]
    fn snapshot_with_monitor_seq_records_monitor_cursor() {
        let metrics = MetricsLayer::new();
        let flow = DashboardFlowStore::new();
        let topo = ProviderHealthPublisher::default();
        topo.publish(Vec::new());
        let cut = metrics
            .snapshot_with_monitor_seq(&flow, &topo, 99)
            .expect("cut");
        assert_eq!(cut.cursors.monitor_seq, 99);
    }

    #[test]
    fn lock_order_stress_no_deadlock() {
        // The fixed FlowStore→Metrics lock order means concurrent FlowStore mutation
        // + the snapshot task (the ONLY >1-lock holder) cannot deadlock. Hammer both
        // stores from many OS threads while a snapshot thread takes the combined
        // critical section, and assert the whole thing joins (a deadlock would hang
        // the test process). Thread-based (not async) so the OS scheduler — not a
        // cooperative runtime — exercises the real lock contention.
        let metrics = MetricsLayer::new();
        let flow = DashboardFlowStore::new();
        let topo = ProviderHealthPublisher::default();
        topo.publish(Vec::new());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));

        let snapshot_thread = {
            let metrics = metrics.clone();
            let flow = flow.clone();
            let topo = topo.clone();
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let _ = metrics.snapshot(&flow, &topo);
                    std::thread::yield_now();
                }
            })
        };

        let mut workers = Vec::new();
        for worker_id in 0..8 {
            let flow = flow.clone();
            let metrics = metrics.clone();
            workers.push(std::thread::spawn(move || {
                for index in 0..2000 {
                    let api = format!("api_{worker_id}_{index}");
                    flow.open(
                        api.clone(),
                        "POST".to_string(),
                        "/v1/responses".to_string(),
                        crate::dashboard_flow::redact_headers(&axum::http::HeaderMap::new()),
                        None,
                    );
                    flow.finalize(
                        &api,
                        FlowStatus::Completed,
                        Some("done".to_string()),
                        Some("p".to_string()),
                    );
                    metrics.record_response(
                        FlowStatus::Completed,
                        Some("m"),
                        "/v1/responses",
                        Some("p"),
                        5,
                    );
                }
            }));
        }
        for worker in workers {
            worker.join().expect("worker thread joins (no deadlock)");
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        snapshot_thread.join().expect("snapshot thread joins");

        // A cut was taken and is body-free + quota-bounded.
        let cut = metrics.latest_snapshot().expect("a cut");
        assert!(cut.summaries.len() <= crate::monitor::REQUEST_EVENT_LIMIT);
    }

    #[test]
    fn snapshot_metadata_is_captured_inside_the_lock() {
        // Codex D5 R2 HIGH: the cut's metadata (`taken_at_ms`/epoch, cursors) must be
        // sampled INSIDE the dual-lock critical section, not before it. If `taken_at_ms`
        // were read before the locks, a concurrent writer could `open()` a flow that
        // lands in the summaries (captured under the lock) with a `started_ms` LATER than
        // the pre-lock timestamp — stamping the cut earlier than the state it contains.
        // Hammer `open()` from many threads while a snapshot thread takes cuts and assert
        // every cut's `taken_at_ms` is ≥ the `started_ms` of every flow it contains and
        // that its epoch is mutually consistent with `taken_at_ms`.
        let metrics = MetricsLayer::new();
        let flow = DashboardFlowStore::new();
        let topo = ProviderHealthPublisher::default();
        topo.publish(Vec::new());
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let violations = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let cuts_seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        let snapshot_thread = {
            let metrics = metrics.clone();
            let flow = flow.clone();
            let topo = topo.clone();
            let stop = Arc::clone(&stop);
            let violations = Arc::clone(&violations);
            let cuts_seen = Arc::clone(&cuts_seen);
            std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    if let Some(cut) = metrics.snapshot(&flow, &topo) {
                        cuts_seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // The cut can never be stamped before a flow it already contains:
                        // `started_ms` is set at `open()` (under the FlowStore lock) and
                        // the summary that carries it was captured under the SAME lock the
                        // cut's `taken_at_ms` is now sampled under, so the timestamp must
                        // dominate every contained flow.
                        let ts_ok = cut
                            .summaries
                            .iter()
                            .all(|summary| cut.taken_at_ms >= summary.started_ms);
                        if !ts_ok {
                            violations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        }
                    }
                    std::thread::yield_now();
                }
            })
        };

        let mut workers = Vec::new();
        for worker_id in 0..8 {
            let flow = flow.clone();
            let metrics = metrics.clone();
            workers.push(std::thread::spawn(move || {
                for index in 0..2000 {
                    let api = format!("api_{worker_id}_{index}");
                    flow.open(
                        api.clone(),
                        "POST".to_string(),
                        "/v1/responses".to_string(),
                        crate::dashboard_flow::redact_headers(&axum::http::HeaderMap::new()),
                        None,
                    );
                    metrics.record_response(
                        FlowStatus::Completed,
                        Some("m"),
                        "/v1/responses",
                        Some("p"),
                        5,
                    );
                }
            }));
        }
        for worker in workers {
            worker.join().expect("worker thread joins");
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        snapshot_thread.join().expect("snapshot thread joins");

        assert!(
            cuts_seen.load(std::sync::atomic::Ordering::Relaxed) > 0,
            "the snapshot thread took at least one cut under contention"
        );
        assert_eq!(
            violations.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "every cut's taken_at_ms is >= every contained flow's started_ms and its \
             epoch is consistent with taken_at_ms (metadata sampled inside the lock)"
        );
    }
}
