//! Shared fixtures for the claude-relay -> llmconduit behavior port.
//!
//! Authored fresh (not extracted from `gateway.rs`) so the per-surface
//! `tests/port_*.rs` files can share mocks, chunk builders, and SSE collectors
//! without touching the existing 5700-line integration suite.
//!
//! Each `tests/*.rs` file compiles as its own crate, so not every surface uses
//! every helper here -- `allow(dead_code)` keeps unused-in-this-crate helpers
//! from failing the build.
#![allow(dead_code)]

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use llmconduit::config::Config;
use llmconduit::engine::Gateway;
use llmconduit::engine::SseEvent;
use llmconduit::error::AppError;
use llmconduit::models::chat::ChatChunkChoice;
use llmconduit::models::chat::ChatCompletionChunk;
use llmconduit::models::chat::ChatCompletionRequest;
use llmconduit::models::chat::ChatDelta;
use llmconduit::models::chat::ChatFunctionCall;
use llmconduit::models::chat::ChatToolCall;
use llmconduit::models::chat::ChunkUsage;
use llmconduit::models::chat::CompletionTokensDetails;
use llmconduit::models::chat::PromptTokensDetails;
use llmconduit::models::responses::ContentItem;
use llmconduit::models::responses::ReasoningRequest;
use llmconduit::models::responses::ResponseItem;
use llmconduit::models::responses::ResponsesRequest;
use llmconduit::monitor::MonitorHub;
use llmconduit::replay::ReplayStore;
use llmconduit::search::SearchClient;
use llmconduit::search::SearchOutcome;
use llmconduit::search::SearchSource;
use llmconduit::upstream::UpstreamClient;
use llmconduit::upstream::UpstreamStream;
use serde_json::Map as JsonMap;
use serde_json::Value;
use serde_json::json;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio_stream::wrappers::ReceiverStream;

/// One queued upstream turn: the ordered chunk results a single
/// `stream_chat_completion` call will yield.
type ChunkBatch = Vec<Result<ChatCompletionChunk, AppError>>;

// ---------------------------------------------------------------------------
// Mock upstream: queues canned chunk batches, records requests it received.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct MockUpstream {
    requests: Arc<Mutex<Vec<ChatCompletionRequest>>>,
    responses: Arc<Mutex<VecDeque<ChunkBatch>>>,
    supported_models: Arc<Mutex<Vec<String>>>,
    supported_model_queries: Arc<Mutex<usize>>,
}

impl MockUpstream {
    pub async fn push_response(&self, chunks: ChunkBatch) {
        self.responses.lock().await.push_back(chunks);
    }

    pub async fn requests(&self) -> Vec<ChatCompletionRequest> {
        self.requests.lock().await.clone()
    }

    pub async fn set_supported_models<I, S>(&self, models: I)
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        *self.supported_models.lock().await = models.into_iter().map(Into::into).collect();
    }

    pub async fn supported_model_queries(&self) -> usize {
        *self.supported_model_queries.lock().await
    }
}

#[async_trait]
impl UpstreamClient for MockUpstream {
    async fn stream_chat_completion(
        &self,
        request: &ChatCompletionRequest,
    ) -> Result<UpstreamStream, AppError> {
        self.requests.lock().await.push(request.clone());
        let chunks = self
            .responses
            .lock()
            .await
            .pop_front()
            .expect("queued upstream response");
        Ok(Box::pin(stream::iter(chunks)))
    }

    async fn list_models(&self) -> Result<reqwest::Response, AppError> {
        Err(AppError::internal("unused in this test"))
    }

    async fn supported_model_ids(&self) -> Result<Vec<String>, AppError> {
        *self.supported_model_queries.lock().await += 1;
        Ok(self.supported_models.lock().await.clone())
    }
}

// ---------------------------------------------------------------------------
// Mock Brave search: records queries, returns a deterministic outcome.
// ---------------------------------------------------------------------------

#[derive(Clone, Default)]
pub struct MockSearch {
    queries: Arc<Mutex<Vec<String>>>,
}

impl MockSearch {
    pub async fn queries(&self) -> Vec<String> {
        self.queries.lock().await.clone()
    }
}

#[async_trait]
impl SearchClient for MockSearch {
    async fn search(&self, query: &str) -> Result<SearchOutcome, AppError> {
        self.queries.lock().await.push(query.to_string());
        Ok(SearchOutcome {
            formatted: format!("Search result for {query}"),
            sources: vec![SearchSource {
                title: format!("Result for {query}"),
                url: "https://example.com/result".to_string(),
            }],
        })
    }
}

// ---------------------------------------------------------------------------
// Gateway / Config / request builders.
// ---------------------------------------------------------------------------

pub fn test_gateway(upstream: MockUpstream, search: MockSearch) -> Arc<Gateway> {
    test_gateway_with_config(upstream, search, test_config())
}

pub fn test_gateway_with_config(
    upstream: MockUpstream,
    search: MockSearch,
    config: Config,
) -> Arc<Gateway> {
    Arc::new(Gateway::new(
        config,
        ReplayStore::new(1000),
        Arc::new(upstream),
        Arc::new(search),
        MonitorHub::new(128),
        None,
    ))
}

pub fn test_config() -> Config {
    Config {
        bind_addr: "127.0.0.1:0".parse().expect("socket addr"),
        upstream_base_url: "http://127.0.0.1:8000/v1".parse().expect("url"),
        upstream_api_key: None,
        upstream_model: None,
        system_prompt_prefix: None,
        upstream_request_log_path: None,
        upstream_chat_kwargs: JsonMap::new(),
        upstreams: Vec::new(),
        fallback_upstreams: Vec::new(),
        upstream_failure_cooldown_secs: 30,
        model_profiles: BTreeMap::new(),
        brave_base_url: "https://example.com/".parse().expect("url"),
        brave_api_key: Some("test-key".to_string()),
        brave_max_results: 5,
        request_timeout: std::time::Duration::from_secs(30),
        connect_timeout_secs: 10,
        max_web_search_rounds: 5,
        flatten_content: true,
        max_replay_entries: 1000,
        debug_log_max_age_hours: None,
    }
}

pub fn base_request(input: Vec<ResponseItem>) -> ResponsesRequest {
    ResponsesRequest {
        model: "glm-5.1".to_string(),
        instructions: String::new(),
        input,
        tools: Vec::new(),
        tool_choice: json!("auto"),
        parallel_tool_calls: true,
        reasoning: Some(ReasoningRequest {
            effort: Some("medium".to_string()),
            summary: None,
        }),
        store: false,
        stream: true,
        include: Vec::new(),
        service_tier: None,
        prompt_cache_key: None,
        text: None,
        client_metadata: None,
        previous_response_id: None,
        temperature: None,
        top_p: None,
        max_output_tokens: None,
        frequency_penalty: None,
        presence_penalty: None,
        truncation: None,
        metadata: None,
        stop: None,
        extra_body: Default::default(),
    }
}

pub fn user_message(text: &str) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: text.to_string(),
        }],
        phase: None,
    }
}

// ---------------------------------------------------------------------------
// Upstream chunk builders (mirror the OpenAI chat-completions chunk shape).
// ---------------------------------------------------------------------------

pub fn content_chunk(id: &str, content: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                content: Some(content.to_string()),
                reasoning_content: None,
                tool_calls: None,
                function_call: None,
                refusal: None,
                extra: Default::default(),
            },
            finish_reason: None,
        }],
        usage: None,
    }
}

pub fn finish_chunk(id: &str, finish_reason: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                content: None,
                reasoning_content: None,
                tool_calls: None,
                function_call: None,
                refusal: None,
                extra: Default::default(),
            },
            finish_reason: Some(finish_reason.to_string()),
        }],
        usage: None,
    }
}

pub fn reasoning_chunk(id: &str, reasoning: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                content: None,
                reasoning_content: Some(reasoning.to_string()),
                tool_calls: None,
                function_call: None,
                refusal: None,
                extra: Default::default(),
            },
            finish_reason: None,
        }],
        usage: None,
    }
}

pub fn nested_thinking_chunk(id: &str, thinking: &str, signature: &str) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                content: None,
                reasoning_content: None,
                tool_calls: None,
                function_call: None,
                refusal: None,
                extra: BTreeMap::from([(
                    "thinking".to_string(),
                    json!({ "content": thinking, "signature": signature }),
                )]),
            },
            finish_reason: None,
        }],
        usage: None,
    }
}

pub fn tool_call_chunk(
    id: &str,
    call_id: &str,
    name: &str,
    arguments: &str,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        choices: vec![ChatChunkChoice {
            index: 0,
            delta: ChatDelta {
                content: None,
                reasoning_content: None,
                tool_calls: Some(vec![ChatToolCall {
                    id: Some(call_id.to_string()),
                    index: Some(0),
                    kind: "function".to_string(),
                    function: ChatFunctionCall {
                        name: Some(name.to_string()),
                        arguments: Some(Value::String(arguments.to_string())),
                    },
                }]),
                function_call: None,
                refusal: None,
                extra: Default::default(),
            },
            finish_reason: Some("tool_calls".to_string()),
        }],
        usage: None,
    }
}

/// Deliberately slimmer than gateway.rs's `usage_chunk` (4 args vs 6): omits the
/// `cached_tokens` / `reasoning_tokens` detail fields. Add them here if a
/// usage-detail surface is ported.
pub fn usage_chunk(
    id: &str,
    prompt_tokens: u64,
    completion_tokens: u64,
    total_tokens: u64,
) -> ChatCompletionChunk {
    ChatCompletionChunk {
        id: id.to_string(),
        usage: Some(ChunkUsage {
            prompt_tokens: prompt_tokens.try_into().expect("prompt_tokens fits in i64"),
            completion_tokens: completion_tokens
                .try_into()
                .expect("completion_tokens fits in i64"),
            total_tokens: total_tokens.try_into().expect("total_tokens fits in i64"),
            reasoning_tokens: None,
            prompt_tokens_details: None::<PromptTokensDetails>,
            completion_tokens_details: None::<CompletionTokensDetails>,
        }),
        choices: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// SSE collectors. Canonical Responses events carry an injected `_event` key so
// tests can assert on the event-name sequence as well as the payload.
// ---------------------------------------------------------------------------

pub async fn collect_stream(stream: ReceiverStream<SseEvent>) -> Vec<Value> {
    stream
        .map(|event| {
            let mut value = event.data;
            if let Value::Object(map) = &mut value {
                map.insert("_event".to_string(), Value::String(event.event));
            }
            value
        })
        .collect()
        .await
}

pub fn event_names(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(|event| event["_event"].as_str().expect("event name present"))
        .collect()
}

pub fn done_items(events: &[Value]) -> Vec<ResponseItem> {
    events
        .iter()
        .filter(|event| event["_event"] == "response.output_item.done")
        .map(|event| serde_json::from_value(event["item"].clone()).expect("response item"))
        .collect()
}

pub fn parse_anthropic_sse_events(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|block| {
            block.lines().find_map(|line| {
                line.strip_prefix("data: ")
                    .map(|data| serde_json::from_str(data).expect("valid Anthropic SSE JSON"))
            })
        })
        .collect()
}

pub fn parse_chat_sse_events(body: &str) -> Vec<Value> {
    body.split("\n\n")
        .filter_map(|block| {
            block.lines().find_map(|line| {
                line.strip_prefix("data: ").and_then(|data| {
                    (data != "[DONE]")
                        .then(|| serde_json::from_str(data).expect("valid Chat SSE JSON"))
                })
            })
        })
        .collect()
}
