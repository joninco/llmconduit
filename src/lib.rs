pub mod adapters;
pub mod cli;
pub mod config;
pub mod engine;
pub mod error;
pub mod http;
pub mod models;
pub mod monitor;
pub mod replay;
pub mod request_log;
pub mod search;
pub mod ui;
pub mod upstream;

use crate::config::Config;
use crate::engine::Gateway;
use crate::http::build_router;
use crate::monitor::MonitorHub;
use crate::replay::ReplayStore;
use crate::search::BraveSearchClient;
use crate::upstream::ReqwestUpstreamClient;
use std::sync::Arc;
use std::time::Duration;

pub fn build_app(config: Config) -> axum::Router {
    build_app_with_gateway(config).0
}

pub fn build_app_with_gateway(config: Config) -> (axum::Router, Arc<Gateway>) {
    let http_client = reqwest::Client::builder()
        .tcp_nodelay(true)
        .timeout(config.request_timeout)
        .connect_timeout(Duration::from_secs(config.connect_timeout_secs))
        .build()
        .expect("reqwest client");
    let replay_store = ReplayStore::new(config.max_replay_entries);
    let monitor = MonitorHub::new(512);
    let upstream = Arc::new(ReqwestUpstreamClient::new(
        http_client.clone(),
        config.upstream_base_url.clone(),
        config.upstream_api_key.clone(),
        config.upstream_request_log_path.clone(),
        config.flatten_content,
    ));
    let search = Arc::new(BraveSearchClient::new(http_client, config.clone()));
    let gateway = Arc::new(Gateway::new(
        config,
        replay_store,
        upstream,
        search,
        monitor,
    ));
    let app = build_router(Arc::clone(&gateway));
    (app, gateway)
}
