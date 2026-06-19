//! Ported surface: config-loading (gap G7, claude-relay `test_config.py`).
//!
//! claude-relay routed requests through a `model_routes` map (name -> backend
//! URL + upstream model), supported glob route keys (`claude-opus-*`), a
//! `--model-route NAME=URL[,UPSTREAM_MODEL]` CLI flag, and a TOML config file.
//! llmconduit's native routing is catalog-driven (`canonical_model_key` over a
//! YAML `upstreams` catalog), so these behaviors are adapted rather than
//! transliterated:
//!
//! - A `model_routes` entry becomes a synthetic routing upstream matched by
//!   request-model NAME (possibly a glob), slotted in `RoutingModelCatalog::resolve`
//!   strictly between an exact catalog model id and the canonical-key/default
//!   fallbacks. PRECEDENCE: exact id > exact route > glob route > canonical key >
//!   default — so an exact model id always beats an overlapping glob, matching
//!   AGENTS.md "Exact model id wins" and claude-relay's exact-before-glob-before-
//!   default.
//! - `--model-route` parses to a persisted route merged AFTER the file and env
//!   (CLI wins); a malformed spec is a clean startup `Err`, never a panic.
//! - `.toml` config loads with identical semantics to the equivalent YAML
//!   (`PersistedConfig` round-trips through both deserializers).
//! - `template_family` (G2) still resolves through the profile chain — a
//!   regression guard, since G7 touches the same config-resolution code.
//!
//! The unit tests exercise pure config resolution; the HTTP tests drive the full
//! gateway with wiremock upstreams and assert which upstream received the routed
//! request and with what rewritten model.

mod common;

use common::test_config;
use llmconduit::config::Config;
use llmconduit::config::OrderedModelRoutes;
use llmconduit::config::PersistedConfig;
use llmconduit::config::PersistedModelRoute;
use llmconduit::config::load_persisted_config;
use llmconduit::config::parse_model_route_spec;
use llmconduit::config::write_persisted_config;
use serde_json::json;
use tower::ServiceExt;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::method;
use wiremock::matchers::path;

use axum::body::Body;
use http::Request;

/// Minimal single-chunk chat-completions SSE body for a wiremock upstream.
fn chat_sse_body(id: &str, content: &str) -> String {
    let chunk = json!({
        "id": id,
        "choices": [{
            "index": 0,
            "delta": {"content": content},
            "finish_reason": null
        }],
        "usage": null
    });
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

/// Build a `Config` from inline YAML, applying the standard `from_persisted`
/// resolution. Keeps the routing/profile resolution identical to production.
fn config_from_yaml(yaml: &str) -> Config {
    let persisted: PersistedConfig = serde_yaml::from_str(yaml).expect("yaml config");
    Config::from_persisted(&persisted).expect("resolve config")
}

// ---------------------------------------------------------------------------
// Unit tests: route parsing + precedence + CLI + TOML + template_family guard
// ---------------------------------------------------------------------------

/// claude-relay `test_parse_model_route_cli_spec`: `NAME=URL,UPSTREAM_MODEL`
/// parses into a route with the URL and upstream model.
#[test]
fn parses_model_route_cli_spec() {
    let (name, route) = parse_model_route_spec("claude-haiku=http://haiku:8000,Qwen2.5")
        .expect("valid model route spec");
    assert_eq!(name, "claude-haiku");
    assert_eq!(
        route.upstream_base_url.as_deref(),
        Some("http://haiku:8000")
    );
    assert_eq!(route.upstream_model.as_deref(), Some("Qwen2.5"));
}

/// The upstream model is optional: `NAME=URL` parses with no model rewrite.
#[test]
fn parses_model_route_cli_spec_without_upstream_model() {
    let (name, route) =
        parse_model_route_spec("claude-opus-*=http://opus:8000").expect("valid route spec");
    assert_eq!(name, "claude-opus-*");
    assert_eq!(route.upstream_base_url.as_deref(), Some("http://opus:8000"));
    assert_eq!(route.upstream_model, None);
}

/// A malformed `--model-route` spec is a clean `Err` (no `=`, blank name, blank
/// URL, or an unparseable URL) rather than a panic.
#[test]
fn malformed_model_route_specs_return_clean_errors() {
    // Missing `=`.
    assert!(parse_model_route_spec("claude-haiku").is_err());
    // Blank name.
    assert!(parse_model_route_spec("=http://haiku:8000").is_err());
    // Blank URL.
    assert!(parse_model_route_spec("claude-haiku=").is_err());
    // Unparseable URL.
    assert!(parse_model_route_spec("claude-haiku=not a url").is_err());
}

/// A malformed CLI route surfaces from `Config::from_env_file_and_routes` as a
/// startup `Err`, never a panic — the wiring path used by `main`.
#[test]
fn malformed_cli_route_fails_config_resolution_cleanly() {
    let result = Config::from_env_file_and_routes(None, &["bogus-no-equals".to_string()]);
    assert!(
        result.is_err(),
        "malformed --model-route must be a clean Err"
    );
}

/// A `model_routes` table entry with a missing `upstream_base_url` is a clean
/// config error, not a panic.
#[test]
fn route_missing_base_url_is_config_error() {
    let persisted = PersistedConfig {
        model_routes: OrderedModelRoutes(vec![(
            "claude-x".to_string(),
            // Table with no URL => missing upstream_base_url.
            serde_yaml::from_str("upstream_model: Kimi-K2.6").expect("route table"),
        )]),
        ..PersistedConfig::default()
    };
    assert!(Config::from_persisted(&persisted).is_err());
}

/// Glob routes compile and are flagged; literal names are not. (Internal shape
/// check that mirrors claude-relay `_is_glob_pattern` classification.)
#[test]
fn resolves_glob_and_literal_routes() {
    let config = config_from_yaml(
        r#"
model_routes:
  "claude-opus-*":
    upstream_base_url: "http://opus:8000/v1"
    upstream_model: "Kimi-K2.6"
  "claude-3-5-sonnet":
    upstream_base_url: "http://sonnet:8000/v1"
    upstream_model: "Qwen3.5"
"#,
    );
    assert_eq!(config.model_routes.len(), 2);
    let glob = config
        .model_routes
        .iter()
        .find(|route| route.name == "claude-opus-*")
        .expect("glob route");
    assert!(glob.glob.is_some(), "glob route must compile a matcher");
    assert_eq!(glob.upstream_model.as_deref(), Some("Kimi-K2.6"));
    let literal = config
        .model_routes
        .iter()
        .find(|route| route.name == "claude-3-5-sonnet")
        .expect("literal route");
    assert!(literal.glob.is_none(), "literal route has no glob matcher");
}

/// A bare-string route value is coerced to a URL-only route, mirroring
/// claude-relay's str-or-table `_coerce_model_route`.
#[test]
fn route_bare_string_value_coerces_to_url() {
    let config = config_from_yaml(
        r#"
model_routes:
  "claude-haiku": "http://haiku:8000/v1"
"#,
    );
    assert_eq!(config.model_routes.len(), 1);
    let route = &config.model_routes[0];
    assert_eq!(route.name, "claude-haiku");
    assert_eq!(route.upstream_base_url.as_str(), "http://haiku:8000/v1");
    assert_eq!(route.upstream_model, None);
}

/// CLI `--model-route` merges AFTER the file and wins on a NAME conflict
/// (documented resolution order: env/file < CLI).
#[test]
fn cli_route_overrides_file_route_with_same_name() {
    let yaml = r#"
model_routes:
  "claude-haiku":
    upstream_base_url: "http://from-file:8000/v1"
    upstream_model: "FileModel"
"#;
    let path = std::env::temp_dir().join(format!(
        "llmconduit-g7-{}.yaml",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&path, yaml).expect("write yaml");
    let config = Config::from_env_file_and_routes(
        Some(&path),
        &["claude-haiku=http://from-cli:8000/v1,CliModel".to_string()],
    )
    .expect("config");
    let _ = std::fs::remove_file(&path);

    let route = config
        .model_routes
        .iter()
        .find(|route| route.name == "claude-haiku")
        .expect("merged route");
    assert_eq!(route.upstream_base_url.as_str(), "http://from-cli:8000/v1");
    assert_eq!(route.upstream_model.as_deref(), Some("CliModel"));
}

/// A CLI `--model-route` overrides a file route whose name differs ONLY in case,
/// IN PLACE (position preserved) — `upsert` must compare names case-insensitively
/// (trim + ASCII-case-insensitive), identical to route dispatch. Without this a
/// `Claude-*` file route would shadow a `claude-*` CLI route, contradicting
/// CLI-wins semantics.
#[test]
fn cli_route_overrides_file_route_case_insensitively_in_place() {
    let yaml = r#"
model_routes:
  "alpha-model":
    upstream_base_url: "http://alpha:8000/v1"
  "Claude-*":
    upstream_base_url: "http://from-file:8000/v1"
    upstream_model: "FileModel"
"#;
    let path = std::env::temp_dir().join(format!(
        "llmconduit-g7-ci-{}.yaml",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&path, yaml).expect("write yaml");
    // CLI route name differs only in case from the file route.
    let config = Config::from_env_file_and_routes(
        Some(&path),
        &["claude-*=http://from-cli:8000/v1,CliModel".to_string()],
    )
    .expect("config");
    let _ = std::fs::remove_file(&path);

    // The case-variant CLI route must REPLACE the file route, not add a second
    // entry: still exactly two routes total.
    assert_eq!(
        config.model_routes.len(),
        2,
        "a case-only-different CLI route must override in place, not duplicate"
    );
    // Position preserved: `alpha-model` first, the overridden glob second.
    let names: Vec<&str> = config
        .model_routes
        .iter()
        .map(|route| route.name.as_str())
        .collect();
    assert_eq!(names, vec!["alpha-model", "claude-*"]);
    // The overridden route resolves to the CLI value.
    let glob = &config.model_routes[1];
    assert!(glob.glob.is_some(), "still a glob route");
    assert_eq!(glob.upstream_base_url.as_str(), "http://from-cli:8000/v1");
    assert_eq!(glob.upstream_model.as_deref(), Some("CliModel"));
}

/// A `.toml` config deserializes into the SAME `PersistedConfig` as the
/// equivalent YAML (claude-relay `test_load_proxy_config_reads_toml_routes`,
/// adapted to llmconduit's schema).
#[test]
fn toml_config_loads_identically_to_yaml() {
    let yaml = r#"
bind_addr: "127.0.0.1:4010"
upstream_base_url: "http://default:8000/v1"
request_timeout_secs: 120
flatten_content: false
model_routes:
  "claude-3-5-sonnet":
    upstream_base_url: "http://sonnet:8000/v1"
    upstream_model: "Qwen3.5"
  "claude-opus-*":
    upstream_base_url: "http://opus:8000/v1"
    upstream_model: "Kimi-K2.6"
"#;
    let toml = r#"
bind_addr = "127.0.0.1:4010"
upstream_base_url = "http://default:8000/v1"
request_timeout_secs = 120
flatten_content = false

[model_routes."claude-3-5-sonnet"]
upstream_base_url = "http://sonnet:8000/v1"
upstream_model = "Qwen3.5"

[model_routes."claude-opus-*"]
upstream_base_url = "http://opus:8000/v1"
upstream_model = "Kimi-K2.6"
"#;

    let from_yaml: PersistedConfig = serde_yaml::from_str(yaml).expect("yaml");
    let from_toml: PersistedConfig = toml::from_str(toml).expect("toml");
    assert_eq!(
        from_yaml, from_toml,
        "TOML and YAML must deserialize to an identical PersistedConfig"
    );
}

/// The `.toml` extension is detected by `load_persisted_config` (byte-identical
/// to loading the equivalent YAML through the file path).
#[test]
fn toml_file_extension_is_detected_on_load() {
    let toml = r#"
upstream_base_url = "http://default:8000/v1"

[model_routes."claude-opus-*"]
upstream_base_url = "http://opus:8000/v1"
upstream_model = "Kimi-K2.6"
"#;
    let path = std::env::temp_dir().join(format!(
        "llmconduit-g7-{}.toml",
        uuid::Uuid::new_v4().simple()
    ));
    std::fs::write(&path, toml).expect("write toml");
    let config = Config::from_env_and_file(Some(&path)).expect("load toml config");
    let _ = std::fs::remove_file(&path);

    assert_eq!(config.upstream_base_url.as_str(), "http://default:8000/v1");
    assert_eq!(config.model_routes.len(), 1);
    assert_eq!(config.model_routes[0].name, "claude-opus-*");
    assert_eq!(
        config.model_routes[0].upstream_model.as_deref(),
        Some("Kimi-K2.6")
    );
}

/// Regression guard for G2: `template_family` still resolves through the profile
/// chain after the G7 config changes. The most-specific matched profile wins
/// over the global override; an unmatched model falls back to the global value.
#[test]
fn template_family_still_resolves_through_profile_chain() {
    let config = config_from_yaml(
        r#"
template_family: deepseek
model_profiles:
  "Kimi-Route":
    template_family: kimi
"#,
    );
    // Profile match (case-insensitive) -> normalized profile family wins.
    assert_eq!(
        config.resolve_template_family("kimi-route", "kimi-route"),
        Some("kimi".to_string())
    );
    // No profile match -> global override applies.
    assert_eq!(
        config.resolve_template_family("other", "other"),
        Some("deepseek".to_string())
    );
}

// ---------------------------------------------------------------------------
// HTTP routing tests: glob match, exact-beats-glob, no-match -> default
// ---------------------------------------------------------------------------

/// A glob route (`claude-opus-*`) matches a request model and routes to the
/// configured upstream, rewriting the model to the route's `upstream_model`.
/// (claude-relay `test_resolve_backend_supports_glob_routes`.)
#[tokio::test]
async fn glob_route_matches_and_routes_to_configured_upstream() {
    let opus = MockServer::start().await;
    // Route providers are NOT catalog providers, so no `/v1/models` mock is
    // needed; only the dispatch endpoint matters. Match on the REWRITTEN model.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"model": "Kimi-K2.6"})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chat_sse_body("chat-opus", "from-opus")),
        )
        .mount(&opus)
        .await;

    let config = config_from_yaml(&format!(
        r#"
upstream_base_url: "http://unused.invalid/v1"
model_routes:
  "claude-opus-*":
    upstream_base_url: "{}/v1/"
    upstream_model: "Kimi-K2.6"
"#,
        opus.uri()
    ));

    let app = llmconduit::build_app(config);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-opus-4-5-20251101",
                        "stream": false,
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);
    let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .expect("read body");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json body");
    assert_eq!(
        body["choices"][0]["message"]["content"].as_str(),
        Some("from-opus")
    );
}

/// An exact catalog model id beats an overlapping glob route: the request goes
/// to the catalog upstream, NOT the glob route target. (AGENTS.md "Exact model
/// id wins"; claude-relay exact-before-glob.)
#[tokio::test]
async fn exact_catalog_id_beats_overlapping_glob_route() {
    let catalog = MockServer::start().await;
    let glob_target = MockServer::start().await;

    // The catalog upstream exposes an exact model id that ALSO matches the glob.
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "claude-opus-exact"}]
        })))
        .mount(&catalog)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"model": "claude-opus-exact"})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chat_sse_body("chat-catalog", "from-catalog")),
        )
        .mount(&catalog)
        .await;

    // The glob target must NEVER be hit for the exact id.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chat_sse_body("chat-glob", "from-glob")),
        )
        .mount(&glob_target)
        .await;

    let config = config_from_yaml(&format!(
        r#"
upstream_base_url: "http://unused.invalid/v1"
upstreams:
  - name: "catalog"
    upstream_base_url: "{}/v1/"
model_routes:
  "claude-opus-*":
    upstream_base_url: "{}/v1/"
    upstream_model: "glob-model"
"#,
        catalog.uri(),
        glob_target.uri()
    ));

    let app = llmconduit::build_app(config);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-opus-exact",
                        "stream": false,
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);
    let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .expect("read body");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json body");
    assert_eq!(
        body["choices"][0]["message"]["content"].as_str(),
        Some("from-catalog"),
        "exact catalog id must win over the overlapping glob route"
    );

    let glob_hits = glob_target
        .received_requests()
        .await
        .expect("glob requests")
        .into_iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.path() == "/v1/chat/completions"
        })
        .count();
    assert_eq!(glob_hits, 0, "glob route must not be hit for an exact id");
}

/// A request model that matches no route and no catalog id falls back to the
/// default (first model of the first non-empty provider catalog), NOT a glob
/// route. (claude-relay `test_resolve_backend_falls_back_to_default_backend`.)
#[tokio::test]
async fn no_route_match_falls_back_to_default_catalog_model() {
    let catalog = MockServer::start().await;
    let glob_target = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "default-model"}]
        })))
        .mount(&catalog)
        .await;
    // Default model is the catalog's first id; the request model is rewritten to it.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"model": "default-model"})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chat_sse_body("chat-default", "from-default")),
        )
        .mount(&catalog)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chat_sse_body("chat-glob", "from-glob")),
        )
        .mount(&glob_target)
        .await;

    let config = config_from_yaml(&format!(
        r#"
upstream_base_url: "http://unused.invalid/v1"
upstreams:
  - name: "catalog"
    upstream_base_url: "{}/v1/"
model_routes:
  "claude-opus-*":
    upstream_base_url: "{}/v1/"
    upstream_model: "glob-model"
"#,
        catalog.uri(),
        glob_target.uri()
    ));

    let app = llmconduit::build_app(config);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "totally-unknown-model",
                        "stream": false,
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);
    let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .expect("read body");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json body");
    assert_eq!(
        body["choices"][0]["message"]["content"].as_str(),
        Some("from-default"),
        "a no-match request must fall back to the default catalog model"
    );

    let glob_hits = glob_target
        .received_requests()
        .await
        .expect("glob requests")
        .into_iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.path() == "/v1/chat/completions"
        })
        .count();
    assert_eq!(glob_hits, 0, "non-matching model must not hit a glob route");
}

/// A `--model-route` CLI spec injects a route that successfully dispatches a
/// matching request through the full gateway (parse + route end-to-end).
#[tokio::test]
async fn cli_model_route_injects_and_routes_request() {
    let target = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"model": "Qwen2.5"})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chat_sse_body("chat-cli", "from-cli-route")),
        )
        .mount(&target)
        .await;

    // No config file; the route comes only from the CLI spec.
    let config = Config::from_env_file_and_routes(
        None,
        &[format!("claude-haiku={}/v1/,Qwen2.5", target.uri())],
    )
    .expect("config with CLI route");

    let app = llmconduit::build_app(config);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-haiku",
                        "stream": false,
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);
    let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .expect("read body");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json body");
    assert_eq!(
        body["choices"][0]["message"]["content"].as_str(),
        Some("from-cli-route")
    );
}

/// `test_config` (no routes) still behaves as a non-routing gateway: this proves
/// the routing-mode trigger does not engage without `upstreams`/`model_routes`.
#[test]
fn empty_model_routes_does_not_engage_routing_mode() {
    let config = test_config();
    assert!(config.model_routes.is_empty());
    assert!(config.upstreams.is_empty());
}

// ---------------------------------------------------------------------------
// Declaration-order regression (overlapping globs): order, not sorting, decides
// ---------------------------------------------------------------------------

/// Resolved `model_routes` preserve DECLARATION order, not alphabetical order.
/// `claude-opus-*` sorts AFTER `claude-*` alphabetically, so a `BTreeMap` would
/// reorder them; the ordered structure keeps file order. Reversing the YAML
/// flips the resolved order, proving declaration order (not sorting) decides.
#[test]
fn overlapping_glob_routes_preserve_declaration_order_not_alphabetical() {
    // `claude-opus-*` declared FIRST, even though it sorts after `claude-*`.
    let opus_first = config_from_yaml(
        r#"
model_routes:
  "claude-opus-*":
    upstream_base_url: "http://opus:8000/v1"
  "claude-*":
    upstream_base_url: "http://star:8000/v1"
"#,
    );
    let names: Vec<&str> = opus_first
        .model_routes
        .iter()
        .map(|route| route.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["claude-opus-*", "claude-*"],
        "declaration order must be preserved (a BTreeMap would sort claude-* first)"
    );

    // Reversed declaration => reversed resolved order.
    let star_first = config_from_yaml(
        r#"
model_routes:
  "claude-*":
    upstream_base_url: "http://star:8000/v1"
  "claude-opus-*":
    upstream_base_url: "http://opus:8000/v1"
"#,
    );
    let names: Vec<&str> = star_first
        .model_routes
        .iter()
        .map(|route| route.name.as_str())
        .collect();
    assert_eq!(names, vec!["claude-*", "claude-opus-*"]);
}

/// End-to-end: with two OVERLAPPING globs, the FIRST declared glob wins for an
/// ambiguous request model — and reversing the declaration flips the winner.
/// This locks first-match-by-declaration-order through the real routing path,
/// not alphabetical key order.
#[tokio::test]
async fn overlapping_glob_first_declared_wins_through_routing() {
    async fn route_target(label: &'static str) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(chat_sse_body("chat", label)),
            )
            .mount(&server)
            .await;
        server
    }

    async fn winner_for_declaration(specific_first: bool) -> String {
        let opus = route_target("from-opus").await;
        let star = route_target("from-star").await;
        let opus_block = format!(
            "  \"claude-opus-*\":\n    upstream_base_url: \"{}/v1/\"\n",
            opus.uri()
        );
        let star_block = format!(
            "  \"claude-*\":\n    upstream_base_url: \"{}/v1/\"\n",
            star.uri()
        );
        let routes = if specific_first {
            format!("{opus_block}{star_block}")
        } else {
            format!("{star_block}{opus_block}")
        };
        let config = config_from_yaml(&format!(
            "upstream_base_url: \"http://unused.invalid/v1\"\nmodel_routes:\n{routes}"
        ));
        let app = llmconduit::build_app(config);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "claude-opus-x",
                            "stream": false,
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status().as_u16(), 200);
        let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("read body");
        let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json body");
        body["choices"][0]["message"]["content"]
            .as_str()
            .expect("content")
            .to_string()
    }

    // `claude-opus-*` declared first => it wins for `claude-opus-x`.
    assert_eq!(
        winner_for_declaration(true).await,
        "from-opus",
        "first-declared overlapping glob must win"
    );
    // Reverse the declaration => the broader `claude-*` now wins, proving order
    // (not alphabetical sorting) decides.
    assert_eq!(
        winner_for_declaration(false).await,
        "from-star",
        "reversing declaration order must flip the winning glob"
    );
}

// ---------------------------------------------------------------------------
// serde round-trip + duplicate-key handling for OrderedModelRoutes
// ---------------------------------------------------------------------------

/// `model_routes` written by `write_persisted_config` reload through
/// `load_persisted_config` as a MAP, preserving values AND declaration order.
/// Guards against the round-trip break where the routes serialized as a YAML
/// sequence but only a map deserializes.
#[test]
fn model_routes_round_trip_through_write_and_reload_as_map() {
    let config = PersistedConfig {
        // Declared in non-alphabetical order to also lock order across the trip.
        model_routes: OrderedModelRoutes(vec![
            (
                "claude-opus-*".to_string(),
                PersistedModelRoute {
                    upstream_base_url: Some("http://opus:8000/v1".to_string()),
                    upstream_model: Some("Kimi-K2.6".to_string()),
                },
            ),
            (
                "claude-haiku".to_string(),
                PersistedModelRoute {
                    upstream_base_url: Some("http://haiku:8000/v1".to_string()),
                    upstream_model: None,
                },
            ),
        ]),
        ..PersistedConfig::default()
    };

    // The serialized form must be a MAP (`name: route`), not a sequence.
    let yaml = serde_yaml::to_string(&config).expect("serialize config");
    let parsed: serde_yaml::Value = serde_yaml::from_str(&yaml).expect("parse yaml");
    assert!(
        parsed["model_routes"].is_mapping(),
        "model_routes must serialize as a YAML map, not a sequence:\n{yaml}"
    );

    let path = std::env::temp_dir().join(format!(
        "llmconduit-g7-rt-{}.yaml",
        uuid::Uuid::new_v4().simple()
    ));
    write_persisted_config(&path, &config).expect("write config");
    let reloaded = load_persisted_config(&path).expect("reload config");
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        reloaded, config,
        "routes must round-trip through write + reload unchanged"
    );
    let names: Vec<&str> = reloaded
        .model_routes
        .0
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        names,
        vec!["claude-opus-*", "claude-haiku"],
        "declaration order must survive the round trip"
    );
}

/// Duplicate route keys collapse to last-wins (later value replaces the first,
/// preserving the first position), matching CLI-override and claude-relay dict
/// semantics — not silently keeping a shadowed first entry.
#[test]
fn duplicate_route_keys_collapse_to_last_wins() {
    let config = config_from_yaml(
        r#"
model_routes:
  "claude-x":
    upstream_base_url: "http://first:8000/v1"
    upstream_model: "FirstModel"
  "claude-x":
    upstream_base_url: "http://second:8000/v1"
    upstream_model: "SecondModel"
"#,
    );

    assert_eq!(
        config.model_routes.len(),
        1,
        "duplicate keys must collapse to a single route"
    );
    let route = &config.model_routes[0];
    assert_eq!(route.name, "claude-x");
    assert_eq!(
        route.upstream_base_url.as_str(),
        "http://second:8000/v1",
        "the later duplicate value must win"
    );
    assert_eq!(route.upstream_model.as_deref(), Some("SecondModel"));
}

// ---------------------------------------------------------------------------
// MIXED mode: `upstreams` catalog AND `model_routes` configured together. The
// engine must NOT pre-normalize a route-bound request model to the catalog
// default before the routing client can dispatch the route.
// ---------------------------------------------------------------------------

/// (a) With BOTH a catalog upstream and a glob route, a request matching ONLY
/// the glob (not any catalog id) routes to the glob target, NOT the catalog
/// default. Without the engine route-aware normalization fix this would collapse
/// to `default-model` and hit the catalog.
#[tokio::test]
async fn mixed_mode_glob_route_beats_catalog_default() {
    let catalog = MockServer::start().await;
    let glob_target = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "default-model"}]
        })))
        .mount(&catalog)
        .await;
    // Catalog must NOT receive the routed chat request.
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chat_sse_body("chat-catalog", "from-catalog")),
        )
        .mount(&catalog)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"model": "Kimi-K2.6"})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chat_sse_body("chat-glob", "from-glob")),
        )
        .mount(&glob_target)
        .await;

    let config = config_from_yaml(&format!(
        r#"
upstream_base_url: "http://unused.invalid/v1"
upstreams:
  - name: "catalog"
    upstream_base_url: "{}/v1/"
model_routes:
  "claude-opus-*":
    upstream_base_url: "{}/v1/"
    upstream_model: "Kimi-K2.6"
"#,
        catalog.uri(),
        glob_target.uri()
    ));

    let app = llmconduit::build_app(config);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "claude-opus-x",
                        "stream": false,
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);
    let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .expect("read body");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json body");
    assert_eq!(
        body["choices"][0]["message"]["content"].as_str(),
        Some("from-glob"),
        "a glob-only model must route to the glob target, not the catalog default"
    );

    let catalog_hits = catalog
        .received_requests()
        .await
        .expect("catalog requests")
        .into_iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.path() == "/v1/chat/completions"
        })
        .count();
    assert_eq!(
        catalog_hits, 0,
        "the catalog must not receive a route-bound request"
    );
}

/// (b) An EXACT route name beats a canonical-alias catalog match. The request
/// model's canonical key (`kimik2`) matches the catalog id `kimi-k2`, but its
/// exact name is a route — and exact-route precedence sits ABOVE canonical-key,
/// so the route wins. Without the fix the engine would canonicalize to the
/// catalog id and bypass the route.
#[tokio::test]
async fn mixed_mode_exact_route_name_beats_canonical_alias() {
    let catalog = MockServer::start().await;
    let route_target = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{"id": "kimi-k2"}]
        })))
        .mount(&catalog)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chat_sse_body("chat-catalog", "from-catalog")),
        )
        .mount(&catalog)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(json!({"model": "routed-kimi"})))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chat_sse_body("chat-route", "from-route")),
        )
        .mount(&route_target)
        .await;

    // Route name "Kimi K2" canonicalizes to `kimik2` == catalog `kimi-k2`.
    let config = config_from_yaml(&format!(
        r#"
upstream_base_url: "http://unused.invalid/v1"
upstreams:
  - name: "catalog"
    upstream_base_url: "{}/v1/"
model_routes:
  "Kimi K2":
    upstream_base_url: "{}/v1/"
    upstream_model: "routed-kimi"
"#,
        catalog.uri(),
        route_target.uri()
    ));

    let app = llmconduit::build_app(config);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "Kimi K2",
                        "stream": false,
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .expect("request"),
        )
        .await
        .expect("response");

    assert_eq!(response.status().as_u16(), 200);
    let body_bytes = axum::body::to_bytes(response.into_body(), 4096)
        .await
        .expect("read body");
    let body: serde_json::Value = serde_json::from_slice(&body_bytes).expect("json body");
    assert_eq!(
        body["choices"][0]["message"]["content"].as_str(),
        Some("from-route"),
        "an exact route name must beat a canonical-alias catalog match"
    );

    let catalog_hits = catalog
        .received_requests()
        .await
        .expect("catalog requests")
        .into_iter()
        .filter(|request| {
            request.method.as_str() == "POST" && request.url.path() == "/v1/chat/completions"
        })
        .count();
    assert_eq!(
        catalog_hits, 0,
        "the canonical-alias catalog must not be hit"
    );
}

/// Writing a config to a `.toml` path is rejected cleanly (not a panic) and
/// never produces a file — `configure` writes YAML, `.toml` is read-only.
#[test]
fn writing_config_to_toml_path_errors_and_creates_no_file() {
    let path = std::env::temp_dir().join(format!(
        "llmconduit-g7-write-{}.toml",
        uuid::Uuid::new_v4().simple()
    ));
    let config = PersistedConfig::default();
    let result = write_persisted_config(&path, &config);

    assert!(
        result.is_err(),
        "writing to a .toml path must be a clean Err"
    );
    let message = result.unwrap_err();
    assert!(
        message.contains("read-only") || message.contains(".toml"),
        "error must explain .toml is read-only: {message}"
    );
    assert!(
        !path.exists(),
        "a rejected .toml write must not leave a file behind"
    );
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Per-request model-resolution headers (debug aid for model mismatches)
// ---------------------------------------------------------------------------

/// A request for a model the backend does not serve falls back to the loaded
/// model, and the response is tagged with `x-llmconduit-model` (served) plus
/// `x-llmconduit-requested` (the original) so the mismatch is visible per
/// request — un-throttled, unlike the engine WARN. An exact match tags only the
/// served model and omits `x-llmconduit-requested`.
#[tokio::test]
async fn response_headers_expose_model_fallback() {
    let backend = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data": [{"id": "served-model"}]})),
        )
        .mount(&backend)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(chat_sse_body("chat-1", "ok")),
        )
        .mount(&backend)
        .await;

    let config = config_from_yaml(&format!(
        "upstreams:\n  - name: \"local\"\n    upstream_base_url: \"{}/v1/\"\n",
        backend.uri()
    ));

    let header = |response: &axum::response::Response, name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string)
    };
    let request = |model: &str| {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "model": model,
                    "stream": false,
                    "messages": [{"role": "user", "content": "hi"}]
                })
                .to_string(),
            ))
            .expect("request")
    };

    // Mismatch: requested model is not served -> fallback + BOTH headers.
    let mismatch = llmconduit::build_app(config.clone())
        .oneshot(request("claude-opus-4"))
        .await
        .expect("response");
    assert_eq!(mismatch.status().as_u16(), 200);
    assert_eq!(
        header(&mismatch, "x-llmconduit-model").as_deref(),
        Some("served-model")
    );
    assert_eq!(
        header(&mismatch, "x-llmconduit-requested").as_deref(),
        Some("claude-opus-4")
    );

    // Exact match: served model requested -> served tag only, no `requested`.
    let exact = llmconduit::build_app(config)
        .oneshot(request("served-model"))
        .await
        .expect("response");
    assert_eq!(exact.status().as_u16(), 200);
    assert_eq!(
        header(&exact, "x-llmconduit-model").as_deref(),
        Some("served-model")
    );
    assert_eq!(header(&exact, "x-llmconduit-requested"), None);
}
