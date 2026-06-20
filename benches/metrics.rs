//! D5 criterion bench: proves the zero-cost `MetricsLayer::disabled()` path adds no
//! work (no allocation, no lock, no clone) vs the enabled record path, and that the
//! per-domain `metrics_seq` updates are not a hot-spot under contention.
//!
//! Run with `cargo bench --bench metrics`. The `disabled_*` benches are the
//! production hot path (debug UI off): each method early-returns before any lock or
//! ring work, so its time should be a handful of nanoseconds (an `enabled` bool
//! check + an `Option`/early return), strictly dominated by the `enabled_*` variants.

use criterion::Criterion;
use criterion::criterion_group;
use criterion::criterion_main;
use llmconduit::dashboard_flow::DashboardFlowStore;
use llmconduit::dashboard_flow::FlowStatus;
use llmconduit::dashboard_flow::FlowUsage;
use llmconduit::metrics::MetricsLayer;
use llmconduit::upstream::ProviderHealthPublisher;
use std::hint::black_box;

/// The disabled `record_response` is the production hot path: it must early-return
/// before any lock/ring work. This bench is the zero-cost assertion — its time is
/// the `enabled` check only.
fn bench_record_response_disabled(criterion: &mut Criterion) {
    let metrics = MetricsLayer::disabled();
    criterion.bench_function("record_response_disabled", |bencher| {
        bencher.iter(|| {
            metrics.record_response(
                black_box(FlowStatus::Completed),
                black_box(Some("served-model")),
                black_box("/v1/responses"),
                black_box(Some("provider-a")),
                black_box(42),
            );
        });
    });
}

/// The enabled `record_response` does the real ring + histogram work under the lock.
/// Contrasts with the disabled path to show the disabled path adds nothing.
fn bench_record_response_enabled(criterion: &mut Criterion) {
    let metrics = MetricsLayer::new();
    criterion.bench_function("record_response_enabled", |bencher| {
        bencher.iter(|| {
            metrics.record_response(
                black_box(FlowStatus::Completed),
                black_box(Some("served-model")),
                black_box("/v1/responses"),
                black_box(Some("provider-a")),
                black_box(42),
            );
        });
    });
}

/// The disabled `record_usage` hot path (zero-cost).
fn bench_record_usage_disabled(criterion: &mut Criterion) {
    let metrics = MetricsLayer::disabled();
    let usage = FlowUsage {
        prompt: 100,
        completion: 40,
        total: 140,
        cached: 10,
        reasoning: 7,
    };
    criterion.bench_function("record_usage_disabled", |bencher| {
        bencher.iter(|| {
            metrics.record_usage(
                black_box(FlowStatus::Completed),
                black_box(Some("served-model")),
                black_box("/v1/responses"),
                black_box(Some("provider-a")),
                black_box(usage),
            );
        });
    });
}

/// Per-domain `metrics_seq` read under contention: many records have run, and the
/// cursor read is a single locked field load. Asserts the seq accessor is not a
/// hot-spot (a cheap lock + scalar load), not a re-derivation.
fn bench_metrics_seq_read(criterion: &mut Criterion) {
    let metrics = MetricsLayer::new();
    // Warm the rings so the seq is non-trivial.
    for _ in 0..1000 {
        metrics.record_response(
            FlowStatus::Completed,
            Some("m"),
            "/v1/responses",
            Some("p"),
            5,
        );
    }
    criterion.bench_function("metrics_seq_read", |bencher| {
        bencher.iter(|| black_box(metrics.metrics_seq()));
    });
}

/// The coordinated 5 s snapshot cut (the combined FlowStore→Metrics critical
/// section + one topology Arc capture + a body-free cut pushed onto the ring), over
/// a store holding a realistic number of finalized flows. Benches the cost of the
/// atomic cut (pointer + scalar copies of body-free summaries, no body copy).
fn bench_coordinated_snapshot(criterion: &mut Criterion) {
    let metrics = MetricsLayer::new();
    let flow_store = DashboardFlowStore::new();
    let topology = ProviderHealthPublisher::default();
    topology.publish(Vec::new());
    // Populate the store with finalized flows (body-free summaries dominate the cut).
    for index in 0..256 {
        let api = format!("api_{index}");
        flow_store.open(
            api.clone(),
            "POST".to_string(),
            "/v1/responses".to_string(),
            llmconduit::dashboard_flow::redact_headers(&axum::http::HeaderMap::new()),
            None,
        );
        flow_store.finalize(
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
    criterion.bench_function("coordinated_snapshot_256_flows", |bencher| {
        bencher.iter(|| {
            black_box(metrics.snapshot(black_box(&flow_store), black_box(&topology)));
        });
    });
}

criterion_group!(
    benches,
    bench_record_response_disabled,
    bench_record_response_enabled,
    bench_record_usage_disabled,
    bench_metrics_seq_read,
    bench_coordinated_snapshot,
);
criterion_main!(benches);
