//! Crate-internal, test-only allocation probe shared across unit-test modules.
//! There can be exactly ONE `#[global_allocator]` per crate, so this is the single
//! home for the allocator-observing probe; modules that need to bound a hot path's
//! heap use (`sse_guard` reject-path, `dashboard_flow::capture_body`) arm it rather
//! than defining their own allocator.
//!
//! Two measurement modes share the same passthrough `System` allocator + per-thread
//! arming:
//! - [`peak_alloc_during`] / [`peak_alloc_during_async`]: the largest SINGLE
//!   allocation `>= THRESHOLD` made on the current thread while armed. Catches a
//!   regression that materializes a whole body/chunk in ONE allocation
//!   (`Bytes::copy_from_slice`, `serde_json::from_slice::<Value>` over a 10 MiB
//!   body, `buf = carry ++ chunk`).
//! - [`peak_live_alloc_during`]: the peak NET-LIVE bytes (`alloc − free`) held at
//!   any instant while armed. Catches a path that makes MANY small allocations
//!   summing to O(body) which the largest-single mode would miss (D1 R2 #5).
//!
//! Both arm via an RAII [`ArmGuard`] so a panic inside the measured closure cannot
//! leave the probe armed for later tests on the same thread.
//!
//! Inert by construction outside the probe: the global allocator does one
//! thread-local `Cell` read per `alloc`/`dealloc` and returns immediately unless
//! THIS thread is armed, so the rest of the suite (and every release build — the
//! whole module is `#[cfg(test)]`) pays nothing. Arming is THREAD-LOCAL, so tests
//! running in parallel are never observed; only allocations on the arming thread
//! between arm/disarm count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    /// Whether THIS thread is currently inside an armed region.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    /// Only allocations `>= THRESHOLD` bytes count toward [`PEAK`] (largest-single
    /// mode) — filters out incidental small allocations so that probe fires ONLY on
    /// a body/chunk-sized copy. Live-byte mode ignores the threshold.
    static THRESHOLD: Cell<usize> = const { Cell::new(usize::MAX) };
    /// High-water mark of the largest single recorded allocation, in bytes.
    static PEAK: Cell<usize> = const { Cell::new(0) };
    /// Running net-live bytes (`alloc − free`) held on this thread while armed.
    static LIVE: Cell<isize> = const { Cell::new(0) };
    /// High-water mark of [`LIVE`] — the most live bytes held at any instant.
    static PEAK_LIVE: Cell<isize> = const { Cell::new(0) };
    /// Re-entrancy guard so any allocation made *inside* the hook (none today)
    /// cannot recurse into recording.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// A passthrough `System` allocator that, only while the CURRENT thread is armed,
/// records the largest single allocation `>= threshold` AND tracks net-live bytes.
struct ProbeAlloc;

// SAFETY: every method forwards verbatim to `System`; the recording side does not
// allocate and is re-entrancy-guarded, so it cannot perturb allocation.
unsafe impl GlobalAlloc for ProbeAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            record_alloc(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            record_alloc(layout.size());
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let np = unsafe { System.realloc(ptr, layout, new_size) };
        if !np.is_null() {
            // A realloc frees `layout.size()` and allocates `new_size`; reflect both
            // in the live total, and treat `new_size` as a single allocation for the
            // largest-single peak.
            record_dealloc(layout.size());
            record_alloc(new_size);
        }
        np
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        record_dealloc(layout.size());
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Record an allocation of `size` against this thread iff armed: bump net-live (+
/// its peak) and, when `size >= threshold`, the largest-single peak. During
/// teardown a thread-local may be destroyed, so `try_with` degrades to a no-op.
fn record_alloc(size: usize) {
    if !ARMED.try_with(|c| c.get()).unwrap_or(false) {
        return;
    }
    let reentrant = IN_HOOK.with(|c| {
        let was = c.get();
        c.set(true);
        was
    });
    if !reentrant {
        let threshold = THRESHOLD.with(|c| c.get());
        if size >= threshold {
            PEAK.with(|c| c.set(c.get().max(size)));
        }
        LIVE.with(|live| {
            let next = live.get().saturating_add(size as isize);
            live.set(next);
            PEAK_LIVE.with(|peak| {
                if next > peak.get() {
                    peak.set(next);
                }
            });
        });
        IN_HOOK.with(|c| c.set(false));
    }
}

/// Record a deallocation of `size` against this thread's net-live total iff armed.
fn record_dealloc(size: usize) {
    if !ARMED.try_with(|c| c.get()).unwrap_or(false) {
        return;
    }
    let reentrant = IN_HOOK.with(|c| {
        let was = c.get();
        c.set(true);
        was
    });
    if !reentrant {
        LIVE.with(|live| live.set(live.get().saturating_sub(size as isize)));
        IN_HOOK.with(|c| c.set(false));
    }
}

#[global_allocator]
static GLOBAL: ProbeAlloc = ProbeAlloc;

/// RAII arm guard: arms the current thread on construction and DISARMS on drop, so
/// a panic inside the measured closure cannot leave the probe armed for later tests
/// on the same thread (D1 R2 #5). Resets all counters on arm.
struct ArmGuard;

impl ArmGuard {
    fn arm(threshold: usize) -> Self {
        PEAK.with(|c| c.set(0));
        LIVE.with(|c| c.set(0));
        PEAK_LIVE.with(|c| c.set(0));
        THRESHOLD.with(|c| c.set(threshold));
        ARMED.with(|c| c.set(true));
        ArmGuard
    }
}

impl Drop for ArmGuard {
    fn drop(&mut self) {
        ARMED.with(|c| c.set(false));
    }
}

/// Run `body` with the probe armed; return the largest SINGLE allocation
/// `>= threshold` (0 if none). Allocations BEFORE the call are not counted. The
/// RAII guard disarms even if `body` panics.
pub(crate) fn peak_alloc_during<R>(threshold: usize, body: impl FnOnce() -> R) -> (R, usize) {
    let _guard = ArmGuard::arm(threshold);
    let out = body();
    let peak = PEAK.with(|c| c.get());
    (out, peak)
}

/// `async` sibling of [`peak_alloc_during`]: arm across an `.await`-driven section.
/// Sound under the default current-thread `#[tokio::test]` runtime, where polling
/// happens inline on the arming thread.
pub(crate) async fn peak_alloc_during_async<F, R>(threshold: usize, fut: F) -> (R, usize)
where
    F: std::future::Future<Output = R>,
{
    let _guard = ArmGuard::arm(threshold);
    let out = fut.await;
    let peak = PEAK.with(|c| c.get());
    (out, peak)
}

/// Run `body` with the probe armed; return the peak NET-LIVE bytes (`alloc − free`)
/// held at any instant while armed (0 if none). Unlike [`peak_alloc_during`] this
/// catches a path doing MANY small allocations that sum to O(body) but are not freed
/// until the end (D1 R2 #5). Allocations BEFORE the call are not counted; the RAII
/// guard disarms even if `body` panics.
pub(crate) fn peak_live_alloc_during<R>(body: impl FnOnce() -> R) -> (R, usize) {
    let _guard = ArmGuard::arm(usize::MAX); // threshold irrelevant for live mode
    let out = body();
    let peak_live = PEAK_LIVE.with(|c| c.get()).max(0) as usize;
    (out, peak_live)
}
