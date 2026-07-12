//! Debug-only backend Prometheus collection and normalized engine telemetry.
//!
//! This domain is intentionally observational. Scrapes are sent directly to each
//! concrete leaf and never pass through serving failover/cooldown state. The raw
//! `/metrics` proxy remains a separate, byte-preserving surface.

use futures::StreamExt;
use prometheus_parse::{Scrape, Value};
use reqwest::{Client, StatusCode, Url};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const BACKEND_METRICS_ENV: &str = "LLMCONDUIT_BACKEND_METRICS";
const BODY_LIMIT: usize = 2 * 1024 * 1024;
const SAMPLE_LIMIT: usize = 50_000;
const LABEL_LIMIT: usize = 256;
const MAX_LOGICAL_PROVIDERS: usize = 64;
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(2);
const SCRAPE_INTERVAL: Duration = Duration::from_secs(5);
const STALE_AFTER: Duration = Duration::from_secs(10);
const LONG_RETRY: Duration = Duration::from_secs(300);

#[derive(Clone)]
pub struct BackendMetricsTarget {
    pub route: Option<String>,
    pub provider_id: String,
    metrics_url: Url,
    client: Client,
    api_key: Option<String>,
    credential_identity: [u8; 32],
}

impl std::fmt::Debug for BackendMetricsTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut redacted_url = self.metrics_url.clone();
        let _ = redacted_url.set_username("");
        let _ = redacted_url.set_password(None);
        redacted_url.set_query(None);
        f.debug_struct("BackendMetricsTarget")
            .field("route", &self.route)
            .field("provider_id", &self.provider_id)
            .field("metrics_url", &redacted_url)
            .field("authenticated", &self.api_key.is_some())
            .finish()
    }
}

impl BackendMetricsTarget {
    pub(crate) fn new(
        route: Option<String>,
        provider_id: String,
        metrics_url: Url,
        client: Client,
        api_key: Option<String>,
    ) -> Self {
        let credential_identity: [u8; 32] =
            Sha256::digest(api_key.as_deref().unwrap_or_default()).into();
        Self {
            route,
            provider_id,
            metrics_url,
            client,
            api_key,
            credential_identity,
        }
    }

    fn physical_key(&self) -> PhysicalKey {
        PhysicalKey {
            url: self.metrics_url.to_string(),
            credential_identity: self.credential_identity,
        }
    }

    fn logical_key(&self) -> LogicalProviderKey {
        LogicalProviderKey {
            route: self.route.clone(),
            provider_id: self.provider_id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct PhysicalKey {
    url: String,
    credential_identity: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct LogicalProviderKey {
    pub route: Option<String>,
    pub provider_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackendEngineKind {
    Vllm,
    Sglang,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackendMetricsStatus {
    Warming,
    Fresh,
    Stale,
    Unsupported,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BackendMetricsCoverage {
    Full,
    Partial,
}

/// Physically de-duplicated per-request output-token throughput from the newest
/// successful Prometheus TPOT interval across configured engines. The value is the
/// inverse mean `request_time_per_output_token_seconds`; speculative accepted tokens
/// are already reflected in that shorter output-token time and are never double-counted.
/// One physical `/metrics` endpoint contributes once even when several logical routes
/// point at it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct EngineThroughputSample {
    pub generated_tokens_per_sec: f64,
    pub sampled_at_ms: u128,
    pub measured_sources: u64,
    pub total_sources: u64,
    pub coverage: BackendMetricsCoverage,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BackendInstantMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub running_requests: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_requests: Option<f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub waiting_by_reason: BTreeMap<String, f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kv_cache_utilization: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kv_cache_token_capacity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_sleeping: Option<bool>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BackendHistogramSummary {
    pub samples: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p50: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p95: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub p99: Option<f64>,
    pub quantile_method: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BackendLatencyMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<BackendHistogramSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inter_token_ms: Option<BackendHistogramSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time_per_output_token_ms: Option<BackendHistogramSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_to_end_ms: Option<BackendHistogramSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queue_ms: Option<BackendHistogramSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inference_ms: Option<BackendHistogramSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefill_ms: Option<BackendHistogramSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub decode_ms: Option<BackendHistogramSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens: Option<BackendHistogramSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_tokens: Option<BackendHistogramSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub iteration_tokens: Option<BackendHistogramSummary>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BackendMetricsWindow {
    pub samples: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_tokens_per_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cached_prompt_tokens_per_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generated_tokens_per_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completed_requests_per_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preemptions_per_sec: Option<f64>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub finish_reasons: BTreeMap<String, u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prefix_cache_hit_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt_token_cache_hit_ratio: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speculative_draft_tokens_per_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speculative_accepted_tokens_per_sec: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speculative_acceptance_ratio: Option<f64>,
    pub histograms: BackendLatencyMetrics,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct BackendMetricWindows {
    pub m1: BackendMetricsWindow,
    pub m5: BackendMetricsWindow,
    pub h1: BackendMetricsWindow,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BackendProviderMetrics {
    pub engine_kind: BackendEngineKind,
    pub status: BackendMetricsStatus,
    pub coverage: BackendMetricsCoverage,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scraped_at_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_class: Option<String>,
    pub instant: BackendInstantMetrics,
    pub windows: BackendMetricWindows,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BackendMetricsSnapshot {
    pub seq: u64,
    pub generated_at_ms: u128,
    pub providers: BTreeMap<LogicalProviderKey, BackendProviderMetrics>,
    pub overflow_count: u64,
    /// Latest inverse-mean per-request TPOT, aggregated before logical-provider
    /// fan-out so aliases cannot double count one engine.
    #[serde(default)]
    pub engine_throughput: Option<EngineThroughputSample>,
}

impl BackendMetricsSnapshot {
    pub fn provider(
        &self,
        route: Option<&str>,
        provider_id: &str,
    ) -> Option<&BackendProviderMetrics> {
        self.providers.get(&LogicalProviderKey {
            route: route.map(ToString::to_string),
            provider_id: provider_id.to_string(),
        })
    }

    pub(crate) fn approx_bytes(&self) -> usize {
        let mut bytes = std::mem::size_of::<Self>();
        for (key, value) in &self.providers {
            bytes = bytes
                .saturating_add(key.provider_id.capacity())
                .saturating_add(key.route.as_ref().map_or(0, String::capacity))
                .saturating_add(value.last_error_class.as_ref().map_or(0, String::capacity));
            for window in [&value.windows.m1, &value.windows.m5, &value.windows.h1] {
                bytes = bytes.saturating_add(
                    window
                        .finish_reasons
                        .keys()
                        .map(String::capacity)
                        .sum::<usize>(),
                );
            }
        }
        bytes
    }
}

#[derive(Debug, Clone)]
pub struct BackendMetricsStore {
    inner: Option<Arc<Mutex<Arc<BackendMetricsSnapshot>>>>,
}

impl BackendMetricsStore {
    pub fn new() -> Self {
        Self {
            inner: Some(Arc::new(Mutex::new(Arc::new(
                BackendMetricsSnapshot::default(),
            )))),
        }
    }

    pub fn disabled() -> Self {
        Self { inner: None }
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.is_some()
    }

    pub fn latest(&self) -> Arc<BackendMetricsSnapshot> {
        self.inner.as_ref().map_or_else(
            || Arc::new(BackendMetricsSnapshot::default()),
            |inner| Arc::clone(&inner.lock().expect("backend metrics store poisoned")),
        )
    }

    fn publish(&self, mut snapshot: BackendMetricsSnapshot) {
        let Some(inner) = &self.inner else { return };
        let mut guard = inner.lock().expect("backend metrics store poisoned");
        snapshot.seq = guard.seq.saturating_add(1);
        *guard = Arc::new(snapshot);
    }
}

impl Default for BackendMetricsStore {
    fn default() -> Self {
        Self::disabled()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ScrapeErrorClass {
    Unsupported,
    Authentication,
    RateLimited,
    Http5xx,
    Timeout,
    Transport,
    BodyLimit,
    Parse,
}

impl ScrapeErrorClass {
    fn as_str(self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported",
            Self::Authentication => "authentication",
            Self::RateLimited => "rate_limited",
            Self::Http5xx => "http_5xx",
            Self::Timeout => "timeout",
            Self::Transport => "transport",
            Self::BodyLimit => "body_limit",
            Self::Parse => "parse",
        }
    }
}

#[derive(Debug, Clone, Default)]
struct PromHistogram {
    buckets: BTreeMap<OrderedF64, f64>,
}

#[derive(Debug, Clone, Copy, Default)]
struct OrderedF64(f64);

impl PartialEq for OrderedF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0).is_eq()
    }
}
impl Eq for OrderedF64 {}
impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

#[derive(Debug, Clone)]
struct ParsedMetrics {
    engine: BackendEngineKind,
    coverage: BackendMetricsCoverage,
    instant: BackendInstantMetrics,
    process_start: Option<f64>,
    counters: BTreeMap<String, f64>,
    finish_reasons: BTreeMap<String, f64>,
    histograms: BTreeMap<String, PromHistogram>,
}

#[derive(Debug, Clone, Default)]
struct IntervalMetrics {
    duration_secs: f64,
    counters: BTreeMap<String, f64>,
    finish_reasons: BTreeMap<String, f64>,
    histograms: BTreeMap<String, PromHistogram>,
}

impl IntervalMetrics {
    fn merge(&mut self, other: &Self) {
        self.duration_secs += other.duration_secs;
        merge_numeric_map(&mut self.counters, &other.counters);
        merge_numeric_map(&mut self.finish_reasons, &other.finish_reasons);
        for (name, histogram) in &other.histograms {
            let target = self.histograms.entry(name.clone()).or_default();
            for (bound, count) in &histogram.buckets {
                *target.buckets.entry(*bound).or_default() += count;
            }
        }
    }
}

#[derive(Debug)]
struct EndpointState {
    targets: Vec<BackendMetricsTarget>,
    next_scrape: tokio::time::Instant,
    backoff: Duration,
    parsed: Option<ParsedMetrics>,
    scraped_at_ms: Option<u128>,
    last_success_at: Option<tokio::time::Instant>,
    last_success_ms: Option<u128>,
    last_error: Option<ScrapeErrorClass>,
    intervals_5s: VecDeque<(tokio::time::Instant, IntervalMetrics)>,
    intervals_1m: VecDeque<(tokio::time::Instant, IntervalMetrics)>,
    pending_minute: Option<(tokio::time::Instant, IntervalMetrics)>,
    reported_status: Option<BackendMetricsStatus>,
}

#[derive(Debug)]
struct EngineThroughputAccumulator {
    tpot_seconds_sum: f64,
    tpot_observations: f64,
    sampled_at_ms: Option<u128>,
    measured_sources: u64,
    total_sources: u64,
}

impl Default for EngineThroughputAccumulator {
    fn default() -> Self {
        Self {
            tpot_seconds_sum: 0.0,
            tpot_observations: 0.0,
            sampled_at_ms: None,
            measured_sources: 0,
            total_sources: 0,
        }
    }
}

impl EngineThroughputAccumulator {
    fn observe(&mut self, state: &EndpointState, metrics: &BackendProviderMetrics) {
        self.total_sources = self.total_sources.saturating_add(1);
        if metrics.status != BackendMetricsStatus::Fresh {
            return;
        }
        let Some((_, interval)) = state.intervals_5s.back() else {
            return;
        };
        let Some(tpot_seconds_sum) = interval.counters.get("request_tpot_seconds_sum").copied()
        else {
            return;
        };
        let Some(tpot_observations) = interval.counters.get("request_tpot_observations").copied()
        else {
            return;
        };
        let Some(sampled_at_ms) = state.last_success_ms else {
            return;
        };
        if !tpot_seconds_sum.is_finite()
            || !tpot_observations.is_finite()
            || tpot_seconds_sum <= 0.0
            || tpot_observations <= 0.0
        {
            return;
        }
        self.tpot_seconds_sum += tpot_seconds_sum;
        self.tpot_observations += tpot_observations;
        self.measured_sources = self.measured_sources.saturating_add(1);
        // The aggregate is only as fresh as its oldest contributing source.
        self.sampled_at_ms = Some(
            self.sampled_at_ms
                .map_or(sampled_at_ms, |oldest| oldest.min(sampled_at_ms)),
        );
    }

    fn finish(self) -> Option<EngineThroughputSample> {
        let sampled_at_ms = self.sampled_at_ms?;
        let generated_tokens_per_sec = self.tpot_observations / self.tpot_seconds_sum;
        if !generated_tokens_per_sec.is_finite() || generated_tokens_per_sec < 0.0 {
            return None;
        }
        Some(EngineThroughputSample {
            generated_tokens_per_sec,
            sampled_at_ms,
            measured_sources: self.measured_sources,
            total_sources: self.total_sources,
            // Coverage is specific to this rate: a source counts as measured only
            // when its fresh interval contains at least one completed TPOT observation.
            // The provider-wide coverage flag also reflects unrelated metric families
            // (and is always partial for SGLang), so folding it in would understate an
            // otherwise complete TPOT aggregate.
            coverage: if self.measured_sources == self.total_sources {
                BackendMetricsCoverage::Full
            } else {
                BackendMetricsCoverage::Partial
            },
        })
    }
}

impl EndpointState {
    fn new(targets: Vec<BackendMetricsTarget>) -> Self {
        Self {
            targets,
            next_scrape: tokio::time::Instant::now(),
            backoff: SCRAPE_INTERVAL,
            parsed: None,
            scraped_at_ms: None,
            last_success_at: None,
            last_success_ms: None,
            last_error: None,
            intervals_5s: VecDeque::new(),
            intervals_1m: VecDeque::new(),
            pending_minute: None,
            reported_status: None,
        }
    }

    fn record_success(&mut self, parsed: ParsedMetrics, at: tokio::time::Instant, at_ms: u128) {
        if let (Some(previous), Some(previous_at)) = (&self.parsed, self.last_success_at)
            && previous.process_start == parsed.process_start
        {
            let interval = interval_delta(previous, &parsed, at.duration_since(previous_at));
            if interval.duration_secs > 0.0 {
                self.intervals_5s.push_back((at, interval.clone()));
                while self.intervals_5s.len() > 60 {
                    self.intervals_5s.pop_front();
                }
                self.compact_minute(at, interval);
            }
        } else {
            self.intervals_5s.clear();
            self.intervals_1m.clear();
            self.pending_minute = None;
        }
        self.parsed = Some(parsed);
        self.scraped_at_ms = Some(at_ms);
        self.last_success_at = Some(at);
        self.last_success_ms = Some(at_ms);
        self.last_error = None;
        self.backoff = SCRAPE_INTERVAL;
        self.next_scrape = at + SCRAPE_INTERVAL;
    }

    fn compact_minute(&mut self, at: tokio::time::Instant, interval: IntervalMetrics) {
        match &mut self.pending_minute {
            Some((started, current)) if at.duration_since(*started) < Duration::from_secs(60) => {
                current.merge(&interval);
            }
            Some(_) => {
                let completed = self.pending_minute.take().expect("pending minute");
                self.intervals_1m.push_back(completed);
                while self.intervals_1m.len() > 60 {
                    self.intervals_1m.pop_front();
                }
                self.pending_minute = Some((at, interval));
            }
            None => self.pending_minute = Some((at, interval)),
        }
    }

    fn record_failure(&mut self, class: ScrapeErrorClass, at: tokio::time::Instant) {
        self.last_error = Some(class);
        let delay = match class {
            ScrapeErrorClass::Unsupported | ScrapeErrorClass::Authentication => LONG_RETRY,
            _ => {
                let delay = self
                    .backoff
                    .max(SCRAPE_INTERVAL)
                    .min(Duration::from_secs(60));
                self.backoff = (delay * 2).min(Duration::from_secs(60));
                delay
            }
        };
        self.next_scrape = at + delay + stable_jitter(&self.targets[0].physical_key());
    }

    fn logical_metrics(&self, now: tokio::time::Instant) -> BackendProviderMetrics {
        let parsed = self.parsed.as_ref();
        let stale = self
            .last_success_at
            .is_some_and(|at| now.duration_since(at) > STALE_AFTER);
        let status = if self.last_success_at.is_some() {
            if stale {
                BackendMetricsStatus::Stale
            } else if self.intervals_5s.is_empty() {
                BackendMetricsStatus::Warming
            } else {
                BackendMetricsStatus::Fresh
            }
        } else if self.last_error == Some(ScrapeErrorClass::Unsupported) {
            BackendMetricsStatus::Unsupported
        } else {
            BackendMetricsStatus::Error
        };
        BackendProviderMetrics {
            engine_kind: parsed.map_or(BackendEngineKind::Unknown, |p| p.engine),
            status,
            coverage: parsed.map_or(BackendMetricsCoverage::Partial, |p| p.coverage),
            scraped_at_ms: self.scraped_at_ms,
            last_success_ms: self.last_success_ms,
            last_error_class: self.last_error.map(|e| e.as_str().to_string()),
            instant: parsed.map_or_else(BackendInstantMetrics::default, |p| p.instant.clone()),
            windows: BackendMetricWindows {
                m1: window_from_intervals(self.intervals_5s.iter().rev().take(12).map(|(_, v)| v)),
                m5: window_from_intervals(self.intervals_5s.iter().map(|(_, v)| v)),
                h1: window_from_intervals(
                    self.intervals_1m
                        .iter()
                        .map(|(_, v)| v)
                        .chain(self.pending_minute.iter().map(|(_, v)| v)),
                ),
            },
        }
    }
}

pub fn collection_enabled(with_debug_ui: bool) -> bool {
    with_debug_ui
        && !std::env::var(BACKEND_METRICS_ENV)
            .ok()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("off"))
}

pub fn spawn_backend_metrics_collector(
    store: BackendMetricsStore,
    targets: Vec<BackendMetricsTarget>,
) -> Option<tokio::task::JoinHandle<()>> {
    if !store.is_enabled() || targets.is_empty() {
        return None;
    }
    let mut grouped = BTreeMap::<PhysicalKey, Vec<BackendMetricsTarget>>::new();
    let mut logical_count = 0usize;
    let mut overflow_count = 0u64;
    for target in targets {
        if logical_count >= MAX_LOGICAL_PROVIDERS {
            overflow_count = overflow_count.saturating_add(1);
            continue;
        }
        logical_count += 1;
        grouped
            .entry(target.physical_key())
            .or_default()
            .push(target);
    }
    Some(tokio::spawn(async move {
        let mut states = grouped
            .into_iter()
            .map(|(key, targets)| (key, EndpointState::new(targets)))
            .collect::<BTreeMap<_, _>>();
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let now = tokio::time::Instant::now();
            let due = states
                .iter()
                .filter(|(_, state)| state.next_scrape <= now)
                .map(|(key, state)| (key.clone(), state.targets[0].clone()))
                .collect::<Vec<_>>();
            let results = futures::stream::iter(due.into_iter().map(|(key, target)| async move {
                let result = scrape_target(&target).await;
                (key, result)
            }))
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await;
            for (key, result) in results {
                let at = tokio::time::Instant::now();
                let state = states.get_mut(&key).expect("due state still exists");
                match result {
                    Ok(parsed) => {
                        state.record_success(parsed, at, now_ms());
                    }
                    Err(class) => {
                        state.record_failure(class, at);
                        tracing::debug!(provider = %state.targets[0].provider_id, error_class = class.as_str(), "backend metrics scrape failed");
                    }
                }
            }
            let now = tokio::time::Instant::now();
            let mut providers = BTreeMap::new();
            let mut throughput = EngineThroughputAccumulator::default();
            for state in states.values_mut() {
                let metrics = state.logical_metrics(now);
                throughput.observe(state, &metrics);
                if state.reported_status != Some(metrics.status) {
                    match metrics.status {
                        BackendMetricsStatus::Fresh => {
                            tracing::info!(provider = %state.targets[0].provider_id, "backend metrics available")
                        }
                        BackendMetricsStatus::Warming => {
                            tracing::info!(provider = %state.targets[0].provider_id, "backend metrics warming")
                        }
                        BackendMetricsStatus::Stale => {
                            tracing::warn!(provider = %state.targets[0].provider_id, "backend metrics stale")
                        }
                        BackendMetricsStatus::Unsupported => {
                            tracing::info!(provider = %state.targets[0].provider_id, "backend metrics unsupported")
                        }
                        BackendMetricsStatus::Error => {
                            tracing::warn!(provider = %state.targets[0].provider_id, error_class = metrics.last_error_class.as_deref().unwrap_or("unavailable"), "backend metrics unavailable")
                        }
                    }
                    state.reported_status = Some(metrics.status);
                }
                for target in &state.targets {
                    providers.insert(target.logical_key(), metrics.clone());
                }
            }
            store.publish(BackendMetricsSnapshot {
                seq: 0,
                generated_at_ms: now_ms(),
                providers,
                overflow_count,
                engine_throughput: throughput.finish(),
            });
        }
    }))
}

async fn scrape_target(target: &BackendMetricsTarget) -> Result<ParsedMetrics, ScrapeErrorClass> {
    tokio::time::timeout(SCRAPE_TIMEOUT, scrape_target_inner(target))
        .await
        .map_err(|_| ScrapeErrorClass::Timeout)?
}

async fn scrape_target_inner(
    target: &BackendMetricsTarget,
) -> Result<ParsedMetrics, ScrapeErrorClass> {
    let request = match &target.api_key {
        Some(key) => target
            .client
            .get(target.metrics_url.clone())
            .bearer_auth(key),
        None => target.client.get(target.metrics_url.clone()),
    };
    let response = request.send().await.map_err(|error| {
        if error.is_timeout() {
            ScrapeErrorClass::Timeout
        } else {
            ScrapeErrorClass::Transport
        }
    })?;
    match response.status() {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            return Err(ScrapeErrorClass::Authentication);
        }
        StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED | StatusCode::NOT_IMPLEMENTED => {
            return Err(ScrapeErrorClass::Unsupported);
        }
        StatusCode::TOO_MANY_REQUESTS => return Err(ScrapeErrorClass::RateLimited),
        status if status.is_server_error() => return Err(ScrapeErrorClass::Http5xx),
        status if !status.is_success() => return Err(ScrapeErrorClass::Parse),
        _ => {}
    }
    let mut bytes = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| ScrapeErrorClass::Transport)?;
        if bytes.len().saturating_add(chunk.len()) > BODY_LIMIT {
            return Err(ScrapeErrorClass::BodyLimit);
        }
        bytes.extend_from_slice(&chunk);
    }
    parse_metrics(&bytes)
}

/// Preserve the TPOT histogram's exact sum/count through `prometheus-parse`, which
/// otherwise keeps only its buckets. Aliases use `\w` exclusively because that parser
/// rejects the `:` accepted by Prometheus and used by vLLM/SGLang.
fn normalize_exposition_line(line: &str) -> String {
    let mut normalized = line.replace("vllm:", "vllm_").replace("sglang:", "sglang_");
    let metric_end = normalized
        .find(|ch: char| ch == '{' || ch.is_ascii_whitespace())
        .unwrap_or(normalized.len());
    let alias = match &normalized[..metric_end] {
        "vllm_request_time_per_output_token_seconds_sum"
        | "vllm_time_per_output_token_seconds_sum"
        | "sglang_time_per_output_token_seconds_sum" => Some("llmconduit_request_tpot_seconds_sum"),
        "vllm_request_time_per_output_token_seconds_count"
        | "vllm_time_per_output_token_seconds_count"
        | "sglang_time_per_output_token_seconds_count" => {
            Some("llmconduit_request_tpot_observations")
        }
        _ => None,
    };
    if let Some(alias) = alias {
        normalized.replace_range(..metric_end, alias);
    }
    normalized
}

fn parse_metrics(body: &[u8]) -> Result<ParsedMetrics, ScrapeErrorClass> {
    let text = std::str::from_utf8(body).map_err(|_| ScrapeErrorClass::Parse)?;
    if text.lines().count() > SAMPLE_LIMIT.saturating_mul(4) {
        return Err(ScrapeErrorClass::Parse);
    }
    // prometheus-parse 0.2.5 accepts only `\w` metric names even though the
    // Prometheus grammar (and both engines) permits `:`. It also intentionally drops
    // histogram `_sum`/`_count` samples. Normalize the namespaces and rename only the
    // TPOT components to untyped synthetic metrics before parsing, then restore the
    // engine namespaces. The crate still owns numeric/label parsing; this shim merely
    // prevents the two exact samples needed for inverse-mean TPOT from being discarded.
    let mut scrape = Scrape::parse(
        text.lines()
            .map(|line| Ok::<_, io::Error>(normalize_exposition_line(line))),
    )
    .map_err(|_| ScrapeErrorClass::Parse)?;
    for sample in &mut scrape.samples {
        if let Some(rest) = sample.metric.strip_prefix("vllm_") {
            sample.metric = format!("vllm:{rest}");
        } else if let Some(rest) = sample.metric.strip_prefix("sglang_") {
            sample.metric = format!("sglang:{rest}");
        }
    }
    if scrape.samples.len() > SAMPLE_LIMIT {
        return Err(ScrapeErrorClass::Parse);
    }
    let engine = if scrape
        .samples
        .iter()
        .any(|sample| sample.metric.starts_with("vllm:"))
    {
        BackendEngineKind::Vllm
    } else if scrape
        .samples
        .iter()
        .any(|sample| sample.metric.starts_with("sglang:"))
    {
        BackendEngineKind::Sglang
    } else {
        return Err(ScrapeErrorClass::Unsupported);
    };
    let mut parsed = ParsedMetrics {
        engine,
        coverage: if engine == BackendEngineKind::Vllm {
            BackendMetricsCoverage::Full
        } else {
            BackendMetricsCoverage::Partial
        },
        instant: BackendInstantMetrics::default(),
        process_start: None,
        counters: BTreeMap::new(),
        finish_reasons: BTreeMap::new(),
        histograms: BTreeMap::new(),
    };
    let mut kv_values = Vec::new();
    for sample in scrape.samples {
        if sample
            .labels
            .iter()
            .any(|(key, value)| key.len() > LABEL_LIMIT || value.len() > LABEL_LIMIT)
        {
            continue;
        }
        let scalar = match sample.value {
            Value::Counter(v) | Value::Gauge(v) | Value::Untyped(v) if v.is_finite() => Some(v),
            _ => None,
        };
        match sample.metric.as_str() {
            "process_start_time_seconds" => parsed.process_start = scalar,
            "vllm:num_requests_running" | "sglang:num_running_reqs" => {
                sum_option(&mut parsed.instant.running_requests, scalar)
            }
            "vllm:num_requests_waiting" | "sglang:num_queue_reqs" => {
                sum_option(&mut parsed.instant.waiting_requests, scalar)
            }
            "vllm:kv_cache_usage_perc" | "vllm:gpu_cache_usage_perc" | "sglang:token_usage" => {
                if let Some(v) = scalar {
                    kv_values.push(v.clamp(0.0, 1.0));
                }
            }
            "vllm:kv_cache_tokens_capacity" | "sglang:max_total_num_tokens" => {
                sum_option(&mut parsed.instant.kv_cache_token_capacity, scalar)
            }
            "vllm:engine_sleep_state" => {
                if let Some(v) = scalar {
                    parsed.instant.engine_sleeping =
                        Some(parsed.instant.engine_sleeping.unwrap_or(false) || v > 0.0);
                }
            }
            "vllm:prompt_tokens_total" | "sglang:prompt_tokens_total" => {
                add_counter(&mut parsed.counters, "prompt", scalar)
            }
            "vllm:generation_tokens_total" | "sglang:generation_tokens_total" => {
                add_counter(&mut parsed.counters, "generated", scalar)
            }
            "llmconduit_request_tpot_seconds_sum" => {
                add_counter(&mut parsed.counters, "request_tpot_seconds_sum", scalar)
            }
            "llmconduit_request_tpot_observations" => {
                add_counter(&mut parsed.counters, "request_tpot_observations", scalar)
            }
            "vllm:prefix_cache_queries"
            | "vllm:gpu_prefix_cache_queries"
            | "sglang:prefix_cache_queries_total" => {
                add_counter(&mut parsed.counters, "prefix_queries", scalar)
            }
            "vllm:prefix_cache_hits"
            | "vllm:gpu_prefix_cache_hits"
            | "sglang:prefix_cache_hits_total" => {
                add_counter(&mut parsed.counters, "prefix_hits", scalar)
            }
            "vllm:num_preemptions_total" | "sglang:num_retracted_requests_total" => {
                add_counter(&mut parsed.counters, "preemptions", scalar)
            }
            "vllm:spec_decode_num_draft_tokens_total" | "sglang:spec_draft_tokens_total" => {
                add_counter(&mut parsed.counters, "draft", scalar)
            }
            "vllm:spec_decode_num_accepted_tokens_total" | "sglang:spec_accepted_tokens_total" => {
                add_counter(&mut parsed.counters, "accepted", scalar)
            }
            "vllm:request_success_total" | "sglang:request_success_total" => {
                add_counter(&mut parsed.counters, "completed", scalar);
                if let Some(value) = scalar {
                    let reason = bounded_reason(
                        sample
                            .labels
                            .get("finished_reason")
                            .or_else(|| sample.labels.get("finish_reason")),
                    );
                    *parsed.finish_reasons.entry(reason.to_string()).or_default() += value;
                }
            }
            _ => {}
        }
        if let Value::Histogram(buckets) = sample.value
            && let Some(name) = histogram_name(&sample.metric)
        {
            let target = parsed.histograms.entry(name.to_string()).or_default();
            for bucket in buckets {
                if bucket.less_than.is_finite() && bucket.count.is_finite() && bucket.count >= 0.0 {
                    *target
                        .buckets
                        .entry(OrderedF64(bucket.less_than))
                        .or_default() += bucket.count;
                } else if bucket.less_than.is_infinite()
                    && bucket.less_than.is_sign_positive()
                    && bucket.count.is_finite()
                {
                    *target.buckets.entry(OrderedF64(f64::INFINITY)).or_default() +=
                        bucket.count.max(0.0);
                }
            }
        }
    }
    parsed.instant.kv_cache_utilization = kv_values.into_iter().reduce(f64::max);
    Ok(parsed)
}

fn interval_delta(
    previous: &ParsedMetrics,
    current: &ParsedMetrics,
    duration: Duration,
) -> IntervalMetrics {
    let mut interval = IntervalMetrics {
        duration_secs: duration.as_secs_f64(),
        ..Default::default()
    };
    delta_map(
        &previous.counters,
        &current.counters,
        &mut interval.counters,
    );
    delta_map(
        &previous.finish_reasons,
        &current.finish_reasons,
        &mut interval.finish_reasons,
    );
    for (name, current_hist) in &current.histograms {
        let Some(previous_hist) = previous.histograms.get(name) else {
            continue;
        };
        if current_hist.buckets.iter().any(|(bound, count)| {
            previous_hist
                .buckets
                .get(bound)
                .is_some_and(|old| count < old)
        }) {
            continue;
        }
        let mut delta = PromHistogram::default();
        for (bound, count) in &current_hist.buckets {
            let value = count - previous_hist.buckets.get(bound).copied().unwrap_or(*count);
            delta.buckets.insert(*bound, value.max(0.0));
        }
        interval.histograms.insert(name.clone(), delta);
    }
    interval
}

fn window_from_intervals<'a>(
    intervals: impl Iterator<Item = &'a IntervalMetrics>,
) -> BackendMetricsWindow {
    let mut aggregate = IntervalMetrics::default();
    let mut samples = 0u64;
    for interval in intervals {
        aggregate.merge(interval);
        samples += 1;
    }
    if samples == 0 || aggregate.duration_secs <= 0.0 {
        return BackendMetricsWindow::default();
    }
    let rate = |name: &str| {
        aggregate
            .counters
            .get(name)
            .map(|value| value / aggregate.duration_secs)
    };
    let prefix_queries = aggregate.counters.get("prefix_queries").copied();
    let prefix_hits = aggregate.counters.get("prefix_hits").copied();
    let draft = aggregate.counters.get("draft").copied();
    let accepted = aggregate.counters.get("accepted").copied();
    let hist = |name: &str, scale: f64| {
        aggregate
            .histograms
            .get(name)
            .and_then(|h| summarize_histogram(h, scale))
    };
    BackendMetricsWindow {
        samples,
        prompt_tokens_per_sec: rate("prompt"),
        cached_prompt_tokens_per_sec: rate("prefix_hits"),
        generated_tokens_per_sec: rate("generated"),
        completed_requests_per_sec: rate("completed"),
        preemptions_per_sec: rate("preemptions"),
        finish_reasons: aggregate
            .finish_reasons
            .iter()
            .map(|(k, v)| (k.clone(), (*v).max(0.0) as u64))
            .collect(),
        prefix_cache_hit_ratio: ratio(prefix_hits, prefix_queries),
        prompt_token_cache_hit_ratio: ratio(prefix_hits, aggregate.counters.get("prompt").copied()),
        speculative_draft_tokens_per_sec: draft.map(|v| v / aggregate.duration_secs),
        speculative_accepted_tokens_per_sec: accepted.map(|v| v / aggregate.duration_secs),
        speculative_acceptance_ratio: ratio(accepted, draft),
        histograms: BackendLatencyMetrics {
            ttft_ms: hist("ttft", 1000.0),
            inter_token_ms: hist("inter_token", 1000.0),
            time_per_output_token_ms: hist("tpot", 1000.0),
            end_to_end_ms: hist("e2e", 1000.0),
            queue_ms: hist("queue", 1000.0),
            inference_ms: hist("inference", 1000.0),
            prefill_ms: hist("prefill", 1000.0),
            decode_ms: hist("decode", 1000.0),
            prompt_tokens: hist("prompt_tokens", 1.0),
            generation_tokens: hist("generation_tokens", 1.0),
            iteration_tokens: hist("iteration_tokens", 1.0),
        },
    }
}

fn summarize_histogram(histogram: &PromHistogram, scale: f64) -> Option<BackendHistogramSummary> {
    let total = histogram
        .buckets
        .get(&OrderedF64(f64::INFINITY))
        .copied()
        .or_else(|| histogram.buckets.values().next_back().copied())?;
    if total <= 0.0 {
        return None;
    }
    Some(BackendHistogramSummary {
        samples: total as u64,
        p50: histogram_quantile(histogram, 0.50).map(|v| v * scale),
        p95: histogram_quantile(histogram, 0.95).map(|v| v * scale),
        p99: histogram_quantile(histogram, 0.99).map(|v| v * scale),
        quantile_method: "prometheus_histogram_derived".to_string(),
    })
}

fn histogram_quantile(histogram: &PromHistogram, quantile: f64) -> Option<f64> {
    let total = histogram
        .buckets
        .get(&OrderedF64(f64::INFINITY))
        .copied()
        .or_else(|| histogram.buckets.values().next_back().copied())?;
    if total <= 0.0 {
        return None;
    }
    let rank = quantile * total;
    let mut lower_bound = 0.0;
    let mut lower_count = 0.0;
    for (bound, count) in &histogram.buckets {
        if bound.0.is_infinite() {
            return Some(lower_bound);
        }
        if *count >= rank {
            let bucket_count = (*count - lower_count).max(0.0);
            if bucket_count == 0.0 {
                return Some(bound.0);
            }
            let fraction = ((rank - lower_count) / bucket_count).clamp(0.0, 1.0);
            return Some(lower_bound + (bound.0 - lower_bound) * fraction);
        }
        lower_bound = bound.0;
        lower_count = *count;
    }
    None
}

fn histogram_name(metric: &str) -> Option<&'static str> {
    match metric {
        "vllm:time_to_first_token_seconds" | "sglang:time_to_first_token_seconds" => Some("ttft"),
        "vllm:inter_token_latency_seconds" | "sglang:inter_token_latency_seconds" => {
            Some("inter_token")
        }
        "vllm:time_per_output_token_seconds"
        | "vllm:request_time_per_output_token_seconds"
        | "sglang:time_per_output_token_seconds" => Some("tpot"),
        "vllm:e2e_request_latency_seconds" | "sglang:e2e_request_latency_seconds" => Some("e2e"),
        "vllm:request_queue_time_seconds" | "sglang:request_queue_time_seconds" => Some("queue"),
        "vllm:request_inference_time_seconds" | "sglang:request_inference_time_seconds" => {
            Some("inference")
        }
        "vllm:request_prefill_time_seconds" | "sglang:request_prefill_time_seconds" => {
            Some("prefill")
        }
        "vllm:request_decode_time_seconds" | "sglang:request_decode_time_seconds" => Some("decode"),
        "vllm:request_prompt_tokens" | "sglang:request_prompt_tokens" => Some("prompt_tokens"),
        "vllm:request_generation_tokens" | "sglang:request_generation_tokens" => {
            Some("generation_tokens")
        }
        "vllm:iteration_tokens_total" | "sglang:iteration_tokens_total" => Some("iteration_tokens"),
        _ => None,
    }
}

fn bounded_reason(reason: Option<&str>) -> &'static str {
    match reason.unwrap_or_default().to_ascii_lowercase().as_str() {
        "stop" | "eos" => "stop",
        "length" | "max_tokens" => "length",
        "abort" | "cancelled" => "cancelled",
        "error" => "error",
        _ => "other",
    }
}
fn add_counter(map: &mut BTreeMap<String, f64>, name: &str, value: Option<f64>) {
    if let Some(v) = value {
        *map.entry(name.to_string()).or_default() += v;
    }
}
fn sum_option(target: &mut Option<f64>, value: Option<f64>) {
    if let Some(v) = value {
        *target.get_or_insert(0.0) += v;
    }
}
fn merge_numeric_map(target: &mut BTreeMap<String, f64>, source: &BTreeMap<String, f64>) {
    for (k, v) in source {
        *target.entry(k.clone()).or_default() += v;
    }
}
fn delta_map(
    previous: &BTreeMap<String, f64>,
    current: &BTreeMap<String, f64>,
    target: &mut BTreeMap<String, f64>,
) {
    for (k, v) in current {
        if let Some(old) = previous.get(k)
            && v >= old
        {
            target.insert(k.clone(), v - old);
        }
    }
}
fn ratio(numerator: Option<f64>, denominator: Option<f64>) -> Option<f64> {
    match (numerator, denominator) {
        (Some(n), Some(d)) if d > 0.0 => Some((n / d).clamp(0.0, 1.0)),
        _ => None,
    }
}
fn stable_jitter(key: &PhysicalKey) -> Duration {
    let digest = Sha256::digest(format!("{}:{:?}", key.url, key.credential_identity));
    Duration::from_millis(u64::from(digest[0]) * 2)
}
fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const VLLM: &str = r#"
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running{model_name="a"} 2
vllm:num_requests_running{model_name="b"} 1
# TYPE vllm:num_requests_waiting gauge
vllm:num_requests_waiting 4
# TYPE vllm:kv_cache_usage_perc gauge
vllm:kv_cache_usage_perc{engine="0"} 0.5
vllm:kv_cache_usage_perc{engine="1"} 0.75
# TYPE vllm:generation_tokens_total counter
vllm:generation_tokens_total 100
# TYPE vllm:request_time_per_output_token_seconds histogram
vllm:request_time_per_output_token_seconds_bucket{le="0.01"} 1
vllm:request_time_per_output_token_seconds_bucket{le="+Inf"} 1
vllm:request_time_per_output_token_seconds_sum{engine="0",model_name="a"} 0.006666666666666667
vllm:request_time_per_output_token_seconds_count{engine="0",model_name="a"} 1
# TYPE vllm:time_to_first_token_seconds histogram
vllm:time_to_first_token_seconds_bucket{le="0.1"} 5
vllm:time_to_first_token_seconds_bucket{le="0.5"} 10
vllm:time_to_first_token_seconds_bucket{le="+Inf"} 10
vllm:time_to_first_token_seconds_count 10
"#;

    #[test]
    fn parses_vllm_and_aggregates_labels() {
        let parsed = parse_metrics(VLLM.as_bytes()).unwrap();
        assert_eq!(parsed.engine, BackendEngineKind::Vllm);
        assert_eq!(parsed.instant.running_requests, Some(3.0));
        assert_eq!(parsed.instant.kv_cache_utilization, Some(0.75));
        assert_eq!(parsed.counters.get("generated"), Some(&100.0));
        assert_eq!(
            parsed.counters.get("request_tpot_seconds_sum"),
            Some(&0.006666666666666667)
        );
        assert_eq!(parsed.counters.get("request_tpot_observations"), Some(&1.0));
        assert_eq!(parsed.histograms["tpot"].buckets[&OrderedF64(0.01)], 1.0);
        assert_eq!(parsed.histograms["ttft"].buckets[&OrderedF64(0.5)], 10.0);
    }

    #[test]
    fn unknown_schema_is_unsupported() {
        assert_eq!(
            parse_metrics(b"python_gc_objects 1\n").unwrap_err(),
            ScrapeErrorClass::Unsupported
        );
    }

    #[test]
    fn deltas_never_emit_negative_rates_and_quantiles_interpolate() {
        let first = parse_metrics(VLLM.as_bytes()).unwrap();
        let second_text = VLLM
            .replace("generation_tokens_total 100", "generation_tokens_total 140")
            .replace(
                "request_time_per_output_token_seconds_bucket{le=\"0.01\"} 1",
                "request_time_per_output_token_seconds_bucket{le=\"0.01\"} 2",
            )
            .replace(
                "request_time_per_output_token_seconds_bucket{le=\"+Inf\"} 1",
                "request_time_per_output_token_seconds_bucket{le=\"+Inf\"} 2",
            )
            .replace(
                "request_time_per_output_token_seconds_sum{engine=\"0\",model_name=\"a\"} 0.006666666666666667",
                "request_time_per_output_token_seconds_sum{engine=\"0\",model_name=\"a\"} 0.013333333333333334",
            )
            .replace(
                "request_time_per_output_token_seconds_count{engine=\"0\",model_name=\"a\"} 1",
                "request_time_per_output_token_seconds_count{engine=\"0\",model_name=\"a\"} 2",
            )
            .replace("bucket{le=\"0.1\"} 5", "bucket{le=\"0.1\"} 7")
            .replace("bucket{le=\"0.5\"} 10", "bucket{le=\"0.5\"} 14")
            .replace("bucket{le=\"+Inf\"} 10", "bucket{le=\"+Inf\"} 14");
        let second = parse_metrics(second_text.as_bytes()).unwrap();
        let interval = interval_delta(&first, &second, Duration::from_secs(5));
        let window = window_from_intervals(std::iter::once(&interval));
        assert_eq!(window.generated_tokens_per_sec, Some(8.0));
        let tpot_rate = interval.counters["request_tpot_observations"]
            / interval.counters["request_tpot_seconds_sum"];
        assert!((tpot_rate - 150.0).abs() < 1e-9);
        assert!(window.histograms.ttft_ms.unwrap().p95.unwrap() <= 500.0);
    }

    #[test]
    fn missing_families_remain_absent() {
        let parsed =
            parse_metrics(b"# TYPE sglang:num_running_reqs gauge\nsglang:num_running_reqs 2\n")
                .unwrap();
        assert_eq!(parsed.engine, BackendEngineKind::Sglang);
        assert_eq!(parsed.instant.waiting_requests, None);
        assert!(
            window_from_intervals(std::iter::empty())
                .generated_tokens_per_sec
                .is_none()
        );
    }

    #[test]
    fn parses_sglang_tpot_sum_and_count_aliases() {
        let parsed = parse_metrics(
            br#"
# TYPE sglang:time_per_output_token_seconds histogram
sglang:time_per_output_token_seconds_bucket{le="0.01"} 3
sglang:time_per_output_token_seconds_bucket{le="+Inf"} 3
sglang:time_per_output_token_seconds_sum 0.02
sglang:time_per_output_token_seconds_count 3
"#,
        )
        .unwrap();
        assert_eq!(parsed.engine, BackendEngineKind::Sglang);
        assert_eq!(parsed.counters["request_tpot_seconds_sum"], 0.02);
        assert_eq!(parsed.counters["request_tpot_observations"], 3.0);
    }

    fn endpoint_with_tpot_interval(
        rate: f64,
        observations: u64,
        coverage: BackendMetricsCoverage,
        sampled_at_ms: u128,
        aliases: usize,
    ) -> EndpointState {
        let client = Client::new();
        let url: Url = "http://localhost:8000/metrics".parse().unwrap();
        let targets = (0..aliases)
            .map(|index| {
                BackendMetricsTarget::new(
                    Some(format!("route-{index}")),
                    "shared-engine".to_string(),
                    url.clone(),
                    client.clone(),
                    None,
                )
            })
            .collect();
        let now = tokio::time::Instant::now();
        let mut state = EndpointState::new(targets);
        state.parsed = Some(ParsedMetrics {
            engine: BackendEngineKind::Vllm,
            coverage,
            instant: BackendInstantMetrics::default(),
            process_start: Some(1.0),
            counters: BTreeMap::new(),
            finish_reasons: BTreeMap::new(),
            histograms: BTreeMap::new(),
        });
        state.last_success_at = Some(now);
        state.last_success_ms = Some(sampled_at_ms);
        state.intervals_5s.push_back((
            now,
            IntervalMetrics {
                duration_secs: 5.0,
                counters: BTreeMap::from([
                    (
                        "request_tpot_seconds_sum".to_string(),
                        observations as f64 / rate,
                    ),
                    ("request_tpot_observations".to_string(), observations as f64),
                ]),
                ..Default::default()
            },
        ));
        state
    }

    #[test]
    fn engine_throughput_counts_each_physical_endpoint_once_before_alias_fanout() {
        let shared = endpoint_with_tpot_interval(150.0, 2, BackendMetricsCoverage::Full, 12_000, 2);
        let now = tokio::time::Instant::now();
        let metrics = shared.logical_metrics(now);
        let mut aggregate = EngineThroughputAccumulator::default();
        // One observation represents the collector's physical-state loop; the two
        // logical targets are fanned out only after this aggregation seam.
        aggregate.observe(&shared, &metrics);
        let sample = aggregate.finish().expect("TPOT interval is measurable");
        assert!((sample.generated_tokens_per_sec - 150.0).abs() < 1e-9);
        assert_eq!(sample.measured_sources, 1);
        assert_eq!(sample.total_sources, 1);
        assert_eq!(sample.coverage, BackendMetricsCoverage::Full);
    }

    #[test]
    fn engine_throughput_uses_tpot_not_idle_time_in_the_scrape_interval() {
        let mut state =
            endpoint_with_tpot_interval(149.011, 1, BackendMetricsCoverage::Full, 12_000, 1);
        let interval = &mut state.intervals_5s.back_mut().unwrap().1;
        interval.duration_secs = 6.0;
        interval.counters.insert("generated".to_string(), 256.0);
        let now = tokio::time::Instant::now();
        let mut aggregate = EngineThroughputAccumulator::default();
        aggregate.observe(&state, &state.logical_metrics(now));
        let sample = aggregate.finish().expect("TPOT interval is measurable");
        assert!((sample.generated_tokens_per_sec - 149.011).abs() < 1e-9);
        assert!(
            (sample.generated_tokens_per_sec - (256.0 / 6.0)).abs() > 100.0,
            "scrape-window idle time must not dilute active decode throughput"
        );
    }

    #[test]
    fn engine_throughput_combines_tpot_observations_and_marks_incomplete_coverage() {
        let full = endpoint_with_tpot_interval(150.0, 2, BackendMetricsCoverage::Full, 12_000, 1);
        let partial =
            endpoint_with_tpot_interval(100.0, 1, BackendMetricsCoverage::Partial, 11_500, 1);
        let mut unavailable =
            endpoint_with_tpot_interval(99.0, 1, BackendMetricsCoverage::Full, 11_000, 1);
        let now = tokio::time::Instant::now();
        unavailable.last_success_at = Some(now - STALE_AFTER - Duration::from_secs(1));

        let mut aggregate = EngineThroughputAccumulator::default();
        for state in [&full, &partial, &unavailable] {
            aggregate.observe(state, &state.logical_metrics(now));
        }
        let sample = aggregate.finish().expect("two sources are measurable");
        let expected = 3.0 / (2.0 / 150.0 + 1.0 / 100.0);
        assert!((sample.generated_tokens_per_sec - expected).abs() < 1e-9);
        assert_eq!(sample.sampled_at_ms, 11_500);
        assert_eq!(sample.measured_sources, 2);
        assert_eq!(sample.total_sources, 3);
        assert_eq!(sample.coverage, BackendMetricsCoverage::Partial);
    }

    #[tokio::test]
    async fn collector_scrapes_once_per_physical_endpoint_and_fans_out_logical_providers() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/metrics"))
            .and(header("authorization", "Bearer secret"))
            .respond_with(ResponseTemplate::new(200).set_body_string(VLLM))
            .expect(1)
            .mount(&server)
            .await;
        let client = Client::new();
        let url: Url = format!("{}/metrics", server.uri()).parse().unwrap();
        let targets = vec![
            BackendMetricsTarget::new(
                Some("route-a".to_string()),
                "primary".to_string(),
                url.clone(),
                client.clone(),
                Some("secret".to_string()),
            ),
            BackendMetricsTarget::new(
                Some("route-b".to_string()),
                "primary".to_string(),
                url,
                client,
                Some("secret".to_string()),
            ),
        ];
        let store = BackendMetricsStore::new();
        let task = spawn_backend_metrics_collector(store.clone(), targets).unwrap();
        for _ in 0..50 {
            if store.latest().providers.len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let snapshot = store.latest();
        task.abort();
        assert_eq!(snapshot.providers.len(), 2);
        assert!(snapshot.provider(Some("route-a"), "primary").is_some());
        assert!(snapshot.provider(Some("route-b"), "primary").is_some());
        assert_eq!(
            snapshot
                .provider(Some("route-a"), "primary")
                .unwrap()
                .status,
            BackendMetricsStatus::Warming
        );
    }

    #[test]
    fn env_gate_requires_debug_ui_and_honors_off() {
        unsafe { std::env::remove_var(BACKEND_METRICS_ENV) };
        assert!(!collection_enabled(false));
        assert!(collection_enabled(true));
        unsafe { std::env::set_var(BACKEND_METRICS_ENV, "off") };
        assert!(!collection_enabled(true));
        unsafe { std::env::remove_var(BACKEND_METRICS_ENV) };
    }
}
