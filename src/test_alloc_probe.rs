//! Crate-internal, test-only peak-allocation probe shared across unit-test
//! modules. There can be exactly ONE `#[global_allocator]` per crate, so this is
//! the single home for the allocator-observing probe; modules that need to bound
//! a hot path's heap use (`sse_guard` reject-path, `dashboard_flow::capture_body`)
//! arm it via [`peak_alloc_during`] rather than defining their own allocator.
//!
//! A passthrough `System` allocator records the largest single allocation
//! `>= THRESHOLD` bytes made on the CURRENT thread while it is armed. This catches
//! a regressed implementation that materializes a whole body/chunk in one
//! allocation (e.g. `Bytes::copy_from_slice`, `serde_json::from_slice::<Value>`
//! over a 10 MiB body, or `buf = carry ++ chunk`) even when the survivor-side byte
//! accounting is structurally blind to that temporary.
//!
//! Inert by construction outside the probe: the global allocator does one
//! thread-local `Cell` read per `alloc` and returns immediately unless THIS thread
//! has armed it, so the rest of the suite (and every release build — the whole
//! module is `#[cfg(test)]`) pays nothing. Arming is THREAD-LOCAL, so other tests
//! running in parallel (incl. ones that allocate many MiB of their own) are never
//! observed; only allocations on the arming thread between arm/disarm count.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

thread_local! {
    /// Whether THIS thread is currently inside an armed region.
    static ARMED: Cell<bool> = const { Cell::new(false) };
    /// Only allocations `>= THRESHOLD` bytes are recorded — filters out the
    /// incidental small allocations (error-string formatting, `Vec` growth, small
    /// scalar copies) so the probe fires ONLY on a body/chunk-sized copy.
    static THRESHOLD: Cell<usize> = const { Cell::new(usize::MAX) };
    /// High-water mark of the largest single recorded allocation, in bytes.
    static PEAK: Cell<usize> = const { Cell::new(0) };
    /// Re-entrancy guard so any allocation made *inside* the hook (none today)
    /// cannot recurse into recording.
    static IN_HOOK: Cell<bool> = const { Cell::new(false) };
}

/// A passthrough `System` allocator that, only while the CURRENT thread is armed,
/// records the largest single allocation `>= threshold`.
struct ProbeAlloc;

// SAFETY: every method forwards verbatim to `System`; the recording side does not
// allocate and is re-entrancy-guarded, so it cannot perturb allocation.
unsafe impl GlobalAlloc for ProbeAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        record(layout.size());
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        record(layout.size());
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let np = unsafe { System.realloc(ptr, layout, new_size) };
        record(new_size);
        np
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Record `size` against this thread's peak iff armed and at/over threshold.
/// During teardown a thread-local may be destroyed, so `try_with` degrades to a
/// no-op rather than panicking.
fn record(size: usize) {
    let armed = ARMED.try_with(|c| c.get()).unwrap_or(false);
    if !armed {
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
        IN_HOOK.with(|c| c.set(false));
    }
}

#[global_allocator]
static GLOBAL: ProbeAlloc = ProbeAlloc;

/// Run `body` with the allocation probe armed on the current thread, recording any
/// single allocation `>= threshold`, and return the largest such recorded
/// allocation in bytes (0 if none). Allocations made BEFORE the call (e.g. the
/// hostile input chunk / pre-built body) are not counted — only what `body`
/// allocates is.
pub(crate) fn peak_alloc_during<R>(threshold: usize, body: impl FnOnce() -> R) -> (R, usize) {
    PEAK.with(|c| c.set(0));
    THRESHOLD.with(|c| c.set(threshold));
    ARMED.with(|c| c.set(true));
    let out = body();
    ARMED.with(|c| c.set(false));
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
    PEAK.with(|c| c.set(0));
    THRESHOLD.with(|c| c.set(threshold));
    ARMED.with(|c| c.set(true));
    let out = fut.await;
    ARMED.with(|c| c.set(false));
    let peak = PEAK.with(|c| c.get());
    (out, peak)
}
