//! End-to-end security regressions for the public Responses and raw proxy
//! surfaces. All credential-like values below are fixed test sentinels.

mod common;

use async_trait::async_trait;
use axum::Router;
use axum::body::{Body, Bytes, to_bytes};
use axum::http::{HeaderMap, Request, StatusCode};
use common::{MockSearch, test_config};
use llmconduit::config::{Config, LogBodyMode};
use llmconduit::dashboard_flow::DashboardFlowStore;
use llmconduit::engine::Gateway;
use llmconduit::error::AppResult;
use llmconduit::monitor::MonitorHub;
use llmconduit::replay::ReplayStore;
use llmconduit::responses_capabilities::{
    CapabilityCandidate, CapabilityPlan, CapabilityTarget, PromptCacheKeyCapability,
    ResponsesCapabilities, ResponsesCapabilitiesConfig,
};
use llmconduit::turn_capture::TurnCapture;
use llmconduit::upstream::{
    BackendChatRequest, ReqwestUpstreamClient, UpstreamClient, UpstreamStream,
};
use llmconduit::vision::{ImageCache, ReqwestVisionClient, VisionClient};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const SECRET_SENTINEL: &str = "fixed-prompt-cache-secret-7d3b1f";

struct TempArtifacts {
    root: PathBuf,
}

impl TempArtifacts {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "llmconduit-responses-security-{label}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&root).expect("create security-test artifact directory");
        Self { root }
    }

    fn capture_dir(&self) -> PathBuf {
        self.root.join("turns")
    }

    fn request_log(&self) -> PathBuf {
        self.root.join("upstream.jsonl")
    }
}

impl Drop for TempArtifacts {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A real HTTP leaf with an explicit Responses capability plan. The production
/// leaf's capability setter is intentionally crate-private, so this small test
/// wrapper delegates every exercised operation while exposing the exact plan
/// needed by the public engine seam.
struct CapabilityLeaf {
    inner: ReqwestUpstreamClient,
    capabilities: ResponsesCapabilities,
}

#[async_trait]
impl UpstreamClient for CapabilityLeaf {
    async fn stream_chat_completion(
        &self,
        request: &BackendChatRequest,
    ) -> AppResult<UpstreamStream> {
        self.inner.stream_chat_completion(request).await
    }

    async fn stream_chat_completion_with_timeout(
        &self,
        request: &BackendChatRequest,
        request_timeout: Duration,
    ) -> AppResult<UpstreamStream> {
        self.inner
            .stream_chat_completion_with_timeout(request, request_timeout)
            .await
    }

    async fn list_models(&self) -> AppResult<reqwest::Response> {
        self.inner.list_models().await
    }

    async fn proxy_completions(
        &self,
        headers: HeaderMap,
        body: Bytes,
    ) -> AppResult<reqwest::Response> {
        self.inner.proxy_completions(headers, body).await
    }

    fn response_body_idle_timeout(&self) -> Duration {
        self.inner.response_body_idle_timeout()
    }

    async fn responses_capability_plan(&self, requested_model: &str) -> CapabilityPlan {
        CapabilityPlan {
            candidates: vec![CapabilityCandidate {
                target: CapabilityTarget {
                    provider: "primary".to_string(),
                    model: requested_model.to_string(),
                },
                capabilities: self.capabilities.clone(),
            }],
        }
    }
}

fn app_with_upstream(config: Config, upstream: Arc<dyn UpstreamClient>) -> Router {
    let capture = config
        .turn_capture_dir
        .clone()
        .map(TurnCapture::enabled)
        .unwrap_or_else(TurnCapture::disabled);
    let replay_entries = config.replay.max_entries;
    let vision: Arc<dyn VisionClient> =
        Arc::new(ReqwestVisionClient::new(reqwest::Client::new(), &config));
    let image_cache = Arc::new(ImageCache::from_config(&config));
    let gateway = Gateway::new(
        config,
        ReplayStore::new(replay_entries),
        upstream,
        Arc::new(MockSearch::default()),
        vision,
        image_cache,
        MonitorHub::disabled(),
        None,
        DashboardFlowStore::disabled(),
    )
    .with_api_auth(None)
    .with_turn_capture(capture);
    llmconduit::build_app_from_gateway(Arc::new(gateway))
}

async fn mount_models_and_success(server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object":"list",
            "data":[{"id":"model-a","object":"model"}]
        })))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(common::chat_completion_sse_body(&[
                    json!({
                        "id":"chat-security",
                        "object":"chat.completion.chunk",
                        "created":1,
                        "model":"model-a",
                        "choices":[{
                            "index":0,
                            "delta":{"content":"safe output"},
                            "finish_reason":"stop"
                        }]
                    }),
                    json!({
                        "id":"chat-security",
                        "object":"chat.completion.chunk",
                        "created":1,
                        "model":"model-a",
                        "choices":[],
                        "usage":{
                            "prompt_tokens":3,
                            "completion_tokens":2,
                            "total_tokens":5
                        }
                    }),
                ])),
        )
        .mount(server)
        .await;
}

async fn wait_for_request_log(path: &Path) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Ok(contents) = std::fs::read_to_string(path)
                && !contents.trim().is_empty()
            {
                break contents;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("request log did not appear at {}", path.display()))
}

async fn wait_for_capture_artifact(dir: &Path) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        'poll: loop {
            if let Ok(entries) = std::fs::read_dir(dir) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().and_then(|ext| ext.to_str()) == Some("json")
                        && let Ok(bytes) = std::fs::read(path)
                        && let Ok(value) = serde_json::from_slice(&bytes)
                    {
                        break 'poll value;
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("turn-capture artifact did not appear at {}", dir.display()))
}

fn retained_files(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn visit(path: &Path, files: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(path) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, files);
            } else if let Ok(bytes) = std::fs::read(&path) {
                files.push((path, bytes));
            }
        }
    }

    let mut files = Vec::new();
    visit(root, &mut files);
    files
}

thread_local! {
    static CAPTURE_BUF: std::cell::RefCell<Option<Vec<u8>>> = const {
        std::cell::RefCell::new(None)
    };
}

struct ThreadLocalCapture;
struct ThreadLocalCaptureWriter;

impl std::io::Write for ThreadLocalCaptureWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        CAPTURE_BUF.with(|buffer| {
            if let Some(sink) = buffer.borrow_mut().as_mut() {
                sink.extend_from_slice(bytes);
            }
        });
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for ThreadLocalCapture {
    type Writer = ThreadLocalCaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        ThreadLocalCaptureWriter
    }
}

fn install_capture_subscriber() {
    static INSTALLED: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INSTALLED.get_or_init(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_writer(ThreadLocalCapture)
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .finish();
        let _ = tracing::subscriber::set_global_default(subscriber);
    });
}

fn capture_logs<F: std::future::Future<Output = ()>>(body: impl FnOnce() -> F) -> String {
    install_capture_subscriber();
    CAPTURE_BUF.with(|buffer| *buffer.borrow_mut() = Some(Vec::new()));
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("current-thread runtime")
        .block_on(body());
    CAPTURE_BUF.with(|buffer| {
        String::from_utf8(buffer.borrow_mut().take().unwrap_or_default())
            .expect("captured tracing is UTF-8")
    })
}

#[test]
fn responses_prompt_cache_secret_is_absent_from_logs_and_turn_capture() {
    let tracing = capture_logs(|| async {
        for mode in [LogBodyMode::Metadata, LogBodyMode::RedactedPayload] {
            let artifacts = TempArtifacts::new(match mode {
                LogBodyMode::Metadata => "metadata",
                LogBodyMode::RedactedPayload => "redacted",
            });
            let server = MockServer::start().await;
            mount_models_and_success(&server).await;

            let capability_config = ResponsesCapabilitiesConfig {
                prompt_cache_key: Some(PromptCacheKeyCapability::Upstream),
                ..Default::default()
            };
            let capabilities = capability_config.resolve();

            let mut config = test_config();
            config.brave_api_key = None;
            config.upstream_base_url = format!("{}/v1", server.uri()).parse().unwrap();
            config.api_log_body_mode = mode;
            config.upstream_request_log_body_mode = mode;
            config.upstream_request_log_path = Some(artifacts.request_log());
            config.turn_capture_dir = Some(artifacts.capture_dir());
            config.responses_capabilities = capability_config;

            let leaf = ReqwestUpstreamClient::new(
                reqwest::Client::new(),
                config.upstream_base_url.clone(),
                None,
                config.upstream_request_log_path.clone(),
                config.flatten_content,
                config.min_completion_tokens,
            )
            .with_request_timeout(config.request_timeout)
            .with_request_log_body_mode(mode);
            let app = app_with_upstream(
                config,
                Arc::new(CapabilityLeaf {
                    inner: leaf,
                    capabilities,
                }),
            );

            let response = app
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri("/v1/responses")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            json!({
                                "model":"model-a",
                                "input":"keep the request secret out of diagnostics",
                                "prompt_cache_key":SECRET_SENTINEL,
                                "stream":false
                            })
                            .to_string(),
                        ))
                        .expect("Responses request"),
                )
                .await
                .expect("Responses HTTP result");
            assert_eq!(response.status(), StatusCode::OK);
            let response_bytes = to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("read Responses resource");
            let mut client_resource: Value =
                serde_json::from_slice(&response_bytes).expect("Responses resource JSON");

            // The official resource intentionally echoes this request control.
            // Handle that client-visible value explicitly, then redact it before
            // applying the same whole-value no-secret assertion used for sinks.
            assert_eq!(client_resource["prompt_cache_key"], SECRET_SENTINEL);
            client_resource
                .as_object_mut()
                .expect("response object")
                .insert(
                    "prompt_cache_key".to_string(),
                    Value::String("[redacted: official client echo]".to_string()),
                );
            assert!(!client_resource.to_string().contains(SECRET_SENTINEL));

            let request_log = wait_for_request_log(&artifacts.request_log()).await;
            assert!(
                !request_log.contains(SECRET_SENTINEL),
                "{mode:?} upstream JSONL leaked the prompt cache key: {request_log}"
            );
            let request_log_entry: Value = serde_json::from_str(
                request_log
                    .lines()
                    .find(|line| !line.trim().is_empty())
                    .unwrap(),
            )
            .expect("upstream JSONL entry");
            match mode {
                LogBodyMode::Metadata => {
                    assert_eq!(request_log_entry["type"], "request_metadata");
                    assert!(request_log_entry["body_sha256"].is_string());
                }
                LogBodyMode::RedactedPayload => {
                    assert_eq!(request_log_entry["prompt_cache_key"], "[redacted]");
                }
            }

            let artifact = wait_for_capture_artifact(&artifacts.capture_dir()).await;
            let artifact_text = artifact.to_string();
            assert!(
                !artifact_text.contains(SECRET_SENTINEL),
                "turn capture leaked the prompt cache key: {artifact_text}"
            );
            assert_eq!(
                artifact["sections"]["inbound_request"]["content"]["prompt_cache_key"],
                "[redacted]"
            );
            assert!(
                artifact["sections"]["served_response"]["content"]
                    .as_str()
                    .is_some_and(|content| content.contains("response capture suppressed"))
            );

            // Scan every file that remains under the test root, including the
            // JSONL log and assembled turn artifact (and any unexpected residue).
            let retained = retained_files(&artifacts.root);
            assert!(!retained.is_empty());
            for (path, bytes) in retained {
                assert!(
                    !bytes
                        .windows(SECRET_SENTINEL.len())
                        .any(|window| window == SECRET_SENTINEL.as_bytes()),
                    "retained file {} leaked the prompt cache key",
                    path.display()
                );
            }

            // Upstream forwarding is explicitly authorized by this test's
            // `prompt_cache_key: upstream` capability. The ephemeral mock proves
            // the redaction checks above exercised a request that really carried
            // the secret rather than one where the field was silently discarded.
            let requests = server.received_requests().await.expect("recorded requests");
            let chat = requests
                .iter()
                .find(|request| request.url.path() == "/v1/chat/completions")
                .expect("upstream chat request");
            let upstream_body: Value = serde_json::from_slice(&chat.body).unwrap();
            assert_eq!(upstream_body["prompt_cache_key"], SECRET_SENTINEL);
        }
    });

    assert!(
        tracing.contains("/v1/responses"),
        "real request did not traverse tracing middleware: {tracing}"
    );
    assert!(
        tracing.contains("[redacted]"),
        "redacted-payload trace did not exercise secret redaction: {tracing}"
    );
    assert!(
        !tracing.contains(SECRET_SENTINEL),
        "tracing leaked the prompt cache key: {tracing}"
    );
}

#[tokio::test]
async fn raw_completions_proxy_strips_credentials_and_forwards_only_safe_headers() {
    // Install the process subscriber before this test reaches the shared HTTP
    // callsites, so the tracing-capture regression remains deterministic even
    // when the test harness schedules these two tests concurrently.
    install_capture_subscriber();

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/json")
                .set_body_json(json!({
                    "id":"cmpl-security",
                    "object":"text_completion",
                    "choices":[]
                })),
        )
        .expect(1)
        .mount(&server)
        .await;

    let mut config = test_config();
    config.brave_api_key = None;
    config.upstream_base_url = format!("{}/v1", server.uri()).parse().unwrap();
    config.upstream_api_key = Some("fixed-upstream-owned-key".to_string());
    let leaf = ReqwestUpstreamClient::new(
        reqwest::Client::new(),
        config.upstream_base_url.clone(),
        config.upstream_api_key.clone(),
        None,
        config.flatten_content,
        config.min_completion_tokens,
    )
    .with_request_timeout(config.request_timeout);
    let app = app_with_upstream(config, Arc::new(leaf));

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/completions")
                .header("accept", "application/json")
                .header("content-type", "application/json")
                .header(
                    "traceparent",
                    "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01",
                )
                .header("tracestate", "vendor=trace-safe")
                .header("x-request-id", "request-safe")
                .header("x-trace-id", "trace-safe")
                .header("authorization", "Bearer fixed-client-auth")
                .header("x-api-key", "fixed-client-x-api-key")
                .header("api-key", "fixed-client-api-key")
                .header(
                    "cookie",
                    "llmconduit_session=fixed-session; llmconduit_csrf=fixed-csrf-cookie",
                )
                .header("proxy-authorization", "Basic fixed-proxy-auth")
                .header("x-csrf-token", "fixed-dashboard-csrf")
                .header("x-session-token", "fixed-session-token")
                .header("x-llmconduit-session", "fixed-dashboard-session")
                .header("x-dashboard-token", "fixed-dashboard-token")
                .body(Body::from(
                    json!({
                        "model":"test-model",
                        "prompt":"hello",
                        "stream":false
                    })
                    .to_string(),
                ))
                .expect("raw Completions request"),
        )
        .await
        .expect("raw proxy HTTP result");
    assert_eq!(response.status(), StatusCode::OK);
    let _ = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("consume raw proxy response");

    let requests = server.received_requests().await.expect("recorded request");
    assert_eq!(requests.len(), 1);
    let sent = &requests[0].headers;
    assert_eq!(sent["accept"], "application/json");
    assert_eq!(sent["content-type"], "application/json");
    assert_eq!(
        sent["traceparent"],
        "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
    );
    assert_eq!(sent["tracestate"], "vendor=trace-safe");
    assert_eq!(sent["x-request-id"], "request-safe");
    assert_eq!(sent["x-trace-id"], "trace-safe");
    assert_eq!(
        sent["authorization"], "Bearer fixed-upstream-owned-key",
        "the gateway-owned upstream credential must replace client auth"
    );

    for denied in [
        "x-api-key",
        "api-key",
        "cookie",
        "proxy-authorization",
        "x-csrf-token",
        "x-session-token",
        "x-llmconduit-session",
        "x-dashboard-token",
    ] {
        assert!(
            !sent.contains_key(denied),
            "raw proxy forwarded sensitive/non-allowlisted header {denied}"
        );
    }
    let sent_debug = format!("{sent:?}");
    for client_secret in [
        "fixed-client-auth",
        "fixed-client-x-api-key",
        "fixed-client-api-key",
        "fixed-session",
        "fixed-csrf-cookie",
        "fixed-proxy-auth",
        "fixed-dashboard-csrf",
        "fixed-session-token",
        "fixed-dashboard-session",
        "fixed-dashboard-token",
    ] {
        assert!(
            !sent_debug.contains(client_secret),
            "raw proxy leaked client sentinel {client_secret}"
        );
    }
}
