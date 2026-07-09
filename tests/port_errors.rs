//! Ported error-classification behaviors for the context-window-limit retry
//! (gap G1), adapted from claude-relay's `tests/test_server.py::test_non_200_retry_*`.
//!
//! The unit tests exercise the pure classifier
//! [`llmconduit::upstream::classify_context_overflow`] directly: each upstream
//! overflow shape must parse into the recomputed UNCLAMPED
//! `available_completion_tokens` (`ctx − safety − input`; floor/terminal policy
//! lives in the leaf retry loop), the right `reason`/lower-bound flags, and
//! unrelated text must yield `None` (no retry). The integration tests prove the
//! classifier is wired into the upstream non-2xx path as a bounded CONVERGENCE
//! loop: it re-budgets from each error's LATEST reported input, goes terminal
//! ("reduce the prompt") when even the minimum completion budget cannot fit
//! beside a backend-REPORTED input, stops on no-progress or the attempt cap,
//! and hands a non-overflow retry error to the normal failover disposition
//! (E2a) instead of looping.

use llmconduit::upstream::classify_context_overflow;

/// Mirrors `upstream::CONTEXT_RETRY_SAFETY_TOKENS`. Deliberately tiny: backends
/// reject only `input + output > ctx`, and the leaf loop (not a fat margin)
/// absorbs a lower bound's understatement by re-budgeting from the next error.
const SAFETY: i64 = 8;

#[test]
fn detects_completion_token_limit() {
    let error_body = "max_completion_tokens=250000 cannot be greater than max_model_len=202,752";

    let retry = classify_context_overflow(error_body, None).expect("retry decision");

    assert_eq!(retry.reason, "completion_limit");
    assert_eq!(retry.ctx_limit, 202752);
    assert_eq!(retry.input_tokens, None);
    // ctx_limit - SAFETY, no input estimate available.
    assert_eq!(retry.available_completion_tokens, 202752 - SAFETY);
}

#[test]
fn uses_available_context_for_completion_token_limit() {
    let error_body = "max_completion_tokens=250000 cannot be greater than max_model_len=202,752";

    let retry = classify_context_overflow(error_body, Some(139000)).expect("retry decision");

    assert_eq!(retry.reason, "completion_limit");
    assert_eq!(retry.ctx_limit, 202752);
    assert_eq!(retry.input_tokens, Some(139000));
    // 202752 - 8 - 139000.
    assert_eq!(retry.available_completion_tokens, 63744);
}

#[test]
fn detects_vllm_context_limit() {
    let error_body = "This model's maximum context length is 202752 tokens. \
        However, you requested 64000 output tokens and your prompt contains 139000 input tokens.";

    let retry = classify_context_overflow(error_body, None).expect("retry decision");

    assert_eq!(retry.reason, "context_limit");
    assert_eq!(retry.ctx_limit, 202752);
    assert_eq!(retry.input_tokens, Some(139000));
    assert!(!retry.input_tokens_is_lower_bound);
    // 202752 - 8 - 139000.
    assert_eq!(retry.available_completion_tokens, 63744);
}

#[test]
fn detects_vllm_at_least_context_limit() {
    let error_body = "This model's maximum context length is 65536 tokens. \
        However, you requested 64000 output tokens and your prompt contains at least 1537 input tokens, \
        for a total of at least 65537 tokens. Please reduce the length of the input prompt or the number \
        of requested output tokens. (parameter=input_tokens, value=1537)";

    let retry = classify_context_overflow(error_body, None).expect("retry decision");

    assert_eq!(retry.reason, "context_limit");
    assert_eq!(retry.ctx_limit, 65536);
    assert_eq!(retry.input_tokens, Some(1537));
    assert!(retry.input_tokens_is_lower_bound);
    assert_eq!(retry.requested_output_tokens, Some(64000));
    // 1537 + 64000 == 65537 == ctx + 1: the backend back-computed the minimal
    // input that explains the overflow — the derived-bound fingerprint.
    assert!(retry.input_tokens_is_derived);
    // Same small safety reserve as an exact count: 65536 - 8 - 1537. The loop,
    // not a wider margin, absorbs a lower bound's understatement.
    assert_eq!(retry.available_completion_tokens, 63991);
}

#[test]
fn at_least_bound_without_exact_overflow_arithmetic_is_not_derived() {
    // A genuine lower bound whose total does NOT land exactly one past the
    // window (150000 + 64000 != 202753) is a real (if conservative) count —
    // the loop may re-budget from it arithmetically.
    let error_body = "This model's maximum context length is 202752 tokens. \
        However, you requested 64000 output tokens and your prompt contains at least 150000 input tokens.";

    let retry = classify_context_overflow(error_body, None).expect("retry decision");

    assert!(retry.input_tokens_is_lower_bound);
    assert!(!retry.input_tokens_is_derived);
    assert_eq!(retry.available_completion_tokens, 202752 - SAFETY - 150000);
}

#[test]
fn lower_bound_boundary_error_uses_same_small_safety_reserve() {
    let error_body = "This model's maximum context length is 262144 tokens. \
        However, you requested 63798 output tokens and your prompt contains at least 198347 input tokens, \
        for a total of at least 262145 tokens. Please reduce the length of the input prompt or the number \
        of requested output tokens. (parameter=input_tokens, value=198347)";

    let retry = classify_context_overflow(error_body, None).expect("retry decision");

    assert_eq!(retry.reason, "context_limit");
    assert_eq!(retry.ctx_limit, 262144);
    assert_eq!(retry.input_tokens, Some(198347));
    assert!(retry.input_tokens_is_lower_bound);
    // 262144 - 8 - 198347.
    assert_eq!(retry.available_completion_tokens, 63789);
}

#[test]
fn derived_lower_bound_converges_across_rounds() {
    // Live incident (2026-07-08, 524288-ctx backend): the "at least N" input is
    // DERIVED from the request (`ctx − max_tokens + 1`), so it tightened by
    // exactly the shrunk amount and the retired one-shot design (1024-token
    // margin off the FIRST error) re-overflowed by exactly one token. Replaying
    // both captured bodies must yield strictly decreasing budgets whose second
    // round genuinely fits beside the tightened bound.
    let round1 = "This model's maximum context length is 524288 tokens. However, you \
        requested 64000 output tokens and your prompt contains at least 460289 input tokens, \
        for a total of at least 524289 tokens. (parameter=input_tokens, value=460289)";
    let round2 = "This model's maximum context length is 524288 tokens. However, you \
        requested 62975 output tokens and your prompt contains at least 461314 input tokens, \
        for a total of at least 524289 tokens. (parameter=input_tokens, value=461314)";

    let first = classify_context_overflow(round1, None).expect("round 1 classifies");
    assert!(first.input_tokens_is_lower_bound);
    // Both captured rounds carry the derived fingerprint (total == ctx + 1),
    // which is what routes the retry loop to the escalating backoff.
    assert!(first.input_tokens_is_derived);
    assert_eq!(first.requested_output_tokens, Some(64000));
    assert_eq!(first.available_completion_tokens, 524288 - SAFETY - 460289);

    let second = classify_context_overflow(round2, None).expect("round 2 classifies");
    assert_eq!(second.available_completion_tokens, 524288 - SAFETY - 461314);
    assert!(
        second.available_completion_tokens < first.available_completion_tokens,
        "the tightened bound must shrink the budget (strict progress)"
    );
    assert!(
        461314 + second.available_completion_tokens <= 524288,
        "the re-budgeted round must fit the window"
    );
}

#[test]
fn detects_openai_compatible_context_limit() {
    let error_body = "This model's maximum context length is 202752 tokens. \
        However, you requested 203000 tokens (139000 in the messages, 64000 in the completion).";

    let retry = classify_context_overflow(error_body, None).expect("retry decision");

    assert_eq!(retry.reason, "context_limit");
    assert_eq!(retry.ctx_limit, 202752);
    assert_eq!(retry.input_tokens, Some(139000));
    // 202752 - 8 - 139000.
    assert_eq!(retry.available_completion_tokens, 63744);
}

#[test]
fn detects_openai_compatible_context_limit_in_the_prompt_variant() {
    // Canonical OpenAI overflow wording uses "in the prompt" (not "in the
    // messages"). Branch 3 must classify this exactly like the "in the messages"
    // variant and extract the same input (139000) / output (64000) split.
    let error_body = "This model's maximum context length is 202752 tokens. \
        However, you requested 203000 tokens (139000 in the prompt, 64000 in the completion).";

    let retry = classify_context_overflow(error_body, None).expect("retry decision");

    assert_eq!(retry.reason, "context_limit");
    assert_eq!(retry.ctx_limit, 202752);
    assert_eq!(retry.input_tokens, Some(139000));
    // 202752 - 8 - 139000.
    assert_eq!(retry.available_completion_tokens, 63744);
}

#[test]
fn detects_requested_token_count_context_limit() {
    let error_body = "Requested token count exceeds the model's maximum context length of 202752 tokens. \
        You requested a total of 206272 tokens: 142272 tokens from the input messages \
        and 64000 tokens for the completion. Please reduce the number of tokens in the \
        input messages or the completion to fit within the limit.";

    let retry = classify_context_overflow(error_body, None).expect("retry decision");

    assert_eq!(retry.reason, "context_limit");
    assert_eq!(retry.ctx_limit, 202752);
    assert_eq!(retry.input_tokens, Some(142272));
    // 202752 - 8 - 142272.
    assert_eq!(retry.available_completion_tokens, 60472);
}

#[test]
fn reports_budget_below_floor_unclamped() {
    // The classifier no longer clamps to the configured floor: the leaf loop
    // owns that policy (a parsed input leaving less than the floor goes
    // TERMINAL "reduce the prompt" instead of retrying at a budget the backend
    // would reject or truncate into uselessness).
    let error_body = "This model's maximum context length is 202752 tokens. \
        However, you requested 64000 output tokens and your prompt contains 202000 input tokens.";

    let retry = classify_context_overflow(error_body, None).expect("retry decision");

    // 202752 - 8 - 202000 = 744, reported raw.
    assert_eq!(retry.available_completion_tokens, 744);
}

#[test]
fn reports_negative_budget_when_prompt_exceeds_window() {
    // A prompt that alone exceeds the window yields a NEGATIVE budget — still
    // reported raw so the call site can distinguish "cannot fit at all" from
    // "fits below the floor" without re-parsing the body.
    let error_body = "This model's maximum context length is 202752 tokens. \
        However, you requested 64000 output tokens and your prompt contains 202800 input tokens.";

    let retry = classify_context_overflow(error_body, None).expect("retry decision");

    assert_eq!(retry.available_completion_tokens, 202752 - SAFETY - 202800);
    assert!(retry.available_completion_tokens < 0);
}

#[test]
fn ignores_unrelated_errors() {
    assert_eq!(
        classify_context_overflow("backend is unavailable", None),
        None
    );
}

#[test]
fn ignores_unrelated_body_that_merely_mentions_token_keywords() {
    // A validation/echo error that names the max-token parameters (and even
    // carries numbers next to them) but is NOT a context-window overflow. The
    // classifier must require the actual overflow wording, not bare keyword
    // presence, so this yields None (no retry, no shrink).
    let error_body = "Invalid request: parameter max_completion_tokens=64000 is not allowed \
        together with max_model_len=202752 for this deployment; remove one of them.";

    assert_eq!(
        classify_context_overflow(error_body, None),
        None,
        "an unrelated error mentioning max_completion_tokens/max_model_len must not trigger a retry"
    );
}

#[test]
fn ignores_body_with_overflow_anchors_but_no_requested_token_count_exceeds() {
    // The "Requested token count exceeds" shape is gated on that literal phrase.
    // A 4xx body that merely carries its generic anchors ("maximum context
    // length of …" + "tokens from input messages") WITHOUT the overflow
    // assertion must NOT be classified as an overflow (no shrink-and-retry).
    let error_body = "Configuration error: the maximum context length of 202752 tokens \
        was computed from 142272 tokens from input messages and the reserved completion \
        budget; adjust your deployment, this is not a per-request limit.";

    assert_eq!(
        classify_context_overflow(error_body, None),
        None,
        "anchors without the literal 'requested token count exceeds' must not trigger a retry"
    );
}

#[test]
fn ignores_requested_total_anchors_without_requested_token_count_exceeds_literal() {
    // Branch 4 (Codex round 6 overmatch regression): the "requested a total of"
    // shape's DISTINCTIVE LEADING literal is `Requested token count exceeds`.
    // This body carries EVERY generic anchor and token clause the branch keys on
    // -- "maximum context length of N tokens", "requested a total of N tokens",
    // "N tokens from input messages", "N tokens for the completion" -- but it is
    // a validation/diagnostics 400 rejecting a different field ("temperature"),
    // NOT a context overflow, and so it LACKS the leading "requested token count
    // exceeds" assertion. Before the fix this still classified and triggered a
    // shrink/retry; the leading-literal anchor must now reject it (None).
    let error_body = "Diagnostics: maximum context length of 202752 tokens. You requested \
        a total of 206272 tokens: 142272 tokens from input messages and 64000 tokens for \
        the completion, but the rejected field was temperature.";

    assert_eq!(
        classify_context_overflow(error_body, None),
        None,
        "all 'requested a total of' anchors present but the leading 'requested token count \
         exceeds' literal absent must not trigger a retry"
    );
}

#[test]
fn ignores_vllm_combined_anchors_without_requested_output_tokens_literal() {
    // Branch 2 (vLLM combined input+output) negative: a body carrying the
    // generic anchors ("maximum context length is …", bare "output tokens",
    // "prompt contains … input tokens") but WITHOUT the request-side
    // `requested N output tokens` literal must NOT classify. Here the request
    // side reads "we reserved N output tokens" (no "requested N output
    // tokens"), and the bare "output tokens" / "prompt contains" anchors are
    // present — proving co-presence of generic anchors is insufficient.
    let error_body = "Capacity note: this model's maximum context length is 202752 tokens. \
        We reserved 64000 output tokens for streaming; the cached prompt contains \
        139000 input tokens from a prior turn. This is informational, not a limit.";

    assert_eq!(
        classify_context_overflow(error_body, None),
        None,
        "generic combined anchors without the 'requested N output tokens' literal must not retry"
    );
}

#[test]
fn ignores_vllm_combined_when_request_side_follows_context_side() {
    // Branch 2 ordering negative: all the literal pieces exist, but the
    // request-side `requested N output tokens` clause appears AFTER the
    // context-side `prompt contains N input tokens` clause. The required
    // ordering (request side before context side) rejects it.
    let error_body = "This model's maximum context length is 202752 tokens. The prompt contains \
        139000 input tokens, which is fine; separately you requested 64000 output tokens earlier.";

    assert_eq!(
        classify_context_overflow(error_body, None),
        None,
        "out-of-order combined clauses (context side before request side) must not retry"
    );
}

#[test]
fn ignores_openai_compatible_anchors_without_completion_clause() {
    // Branch 3 (OpenAI-compatible) negative: a body with the generic anchors
    // ("maximum context length is …" + "in the messages") but missing both the
    // request-side `requested N tokens` literal and the `… in the completion`
    // half of the parenthetical. Co-presence of "maximum context length is" and
    // "in the messages" alone must NOT classify.
    let error_body = "Diagnostics: this model's maximum context length is 202752 tokens. \
        We counted 139000 tokens in the messages cache for telemetry purposes. No request was rejected.";

    assert_eq!(
        classify_context_overflow(error_body, None),
        None,
        "OpenAI-compatible anchors without the 'requested N tokens' + 'in the completion' literals must not retry"
    );
}

#[test]
fn ignores_openai_compatible_when_clauses_out_of_order() {
    // Branch 3 ordering negative: the request-side `requested N tokens`, the
    // `in the messages` split, and the `in the completion` half all exist, but
    // the completion clause precedes the messages clause. The enforced ordering
    // (requested -> messages -> completion) rejects it.
    let error_body = "This model's maximum context length is 202752 tokens. However, you requested \
        203000 tokens; 64000 in the completion budget were set before 139000 in the messages were tallied.";

    assert_eq!(
        classify_context_overflow(error_body, None),
        None,
        "out-of-order OpenAI-compatible clauses (completion before messages) must not retry"
    );
}

#[test]
fn ignores_requested_without_adjacent_token_count() {
    // Branch 3 (OpenAI-compatible) adjacency negative (Codex round 5): the body
    // carries every generic anchor -- "maximum context length is N tokens", the
    // word "requested", and a parenthetical with "in the prompt" + "in the
    // completion" -- but the word "requested" is NOT immediately followed by a
    // number ("requested operation", not "requested N tokens"). The reference
    // regex's tight `requested\s*([\d,]+)\s*tokens` adjacency rejects it, so it
    // must NOT classify (no genuine `requested N tokens` overflow).
    let error_body = "Invalid requested operation. This model's maximum context length is 202752 \
        tokens. Diagnostics: 139000 in the prompt, 64000 in the completion.";

    assert_eq!(
        classify_context_overflow(error_body, None),
        None,
        "a body whose 'requested' is not adjacent to 'N tokens' must not trigger a retry"
    );
}

mod integration {
    use serde_json::Value;
    use serde_json::json;
    use tower::ServiceExt;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use axum::body::Body;
    use axum::http::Request;
    use llmconduit::config::Config;
    use llmconduit::config::FallbackUpstreamConfig;
    use llmconduit::config::UnsupportedImagePolicy;
    use uuid::Uuid;

    fn config_for(server_uri: &str) -> Config {
        Config {
            bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
            upstream_base_url: format!("{server_uri}/v1/").parse().expect("url"),
            upstream_api_key: None,
            upstream_model: None,
            system_prompt_prefix: None,
            upstream_request_log_path: None,
            turn_capture_dir: None,
            upstream_chat_kwargs: serde_json::Map::new(),
            upstreams: Vec::new(),
            fallback_upstreams: Vec::new(),
            upstream_failure_cooldown_secs: 30,
            model_profiles: std::collections::BTreeMap::new(),
            model_routes: Vec::new(),
            template_family: None,
            brave_base_url: "https://example.com/".parse().expect("url"),
            brave_api_key: None,
            brave_max_results: 5,
            request_timeout: std::time::Duration::from_secs(30),
            connect_timeout_secs: 10,
            max_web_search_rounds: 5,
            flatten_content: true,
            max_replay_entries: 1000,
            debug_log_max_age_hours: None,
            min_completion_tokens: 4096,
            max_sse_frame_bytes: 8 * 1024 * 1024,
            max_request_body_bytes: 10 * 1024 * 1024,
            image_agent_enabled: false,
            vision_url: None,
            vision_model: None,
            image_cache_max_size: 100,
            image_cache_ttl_secs: 300,
            unsupported_image_policy: UnsupportedImagePolicy::Placeholder,
            price_table: std::collections::HashMap::new(),
        }
    }

    fn chat_sse_body() -> String {
        let chunk = json!({
            "id": "chat-retry",
            "object": "chat.completion.chunk",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": "hello after retry"},
                "finish_reason": null
            }],
            "usage": null
        });
        format!("data: {}\n\ndata: [DONE]\n\n", chunk)
    }

    /// Collect the chat-completions POST bodies an upstream mock received, in
    /// arrival order, so tests can assert on the forwarded `max_tokens` per attempt.
    async fn chat_post_bodies(server: &MockServer) -> Vec<Value> {
        server
            .received_requests()
            .await
            .expect("requests")
            .into_iter()
            .filter(|request| {
                request.method.as_str() == "POST" && request.url.path() == "/v1/chat/completions"
            })
            .map(|request| serde_json::from_slice(&request.body).expect("chat body json"))
            .collect()
    }

    /// Upstream returns a context-limit 400 on the first chat POST, then 200 on
    /// the retry. The leaf client must shrink `max_completion_tokens` and retry
    /// (two POSTs total), and the turn must succeed. The reduced budget is
    /// checked on the second (retried) upstream request body.
    #[tokio::test]
    async fn upstream_context_limit_400_then_200_retries_once_with_reduced_budget() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "test-model"}]
            })))
            .mount(&server)
            .await;

        // First chat POST -> context-limit 400. ctx=65536, input=1000,
        // safety=8 => reduced budget = 65536 - 8 - 1000 = 64528.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "This model's maximum context length is 65536 tokens. However, you requested \
                 250000 output tokens and your prompt contains 1000 input tokens.",
            ))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;

        // Retry -> 200 with a normal SSE body.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(chat_sse_body()),
            )
            .with_priority(2)
            .mount(&server)
            .await;

        let app = llmconduit::build_app(config_for(&server.uri()));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "test-model",
                            "stream": false,
                            "max_tokens": 250000,
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status().as_u16(), 200, "turn should succeed");
        let body_bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read body");
        let body: Value = serde_json::from_slice(&body_bytes).expect("json body");
        assert_eq!(
            body["choices"][0]["message"]["content"].as_str(),
            Some("hello after retry")
        );

        let chat_requests = chat_post_bodies(&server).await;

        // One shrink was enough here: the original POST plus a single re-request.
        assert_eq!(
            chat_requests.len(),
            2,
            "expected exactly one shrink-and-retry"
        );

        assert_eq!(
            chat_requests[0]["max_tokens"].as_i64(),
            Some(250000),
            "first attempt keeps the caller's requested budget"
        );
        assert_eq!(
            chat_requests[1]["max_tokens"].as_i64(),
            Some(64528),
            "retry carries the reduced completion budget"
        );
    }

    /// The live-incident shape: the backend's "at least N input tokens" is a
    /// lower bound DERIVED from the request (`ctx − max_tokens + 1`), so the
    /// bound TIGHTENS on the retry and a budget computed from the FIRST error
    /// re-overflows. Round 1 carries the derived fingerprint
    /// (input + output == ctx + 1) → escalating backoff (−512); round 2 reports
    /// a REAL tightened bound (no fingerprint) → arithmetic re-budget. The turn
    /// converges within the attempt cap with strictly decreasing budgets.
    #[tokio::test]
    async fn derived_lower_bound_overflow_converges_after_two_shrinks() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "test-model"}]
            })))
            .mount(&server)
            .await;

        // POST 1 -> derived bound (460289 + 64000 == 524289 == ctx + 1): the
        // fingerprint carries no true prompt size, so the loop backs off
        // 64000 - 512 = 63488 instead of chasing the arithmetic bound.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "This model's maximum context length is 524288 tokens. However, you requested \
                 64000 output tokens and your prompt contains at least 460289 input tokens, \
                 for a total of at least 524289 tokens. (parameter=input_tokens, value=460289)",
            ))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;

        // POST 2 -> a REAL tightened bound (461314 + 63488 != ctx + 1, so no
        // derived fingerprint): arithmetic re-budget = 524288 - 8 - 461314 = 62966.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "This model's maximum context length is 524288 tokens. However, you requested \
                 63488 output tokens and your prompt contains at least 461314 input tokens, \
                 for a total of at least 524802 tokens. (parameter=input_tokens, value=461314)",
            ))
            .up_to_n_times(1)
            .with_priority(2)
            .mount(&server)
            .await;

        // POST 3 -> fits.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(chat_sse_body()),
            )
            .with_priority(3)
            .mount(&server)
            .await;

        let app = llmconduit::build_app(config_for(&server.uri()));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "test-model",
                            "stream": false,
                            "max_tokens": 64000,
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(
            response.status().as_u16(),
            200,
            "the converging loop must turn the persisted overflow into a success"
        );
        let body_bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read body");
        let body: Value = serde_json::from_slice(&body_bytes).expect("json body");
        assert_eq!(
            body["choices"][0]["message"]["content"].as_str(),
            Some("hello after retry")
        );

        let chat_requests = chat_post_bodies(&server).await;
        assert_eq!(
            chat_requests.len(),
            3,
            "two shrinks then success — within the 4-attempt cap"
        );

        let budgets: Vec<i64> = chat_requests
            .iter()
            .map(|request| request["max_tokens"].as_i64().expect("max_tokens"))
            .collect();
        assert_eq!(
            budgets,
            vec![64000, 63488, 62966],
            "derived round backs off 512; the real tightened bound re-budgets arithmetically"
        );
        assert!(
            budgets.windows(2).all(|pair| pair[1] < pair[0]),
            "forwarded budgets must be strictly decreasing"
        );
        // The final budget fits beside the tightest reported bound.
        assert!(461314 + budgets[2] <= 524288);
    }

    /// A backend-REPORTED input that leaves less than the configured minimum
    /// completion budget (4096) cannot be fixed by shrinking output: the leaf
    /// must go terminal on the FIRST error — no retry, no failover — and tell
    /// the caller to reduce the prompt.
    #[tokio::test]
    async fn prompt_exceeding_window_is_terminal_without_retry() {
        let primary = MockServer::start().await;
        let fallback = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "primary-model"}]
            })))
            .mount(&primary)
            .await;

        // input=65000 of ctx=65536 leaves 65536 - 8 - 65000 = 528 < 4096 floor.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "This model's maximum context length is 65536 tokens. However, you requested \
                 4096 output tokens and your prompt contains 65000 input tokens.",
            ))
            .mount(&primary)
            .await;

        // Fallback would happily succeed -- it must NEVER be reached.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(chat_sse_body()),
            )
            .mount(&fallback)
            .await;

        let mut config = config_for(&primary.uri());
        config.upstream_model = Some("primary-model".to_string());
        config.fallback_upstreams = vec![FallbackUpstreamConfig {
            name: "fallback".to_string(),
            upstream_base_url: format!("{}/v1/", fallback.uri()).parse().expect("url"),
            upstream_api_key: None,
            upstream_model: Some("fallback-model".to_string()),
            exposed_model: None,
            upstream_chat_kwargs: serde_json::Map::new(),
            upstream_request_log_path: None,
        }];
        config.upstream_failure_cooldown_secs = 3600;

        let app = llmconduit::build_app(config);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "primary-model",
                            "stream": false,
                            "max_tokens": 64000,
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        // 400, not 502: the client must treat this as its own input to fix
        // (Claude Code keys on the "prompt is too long" shape), never as a
        // transient gateway failure worth retrying.
        assert_eq!(response.status().as_u16(), 400, "prompt-too-long is a 400");
        let body_bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read body");
        let body: Value = serde_json::from_slice(&body_bytes).expect("json body");
        let message = body["error"]["message"].as_str().unwrap_or_default();
        assert!(
            message.contains("prompt is too long") && message.contains("reduce the prompt"),
            "error must tell the caller to reduce the prompt, got: {message}"
        );

        let primary_posts = chat_post_bodies(&primary).await.len();
        assert_eq!(
            primary_posts, 1,
            "no shrink can fix an oversized prompt — exactly one upstream POST"
        );
        let fallback_posts = chat_post_bodies(&fallback).await.len();
        assert_eq!(
            fallback_posts, 0,
            "an oversized prompt must NOT fail over to the next provider"
        );
    }

    /// A NON-overflow error on a shrink retry must exit the loop immediately
    /// with the normal E2a disposition — here a 429, which stays
    /// failover-eligible, so the turn succeeds via the fallback provider
    /// instead of looping against the primary.
    #[tokio::test]
    async fn non_overflow_error_after_shrink_exits_loop_as_failover() {
        let primary = MockServer::start().await;
        let fallback = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "primary-model"}]
            })))
            .mount(&primary)
            .await;

        // POST 1 -> genuine overflow: triggers one shrink.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "This model's maximum context length is 65536 tokens. However, you requested \
                 250000 output tokens and your prompt contains 1000 input tokens.",
            ))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&primary)
            .await;

        // POST 2 (the shrunk retry) -> 429: NOT an overflow, must not loop.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited, slow down"))
            .with_priority(2)
            .mount(&primary)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(chat_sse_body()),
            )
            .mount(&fallback)
            .await;

        let mut config = config_for(&primary.uri());
        config.upstream_model = Some("primary-model".to_string());
        config.fallback_upstreams = vec![FallbackUpstreamConfig {
            name: "fallback".to_string(),
            upstream_base_url: format!("{}/v1/", fallback.uri()).parse().expect("url"),
            upstream_api_key: None,
            upstream_model: Some("fallback-model".to_string()),
            exposed_model: None,
            upstream_chat_kwargs: serde_json::Map::new(),
            upstream_request_log_path: None,
        }];
        config.upstream_failure_cooldown_secs = 3600;

        let app = llmconduit::build_app(config);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "primary-model",
                            "stream": false,
                            "max_tokens": 250000,
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(
            response.status().as_u16(),
            200,
            "the 429 must fail over to the fallback provider, not loop or go terminal"
        );
        let body_bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read body");
        let body: Value = serde_json::from_slice(&body_bytes).expect("json body");
        assert_eq!(
            body["choices"][0]["message"]["content"].as_str(),
            Some("hello after retry")
        );

        let primary_posts = chat_post_bodies(&primary).await.len();
        assert_eq!(
            primary_posts, 2,
            "the 429 must exit the shrink loop immediately (overflow POST + one retry)"
        );
        let fallback_posts = chat_post_bodies(&fallback).await.len();
        assert_eq!(fallback_posts, 1, "the fallback serves the turn");
    }

    /// A context-window overflow that PERSISTS with an UNCHANGED reported input
    /// stalls the budget (the recomputation yields the same value that was just
    /// rejected): the no-progress guard must exit terminal after the second
    /// POST rather than re-sending an identical request, and the failover
    /// client must NOT re-send the oversized prompt to the next provider.
    #[tokio::test]
    async fn persistent_overflow_after_retry_does_not_fail_over_to_next_provider() {
        let primary = MockServer::start().await;
        let fallback = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "primary-model"}]
            })))
            .mount(&primary)
            .await;

        // Primary returns the SAME context-overflow body on every POST, so the
        // second recomputation makes no progress (identical budget).
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "This model's maximum context length is 65536 tokens. However, you requested \
                 250000 output tokens and your prompt contains 1000 input tokens.",
            ))
            .mount(&primary)
            .await;

        // Fallback would happily succeed -- it must NEVER be reached.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(chat_sse_body()),
            )
            .mount(&fallback)
            .await;

        let mut config = config_for(&primary.uri());
        config.upstream_model = Some("primary-model".to_string());
        config.fallback_upstreams = vec![FallbackUpstreamConfig {
            name: "fallback".to_string(),
            upstream_base_url: format!("{}/v1/", fallback.uri()).parse().expect("url"),
            upstream_api_key: None,
            upstream_model: Some("fallback-model".to_string()),
            exposed_model: None,
            upstream_chat_kwargs: serde_json::Map::new(),
            upstream_request_log_path: None,
        }];
        config.upstream_failure_cooldown_secs = 3600;

        let app = llmconduit::build_app(config);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "primary-model",
                            "stream": false,
                            "max_tokens": 250000,
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        // Terminal 400 prompt-too-long, NOT a 200 produced by failing over.
        assert_eq!(
            response.status().as_u16(),
            400,
            "a persistent overflow must surface terminally, not succeed via failover"
        );
        let body_bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read body");
        let body: Value = serde_json::from_slice(&body_bytes).expect("json body");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("persisted after shrink-and-retry")),
            "error should describe the persistent overflow, got: {body}"
        );

        // Primary saw the original POST plus exactly one retry: the unchanged
        // reported input stalls the budget, and the no-progress guard stops the loop.
        let primary_posts = chat_post_bodies(&primary).await.len();
        assert_eq!(
            primary_posts, 2,
            "a stalled budget must stop after one shrink-and-retry (2 POSTs)"
        );

        // The fallback provider must NEVER have been contacted.
        let fallback_posts = chat_post_bodies(&fallback).await.len();
        assert_eq!(
            fallback_posts, 0,
            "a same-provider context overflow must NOT fail over to the next provider"
        );
    }

    /// A reported input that keeps GROWING always promises progress, so only
    /// the attempt cap stops the loop: after 4 total upstream POSTs the leaf
    /// must go terminal ("persisted") — a 5th send that would have succeeded
    /// must never happen.
    #[tokio::test]
    async fn unconverging_bound_exhausts_attempt_cap() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "test-model"}]
            })))
            .mount(&server)
            .await;

        // Four overflow rounds whose reported input grows each time; every
        // recomputation still makes strict progress, so only the cap can stop
        // the loop. ctx=524288, safety=8:
        //   input 300000 -> budget 224280
        //   input 350000 -> budget 174280
        //   input 400000 -> budget 124280
        //   input 450000 -> (4th attempt: cap reached, terminal)
        for (index, input_tokens) in [300000, 350000, 400000, 450000].into_iter().enumerate() {
            Mock::given(method("POST"))
                .and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(400).set_body_string(format!(
                    "This model's maximum context length is 524288 tokens. However, you \
                     requested 250000 output tokens and your prompt contains {input_tokens} \
                     input tokens."
                )))
                .up_to_n_times(1)
                .with_priority((index + 1) as u8)
                .mount(&server)
                .await;
        }

        // A 5th POST would hit this 200 — the cap must prevent it.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(chat_sse_body()),
            )
            .with_priority(5)
            .mount(&server)
            .await;

        let app = llmconduit::build_app(config_for(&server.uri()));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "test-model",
                            "stream": false,
                            "max_tokens": 250000,
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(
            response.status().as_u16(),
            400,
            "cap exhaustion is a terminal 400 prompt-too-long"
        );
        let body_bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read body");
        let body: Value = serde_json::from_slice(&body_bytes).expect("json body");
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("persisted after shrink-and-retry")),
            "cap exhaustion should surface the persisted-overflow terminal, got: {body}"
        );

        let chat_requests = chat_post_bodies(&server).await;
        assert_eq!(
            chat_requests.len(),
            4,
            "attempts are capped at 4 total upstream POSTs"
        );
        let budgets: Vec<i64> = chat_requests
            .iter()
            .map(|request| request["max_tokens"].as_i64().expect("max_tokens"))
            .collect();
        assert_eq!(
            budgets,
            vec![250000, 224280, 174280, 124280],
            "each capped round re-budgets from the LATEST reported input"
        );
    }

    /// A configured `upstream_chat_kwargs` max-token ALIAS (`max_completion_tokens`)
    /// flows into the request `extra_body` when the caller sends no max-token
    /// field. On a context-overflow retry the leaf shrinks the typed
    /// `max_output_tokens` (serialized as `max_tokens`) but must also STRIP the
    /// stale alias so the upstream cannot honor the oversized value and defeat the
    /// retry. The retried body must carry only the reduced `max_tokens` and none
    /// of the max-token aliases.
    #[tokio::test]
    async fn retry_strips_max_token_aliases_from_extra_body() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "test-model"}]
            })))
            .mount(&server)
            .await;

        // First chat POST -> context-limit 400. ctx=65536, input=1000,
        // safety=8 => reduced budget = 65536 - 8 - 1000 = 64528.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "This model's maximum context length is 65536 tokens. However, you requested \
                 250000 output tokens and your prompt contains 1000 input tokens.",
            ))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(chat_sse_body()),
            )
            .with_priority(2)
            .mount(&server)
            .await;

        let mut config = config_for(&server.uri());
        // A configured max-token alias default. Because the caller sends no
        // max-token field, the engine leaves this in the request extra_body, so
        // the leaf retry sees a stale alias that must be stripped.
        config.upstream_chat_kwargs =
            serde_json::Map::from_iter([("max_completion_tokens".to_string(), json!(250000))]);

        let app = llmconduit::build_app(config);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "test-model",
                            "stream": false,
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status().as_u16(), 200, "turn should succeed");

        let chat_requests = chat_post_bodies(&server).await;
        assert_eq!(
            chat_requests.len(),
            2,
            "expected exactly one shrink-and-retry"
        );

        // First attempt: the configured alias is present (no typed field yet).
        assert_eq!(
            chat_requests[0]["max_completion_tokens"].as_i64(),
            Some(250000),
            "first attempt carries the configured alias default"
        );

        // Retry: only the reduced typed `max_tokens` survives; every alias is gone.
        let retried = &chat_requests[1];
        assert_eq!(
            retried["max_tokens"].as_i64(),
            Some(64528),
            "retry carries the reduced completion budget on the typed field"
        );
        assert!(
            retried.get("max_completion_tokens").is_none(),
            "retry must strip the stale max_completion_tokens alias"
        );
        assert!(
            retried.get("max_output_tokens").is_none(),
            "retry must not leak a max_output_tokens alias"
        );
    }

    /// Both the original POST and the G1 shrink-and-retry POST must land in the
    /// JSONL request log so the reduced budget is observable to `analyze-log`.
    /// Before T10 only the first POST was logged; here we assert the file holds
    /// two lines (original budget, then the reduced one) and that `analyze-log`
    /// reports the `max_tokens` change between them.
    #[tokio::test]
    async fn shrink_and_retry_post_is_logged() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "test-model"}]
            })))
            .mount(&server)
            .await;

        // First chat POST -> context-limit 400. ctx=65536, input=1000,
        // safety=8 => reduced budget = 65536 - 8 - 1000 = 64528.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "This model's maximum context length is 65536 tokens. However, you requested \
                 250000 output tokens and your prompt contains 1000 input tokens.",
            ))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(chat_sse_body()),
            )
            .with_priority(2)
            .mount(&server)
            .await;

        let log_path = std::env::temp_dir().join(format!(
            "llmconduit-retry-log-{}.jsonl",
            Uuid::new_v4().simple()
        ));
        let mut config = config_for(&server.uri());
        config.upstream_request_log_path = Some(log_path.clone());

        let app = llmconduit::build_app(config);
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "test-model",
                            "stream": false,
                            "max_tokens": 250000,
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status().as_u16(), 200, "turn should succeed");
        let _ = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read body");

        let logged = std::fs::read_to_string(&log_path).expect("read request log");
        let lines: Vec<&str> = logged.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(
            lines.len(),
            2,
            "both the original and the retry POST must be logged, got: {logged}"
        );

        let first: Value = serde_json::from_str(lines[0]).expect("first logged request");
        assert_eq!(
            first["max_tokens"].as_i64(),
            Some(250000),
            "first logged request keeps the caller's budget"
        );
        let retried: Value = serde_json::from_str(lines[1]).expect("retried logged request");
        assert_eq!(
            retried["max_tokens"].as_i64(),
            Some(64528),
            "second logged request carries the reduced retry budget"
        );

        // `analyze-log` diffs consecutive request lines, so the reduced budget
        // surfaces as a changed `max_tokens` path -- both POSTs are visible to it.
        let report =
            llmconduit::request_log::analyze_request_log(&log_path, 10).expect("analyze log");
        assert!(
            report.contains("$.max_tokens"),
            "analyze-log should report the reduced-budget retry diff, got: {report}"
        );

        let _ = std::fs::remove_file(&log_path);
    }

    /// A backend whose bound is PURELY derived (every error reports exactly
    /// `ctx − sent + 1`, tracking whatever we send — the live 2026-07-09
    /// failure) gives the arithmetic re-budget only SAFETY+1 tokens per round.
    /// The fingerprint must route every such round through the ESCALATING
    /// backoff instead: −512, then −1024 off the budget the backend just
    /// rejected.
    #[tokio::test]
    async fn derived_bound_backoff_escalates_512_then_1024() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "test-model"}]
            })))
            .mount(&server)
            .await;

        // Round 1: derived for sent=64000 (460289 + 64000 == 524289) -> backoff
        // 64000 - 512 = 63488.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "This model's maximum context length is 524288 tokens. However, you requested \
                 64000 output tokens and your prompt contains at least 460289 input tokens, \
                 for a total of at least 524289 tokens. (parameter=input_tokens, value=460289)",
            ))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;

        // Round 2: the bound TRACKED the shrink — derived again for sent=63488
        // (460801 + 63488 == 524289) -> backoff escalates: 63488 - 1024 = 62464.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "This model's maximum context length is 524288 tokens. However, you requested \
                 63488 output tokens and your prompt contains at least 460801 input tokens, \
                 for a total of at least 524289 tokens. (parameter=input_tokens, value=460801)",
            ))
            .up_to_n_times(1)
            .with_priority(2)
            .mount(&server)
            .await;

        // Round 3: fits.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(chat_sse_body()),
            )
            .with_priority(3)
            .mount(&server)
            .await;

        let app = llmconduit::build_app(config_for(&server.uri()));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "model": "test-model",
                            "stream": false,
                            "max_tokens": 64000,
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(
            response.status().as_u16(),
            200,
            "escalating backoff must land a purely derived-bound backend"
        );

        let chat_requests = chat_post_bodies(&server).await;
        let budgets: Vec<i64> = chat_requests
            .iter()
            .map(|request| request["max_tokens"].as_i64().expect("max_tokens"))
            .collect();
        assert_eq!(
            budgets,
            vec![64000, 63488, 62464],
            "each derived round concedes an escalating slice (512, then 1024)"
        );
    }

    /// The Anthropic surface (Claude Code's compaction path): a terminal
    /// prompt-too-long must reach the client as HTTP 400 with Anthropic's
    /// `invalid_request_error` type and a "prompt is too long" message — the
    /// shape clients key on to stop retrying and engage their own trim
    /// fallbacks — never a "temporary" 502 `api_error`.
    #[tokio::test]
    async fn anthropic_prompt_too_long_surfaces_invalid_request_error() {
        let server = MockServer::start().await;

        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{"id": "test-model"}]
            })))
            .mount(&server)
            .await;

        // input=65000 of ctx=65536 leaves 528 < the 4096 floor: terminal on the
        // first error, no retry.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(400).set_body_string(
                "This model's maximum context length is 65536 tokens. However, you requested \
                 4096 output tokens and your prompt contains 65000 input tokens.",
            ))
            .mount(&server)
            .await;

        let app = llmconduit::build_app(config_for(&server.uri()));
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/v1/messages")
                    .header("content-type", "application/json")
                    .header("anthropic-version", "2023-06-01")
                    .body(Body::from(
                        json!({
                            "model": "test-model",
                            "stream": false,
                            "max_tokens": 64000,
                            "messages": [{"role": "user", "content": "hi"}]
                        })
                        .to_string(),
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(
            response.status().as_u16(),
            400,
            "Anthropic surface must serve prompt-too-long as a 400"
        );
        let body_bytes = axum::body::to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("read body");
        let body: Value = serde_json::from_slice(&body_bytes).expect("json body");
        assert_eq!(
            body["error"]["type"].as_str(),
            Some("invalid_request_error"),
            "Anthropic error type must be invalid_request_error, got: {body}"
        );
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("prompt is too long")),
            "message must lead with the prompt-is-too-long shape, got: {body}"
        );

        assert_eq!(
            chat_post_bodies(&server).await.len(),
            1,
            "terminal prompt-too-long must not retry upstream"
        );
    }
}
