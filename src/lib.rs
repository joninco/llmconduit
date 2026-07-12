pub mod adapters;
pub mod backend_metrics;
pub mod cli;
pub mod config;
pub mod dashboard_api;
pub mod dashboard_auth;
pub mod dashboard_contracts;
pub mod dashboard_flow;
pub mod dashboard_history;
pub mod dashboard_ui;
pub mod dashboard_ws;
pub mod debug_ui;
pub mod engine;
pub mod error;
pub mod http;
pub mod log_rotation;
pub mod metrics;
pub mod models;
pub mod monitor;
pub(crate) mod proxy_headers;
pub mod raw;
pub(crate) mod redaction;
pub mod replay;
pub mod request_log;
pub mod search;
pub(crate) mod sse_guard;
/// Crate-internal, test-only peak-allocation probe (the crate's single
/// `#[global_allocator]`), shared by the `sse_guard` reject-path and
/// `dashboard_flow::capture_body` heap-bound tests.
#[cfg(test)]
pub(crate) mod test_alloc_probe;
pub(crate) mod tool_delta_gate;
pub mod turn_capture;
pub mod upstream;
pub mod vision;

/// Build provenance, embedded at compile time by `build.rs`: git short commit,
/// working-tree dirty flag, and UTC build timestamp. Surfaced in `--version`
/// (see [`VERSION`]) and the startup log so a running process is traceable to
/// the exact source commit it was built from.
pub const GIT_HASH: &str = env!("LLMCONDUIT_GIT_HASH");
/// `"true"` if the working tree had uncommitted changes at build time.
pub const GIT_DIRTY: &str = env!("LLMCONDUIT_GIT_DIRTY");
/// UTC build timestamp (`YYYY-MM-DDTHH:MM:SSZ`), or `unknown`.
pub const BUILD_TIME: &str = env!("LLMCONDUIT_BUILD_TIME");
/// `--version` string: `"<semver> (<short-hash>, <build-time>)"`.
pub const VERSION: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("LLMCONDUIT_GIT_HASH"),
    ", ",
    env!("LLMCONDUIT_BUILD_TIME"),
    ")"
);

use crate::config::Config;
use crate::engine::Gateway;
use crate::http::RouterOptions;
use crate::http::build_router;
use crate::monitor::MonitorHub;
use crate::raw::RawOutput;
use crate::replay::ReplayStore;
use crate::search::BraveSearchClient;
use crate::upstream::FailoverUpstreamClient;
use crate::upstream::FailoverUpstreamProvider;
use crate::upstream::ModelRouteSpec;
use crate::upstream::ReqwestUpstreamClient;
use crate::upstream::RouteUpstreamProvider;
use crate::upstream::RoutingUpstreamClient;
use crate::upstream::RoutingUpstreamProvider;
use crate::vision::ImageCache;
use crate::vision::ReqwestVisionClient;
use std::sync::Arc;
use std::time::Duration;

pub fn build_app(config: Config) -> axum::Router {
    build_app_with_gateway(config).0
}

pub fn build_app_with_options(config: Config, options: AppOptions) -> axum::Router {
    build_app_with_gateway_and_options(config, None, options).0
}

pub fn build_app_with_gateway(config: Config) -> (axum::Router, Arc<Gateway>) {
    build_app_with_gateway_and_raw_output(config, None)
}

pub fn build_app_with_gateway_and_raw_output(
    config: Config,
    raw_output: Option<RawOutput>,
) -> (axum::Router, Arc<Gateway>) {
    build_app_with_gateway_and_options(config, raw_output, AppOptions::default())
}

pub fn build_app_with_gateway_and_options(
    config: Config,
    raw_output: Option<RawOutput>,
    options: AppOptions,
) -> (axum::Router, Arc<Gateway>) {
    let http_client = reqwest::Client::builder()
        .tcp_nodelay(true)
        .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
        .build()
        .expect("reqwest client");
    let replay_store = ReplayStore::new(config.max_replay_entries);
    let monitor = if options.with_debug_ui {
        MonitorHub::new(512)
    } else {
        MonitorHub::disabled()
    };
    // D1 dashboard FlowStore: enabled only when the debug UI is on, mirroring the
    // monitor's zero-overhead `disabled()` split.
    let flow_store = if options.with_debug_ui {
        crate::dashboard_flow::DashboardFlowStore::new()
    } else {
        crate::dashboard_flow::DashboardFlowStore::disabled()
    };
    // D5 MetricsLayer: enabled only when the debug UI is on (same zero-overhead
    // `disabled()` split). Attached to the Gateway via `with_metrics`; the shared 1 s
    // publisher (with a snapshot every fifth cut) is spawned below under the same gate,
    // so production runs no ring/histogram/publisher/snapshot work.
    let backend_metrics = if crate::backend_metrics::collection_enabled(options.with_debug_ui) {
        crate::backend_metrics::BackendMetricsStore::new()
    } else {
        crate::backend_metrics::BackendMetricsStore::disabled()
    };
    let metrics = if options.with_debug_ui {
        crate::metrics::MetricsLayer::new().with_backend_metrics(backend_metrics.clone())
    } else {
        crate::metrics::MetricsLayer::disabled()
    };
    // Durable dashboard cuts are independently opt-in through an env-only path, but
    // remain gated by `--with-debug-ui` so a production-disabled dashboard retains its
    // zero-task/zero-IO behavior. Large bodies continue to live in `turn_capture_dir`.
    let dashboard_history = crate::dashboard_history::DashboardHistory::from_env(
        options.with_debug_ui,
        config.turn_capture_dir.clone(),
    );
    let durable_cursors = dashboard_history.bootstrap_cursors();
    flow_store.hydrate_sequence(durable_cursors.flow_seq);
    monitor.hydrate_sequence(durable_cursors.monitor_seq);
    let durable_terminals = dashboard_history.bootstrap_terminal_summaries();
    metrics.hydrate_archive(durable_cursors, durable_terminals.as_slice());
    metrics.hydrate_last_activity(dashboard_history.bootstrap_last_activity());
    // F1 durable per-turn capture remains independently usable without the dashboard,
    // but when both are enabled its final artifact publication is acknowledged by the
    // same archive that owns flow history.
    let turn_capture = match config.turn_capture_dir.clone() {
        Some(dir) => crate::turn_capture::TurnCapture::enabled(dir)
            .with_dashboard_history(dashboard_history.clone()),
        None => crate::turn_capture::TurnCapture::disabled(),
    };
    // Routing mode is engaged by explicit `upstreams` OR ad-hoc `model_routes`
    // (G7); routes alone are enough to switch the gateway into the routing
    // client so route-name/glob matching applies.
    let routing_mode = !config.upstreams.is_empty() || !config.model_routes.is_empty();
    // Per-backend-model finalization policies (effort map, `template_family`
    // override, `upstream_chat_kwargs`), shared (cheap clone) across all leaf
    // clients so each resolves against the FINAL provider model (T1). Built once
    // from config; the leaf (`finalize_request_for_backend`) looks up the policy
    // for the model it actually POSTs to. Copy the scalar config knobs out so
    // the builder closure doesn't borrow `config` (moved into the Gateway below).
    let finalization_policies = crate::upstream::BackendFinalizationPolicies::from_config(&config);
    let flatten_content = config.flatten_content;
    let min_completion_tokens = config.min_completion_tokens;
    let max_sse_frame_bytes = config.max_sse_frame_bytes;
    let make_upstream_client =
        |base_url: url::Url, api_key: Option<String>, log_path: Option<std::path::PathBuf>| {
            ReqwestUpstreamClient::with_options(
                http_client.clone(),
                base_url,
                api_key,
                log_path,
                flatten_content,
                min_completion_tokens,
                max_sse_frame_bytes,
            )
            .with_finalization_policies(finalization_policies.clone())
            // D2: every leaf shares the dashboard FlowStore handle (a cheap `Clone`
            // of the inner `Arc<Mutex>`; `disabled()` no-ops when the debug UI is
            // off) so the single point that sees the on-wire body can capture it.
            .with_flow_store(flow_store.clone())
        };
    let upstream: Arc<dyn crate::upstream::UpstreamClient> = if routing_mode {
        let providers = config
            .upstreams
            .iter()
            .map(|provider| {
                let primary_client = make_upstream_client(
                    provider.upstream_base_url.clone(),
                    provider.upstream_api_key.clone(),
                    provider.upstream_request_log_path.clone(),
                );
                let fallback_providers = provider
                    .fallback_upstreams
                    .iter()
                    .map(|fallback| {
                        FailoverUpstreamProvider::new(
                            fallback.name.clone(),
                            make_upstream_client(
                                fallback.upstream_base_url.clone(),
                                fallback.upstream_api_key.clone(),
                                fallback.upstream_request_log_path.clone(),
                            ),
                            fallback.upstream_model.clone(),
                            fallback.exposed_model.clone(),
                            fallback.upstream_chat_kwargs.clone(),
                        )
                    })
                    .collect();
                RoutingUpstreamProvider::new(
                    provider.name.clone(),
                    primary_client,
                    provider.upstream_model.clone(),
                    provider.upstream_chat_kwargs.clone(),
                    fallback_providers,
                    Duration::from_secs(config.upstream_failure_cooldown_secs),
                )
            })
            .collect();
        // Build a synthetic provider + spec per ad-hoc route (G7). Each route is
        // a single-upstream client keyed by request-model name/glob; the glob
        // matcher was compiled at config time.
        let cooldown = Duration::from_secs(config.upstream_failure_cooldown_secs);
        let mut route_providers = Vec::with_capacity(config.model_routes.len());
        let mut route_specs = Vec::with_capacity(config.model_routes.len());
        for (index, route) in config.model_routes.iter().enumerate() {
            let client = make_upstream_client(
                route.upstream_base_url.clone(),
                config.upstream_api_key.clone(),
                config.upstream_request_log_path.clone(),
            );
            route_providers.push(RouteUpstreamProvider::new(
                format!("route-{}", route.name),
                client,
                cooldown,
            ));
            route_specs.push(ModelRouteSpec::new(
                route.name.clone(),
                route.glob.clone(),
                index,
                route.upstream_model.clone(),
            ));
        }
        Arc::new(RoutingUpstreamClient::with_routes(
            providers,
            route_providers,
            route_specs,
        ))
    } else {
        let primary_upstream = make_upstream_client(
            config.upstream_base_url.clone(),
            config.upstream_api_key.clone(),
            config.upstream_request_log_path.clone(),
        );
        if config.fallback_upstreams.is_empty() {
            // D2: the BARE leaf is the engine's upstream directly — no routing/
            // failover layer owns the `provider` serving field, so mark this leaf to
            // synthesize `provider = "primary"`.
            Arc::new(primary_upstream.into_bare_primary())
        } else {
            let mut providers = vec![FailoverUpstreamProvider::new(
                "primary",
                primary_upstream,
                None,
                None,
                serde_json::Map::new(),
            )];
            providers.extend(config.fallback_upstreams.iter().map(|provider| {
                FailoverUpstreamProvider::new(
                    provider.name.clone(),
                    make_upstream_client(
                        provider.upstream_base_url.clone(),
                        provider.upstream_api_key.clone(),
                        provider.upstream_request_log_path.clone(),
                    ),
                    provider.upstream_model.clone(),
                    provider.exposed_model.clone(),
                    provider.upstream_chat_kwargs.clone(),
                )
            }));
            Arc::new(FailoverUpstreamClient::new(
                providers,
                Duration::from_secs(config.upstream_failure_cooldown_secs),
            ))
        }
    };
    let backend_metrics_targets = upstream.backend_metrics_targets();
    let search = Arc::new(BraveSearchClient::new(http_client.clone(), config.clone()));
    // G4 image agent: a vision client + a shared per-session image cache. The
    // cache is constructed once and shared so the strip seam (in
    // `stream_responses`) and the executor (`run_image_analysis`) see the same
    // store. Construction is unconditional and cheap; gating happens per-turn.
    let vision: Arc<dyn crate::vision::VisionClient> =
        Arc::new(ReqwestVisionClient::new(http_client, &config));
    let image_cache = Arc::new(ImageCache::from_config(&config));

    // D7 dashboard/`/debug` auth, built from the ENVIRONMENT (never from the
    // persisted `Config`). Only constructed when the debug UI is enabled. The
    // env snapshot + bind address also drive the route-registration decision:
    // a non-loopback bind without a token + validated https origin REFUSES to
    // register the protected routes (logged), unless `ALLOW_INSECURE=1`.
    let bind_addr = config.bind_addr;
    let (dashboard_auth, register_protected_routes) = if options.with_debug_ui {
        build_dashboard_auth(bind_addr)
    } else {
        (None, false)
    };

    // Capture cheap handles for the ONE process-wide one-second metrics publisher before
    // the originals move into Gateway. The task takes the fixed FlowStore→Metrics order,
    // broadcasts one immutable shared cut, and persists that same cut every fifth tick.
    let publisher_flow_store = flow_store.clone();
    let flow_history_receiver = flow_store.subscribe();
    let publisher_metrics = metrics.clone();
    let publisher_monitor = monitor.clone();
    let publisher_history = dashboard_history.clone();
    let gateway = Arc::new(
        Gateway::new(
            config,
            replay_store,
            upstream,
            search,
            vision,
            image_cache,
            monitor,
            raw_output,
            flow_store,
        )
        .with_dashboard_auth(dashboard_auth)
        .with_metrics(metrics)
        .with_turn_capture(turn_capture)
        .with_dashboard_history(dashboard_history),
    );
    // Install the initial immutable presentation synchronously, before any REST/WS
    // consumer can observe the Gateway. The async publisher owns every later cut, but
    // this bootstrap cut removes the pre-first-tick recomputation race while sharing
    // the same sequence allocator and FlowStore→Metrics lock order.
    if options.with_debug_ui {
        let topology = gateway.provider_health_publisher();
        topology.hydrate_sequence(durable_cursors.topology_seq);
        topology.publish(gateway.upstream_health());
        let _ = gateway.metrics().publish_metrics_cut(
            gateway.flow_store(),
            &topology,
            gateway.debug_snapshot().last_sequence,
            false,
        );
    }
    // D4: spawn the topology-health publication task ONLY when the debug UI is on,
    // so production keeps the zero-overhead path (no 1 s tick). Guard on a live
    // tokio runtime so a non-async embedder that enables the debug UI does not
    // panic in `tokio::spawn` (the `main.rs` server path always has one).
    if options.with_debug_ui && tokio::runtime::Handle::try_current().is_ok() {
        let _ = crate::backend_metrics::spawn_backend_metrics_collector(
            backend_metrics,
            backend_metrics_targets,
        );
        gateway.spawn_provider_health_publisher();
        // Spawn ONE process-level publisher rather than a timer per dashboard socket.
        // Every fifth one-second cut is also retained as the historical snapshot.
        let _ = crate::metrics::spawn_metrics_publisher_task_with_history(
            publisher_metrics,
            publisher_flow_store,
            gateway.provider_health_publisher(),
            publisher_monitor,
            publisher_history,
        );
        crate::dashboard_history::spawn_monitor_history_task(
            gateway.dashboard_history().clone(),
            gateway.subscribe_monitor(),
        );
        if let Some(receiver) = flow_history_receiver {
            crate::dashboard_history::spawn_flow_history_task(
                gateway.dashboard_history().clone(),
                receiver,
            );
        }
    }
    let router_options = RouterOptions {
        with_debug_ui: options.with_debug_ui,
        register_protected_routes,
    };
    let app = build_router(Arc::clone(&gateway), router_options);
    (app, gateway)
}

/// Build the D7 dashboard auth context + the route-registration decision for a
/// server binding to `bind_addr`, reading secrets from the process environment.
/// Returns `(Some(auth), true)` when the protected routes may register,
/// `(None, false)` when the startup decision refuses them (logged) or the auth
/// context fails to build (e.g. a malformed secret — logged). Warnings (a
/// tokenless loopback dev server, an auto-generated key, an insecure override)
/// are logged here so a running process is auditable.
fn build_dashboard_auth(
    bind_addr: std::net::SocketAddr,
) -> (Option<Arc<crate::dashboard_auth::DashboardAuth>>, bool) {
    use crate::dashboard_auth::DashboardEnv;
    use crate::dashboard_auth::RouteDecision;
    use crate::dashboard_auth::startup_route_decision;

    let env = DashboardEnv::from_process_env();
    let decision = startup_route_decision(bind_addr, &env);
    match decision {
        RouteDecision::Refuse(refusal) => {
            tracing::warn!(
                "dashboard/debug routes NOT registered: {} (set the required env vars, or \
                 LLMCONDUIT_ALLOW_INSECURE_DASHBOARD=1 to override)",
                refusal.reason()
            );
            (None, false)
        }
        RouteDecision::Register { warnings } => {
            for warning in &warnings {
                tracing::warn!("dashboard auth: {warning}");
            }
            match crate::dashboard_auth::DashboardAuth::from_env(bind_addr, &env) {
                Ok(build) => {
                    for warning in &build.warnings {
                        tracing::warn!("dashboard auth: {warning}");
                    }
                    (Some(build.auth), true)
                }
                Err(err) => {
                    tracing::error!(
                        "dashboard/debug routes NOT registered: failed to build auth context: \
                         {err}"
                    );
                    (None, false)
                }
            }
        }
    }
}

pub fn build_app_from_gateway(gateway: Arc<Gateway>) -> axum::Router {
    build_app_from_gateway_with_options(gateway, AppOptions::default())
}

pub fn build_app_from_gateway_with_options(
    gateway: Arc<Gateway>,
    options: AppOptions,
) -> axum::Router {
    build_router(gateway, options.into())
}

#[derive(Debug, Clone, Copy, Default)]
pub struct AppOptions {
    pub with_debug_ui: bool,
}

impl From<AppOptions> for RouterOptions {
    fn from(options: AppOptions) -> Self {
        // The `build_app_from_gateway*` path (tests, embedders) has no bind
        // address / env snapshot to run the D7 startup decision against, so it
        // does NOT register the protected routes — `build_app_with_gateway_and_options`
        // is the path that computes the decision and attaches the auth context.
        Self {
            with_debug_ui: options.with_debug_ui,
            register_protected_routes: false,
        }
    }
}
