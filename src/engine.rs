use crate::adapters::chat_to_responses::FinalizedAssistantTurn;
use crate::adapters::chat_to_responses::ResolvedToolCall;
use crate::adapters::chat_to_responses::StreamEmission;
use crate::adapters::chat_to_responses::StreamState;
use crate::adapters::responses_to_chat::ToolKind;
use crate::adapters::responses_to_chat::lower_request;
use crate::config::Config;
use crate::error::AppError;
use crate::error::AppResult;
use crate::models::chat::ChatCompletionRequest;
use crate::models::chat::ChatMessage;
use crate::models::chat::ChunkUsage;
use crate::models::chat::StreamOptions;
use crate::models::responses::DeltaPayload;
use crate::models::responses::FailedError;
use crate::models::responses::FailedPayload;
use crate::models::responses::FailedResponse;
use crate::models::responses::OutputItemPayload;
use crate::models::responses::ReasoningDeltaPayload;
use crate::models::responses::ResponseCompleted;
use crate::models::responses::ResponseCompletedPayload;
use crate::models::responses::ResponseInputTokensDetails;
use crate::models::responses::ResponseOutputTokensDetails;
use crate::models::responses::ResponseUsage;
use crate::models::responses::ResponseCreatedPayload;
use crate::models::responses::ResponseItem;
use crate::models::responses::ResponseStub;
use crate::models::responses::ResponsesEnvelope;
use crate::models::responses::ResponsesRequest;
use crate::models::responses::WebSearchAction;
use crate::monitor::MonitorEventKind;
use crate::monitor::MonitorHub;
use crate::replay::ReplayRecord;
use crate::replay::ReplayStore;
use crate::search::SearchClient;
use crate::upstream::UpstreamClient;
use futures::StreamExt;
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::time::timeout;
use tokio_stream::wrappers::ReceiverStream;
use uuid::Uuid;

#[derive(Clone)]
pub struct Gateway {
    config: Config,
    replay_store: ReplayStore,
    upstream: Arc<dyn UpstreamClient>,
    search: Arc<dyn SearchClient>,
    monitor: MonitorHub,
}

#[derive(Debug, Clone)]
pub struct SseEvent {
    pub event: String,
    pub data: Value,
}

impl Gateway {
    pub fn new(
        config: Config,
        replay_store: ReplayStore,
        upstream: Arc<dyn UpstreamClient>,
        search: Arc<dyn SearchClient>,
        monitor: MonitorHub,
    ) -> Self {
        Self {
            config,
            replay_store,
            upstream,
            search,
            monitor,
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn upstream_client(&self) -> Arc<dyn UpstreamClient> {
        Arc::clone(&self.upstream)
    }

    pub fn subscribe_monitor(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::monitor::MonitorEvent> {
        self.monitor.subscribe()
    }

    pub async fn stream_responses(
        self: Arc<Self>,
        request: ResponsesRequest,
    ) -> AppResult<ReceiverStream<SseEvent>> {
        let (baseline_record, prefix_len) = self.find_replay_baseline(&request).await?;
        let mut tail_request = request.clone();
        tail_request.input = request.input[prefix_len..].to_vec();
        if self.config.brave_api_key.is_none() {
            tail_request
                .tools
                .retain(|t| !matches!(t, crate::models::responses::ToolSpec::WebSearch { .. }));
        }
        let lowered = lower_request(
            &tail_request,
            baseline_record
                .as_ref()
                .map(|record| record.internal_messages.clone())
                .unwrap_or_default(),
        )?;

        let (tx, rx) = mpsc::channel(128);
        let gateway = Arc::clone(&self);
        let response_id = format!("resp_{}", Uuid::new_v4().simple());
        tokio::spawn(async move {
            let result = gateway
                .run_turn(
                    response_id.clone(),
                    request,
                    lowered.messages,
                    lowered.tools,
                    lowered.tool_registry,
                    lowered.response_format,
                    lowered.reasoning_effort,
                    tx.clone(),
                )
                .await;
            if let Err(err) = result {
                gateway.monitor.emit(
                    response_id,
                    MonitorEventKind::Failed {
                        message: err.to_string(),
                    },
                );
                let _ = tx.send(failure_event(&err)).await;
            }
        });
        Ok(ReceiverStream::new(rx))
    }

    async fn find_replay_baseline(
        &self,
        request: &ResponsesRequest,
    ) -> AppResult<(Option<ReplayRecord>, usize)> {
        if !request.store {
            return Ok((None, 0));
        }
        let record = self
            .replay_store
            .longest_prefix_match(&request.model, &request.instructions, &request.input)
            .await;
        if let Some(record) = record {
            let prefix_len = record.visible_history.len();
            return Ok((Some(record), prefix_len));
        }
        Ok((None, 0))
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_turn(
        &self,
        response_id: String,
        request: ResponsesRequest,
        mut current_messages: Vec<ChatMessage>,
        tools: Vec<crate::models::chat::ChatTool>,
        tool_registry: crate::adapters::responses_to_chat::ToolRegistry,
        response_format: Option<Value>,
        reasoning_effort: Option<String>,
        tx: mpsc::Sender<SseEvent>,
    ) -> AppResult<()> {
        self.monitor.emit(
            response_id.clone(),
            MonitorEventKind::RequestStarted {
                model: request.model.clone(),
                input_items: request.input.len(),
                tool_count: request.tools.len(),
                turn_count: request
                    .input
                    .iter()
                    .filter(|item| {
                        matches!(
                            item,
                            ResponseItem::Message {
                                role,
                                ..
                            } if role == "user"
                        )
                    })
                    .count(),
                user_messages: request
                    .input
                    .iter()
                    .filter(|item| {
                        matches!(
                            item,
                            ResponseItem::Message {
                                role,
                                ..
                            } if role == "user"
                        )
                    })
                    .count(),
                assistant_messages: request
                    .input
                    .iter()
                    .filter(|item| {
                        matches!(
                            item,
                            ResponseItem::Message {
                                role,
                                ..
                            } if role == "assistant"
                        )
                    })
                    .count(),
                system_messages: request
                    .input
                    .iter()
                    .filter(|item| {
                        matches!(
                            item,
                            ResponseItem::Message {
                                role,
                                ..
                            } if role == "system"
                        )
                    })
                    .count(),
                developer_messages: request
                    .input
                    .iter()
                    .filter(|item| {
                        matches!(
                            item,
                            ResponseItem::Message {
                                role,
                                ..
                            } if role == "developer"
                        )
                    })
                    .count(),
                reasoning_items: request
                    .input
                    .iter()
                    .filter(|item| matches!(item, ResponseItem::Reasoning { .. }))
                    .count(),
                function_calls: request
                    .input
                    .iter()
                    .filter(|item| matches!(item, ResponseItem::FunctionCall { .. }))
                    .count(),
                function_outputs: request
                    .input
                    .iter()
                    .filter(|item| matches!(item, ResponseItem::FunctionCallOutput { .. }))
                    .count(),
                tool_items: request
                    .input
                    .iter()
                    .filter(|item| {
                        matches!(
                            item,
                            ResponseItem::FunctionCall { .. }
                                | ResponseItem::FunctionCallOutput { .. }
                                | ResponseItem::CustomToolCall { .. }
                                | ResponseItem::CustomToolCallOutput { .. }
                                | ResponseItem::ToolSearchCall { .. }
                                | ResponseItem::ToolSearchOutput { .. }
                                | ResponseItem::LocalShellCall { .. }
                                | ResponseItem::WebSearchCall { .. }
                                | ResponseItem::ImageGenerationCall { .. }
                        )
                    })
                    .count(),
                input_chars: request
                    .input
                    .iter()
                    .map(|item| match item {
                        ResponseItem::Message { content, .. } => content
                            .iter()
                            .map(|content| match content {
                                crate::models::responses::ContentItem::InputText { text }
                                | crate::models::responses::ContentItem::OutputText { text } => {
                                    text.chars().count()
                                }
                                crate::models::responses::ContentItem::InputImage {
                                    image_url,
                                } => image_url.chars().count(),
                            })
                            .sum::<usize>(),
                        ResponseItem::Reasoning { content, .. } => content
                            .as_ref()
                            .map(|items| {
                                items.iter()
                                    .map(|item| match item {
                                        crate::models::responses::ReasoningContentItem::ReasoningText {
                                            text,
                                        }
                                        | crate::models::responses::ReasoningContentItem::Text {
                                            text,
                                        } => text.chars().count(),
                                    })
                                    .sum()
                            })
                            .unwrap_or(0),
                        ResponseItem::FunctionCall {
                            name, arguments, ..
                        } => name.chars().count() + arguments.chars().count(),
                        ResponseItem::FunctionCallOutput { call_id, output } => {
                            call_id.chars().count() + output.to_string().chars().count()
                        }
                        ResponseItem::CustomToolCall { name, input, .. } => {
                            name.chars().count() + input.chars().count()
                        }
                        ResponseItem::CustomToolCallOutput {
                            call_id,
                            name,
                            output,
                        } => {
                            call_id.chars().count()
                                + name.as_ref().map(|name| name.chars().count()).unwrap_or(0)
                                + output.to_string().chars().count()
                        }
                        ResponseItem::ToolSearchCall { arguments, .. } => {
                            arguments.to_string().chars().count()
                        }
                        ResponseItem::ToolSearchOutput { tools, .. } => tools
                            .iter()
                            .map(|tool| tool.to_string().chars().count())
                            .sum(),
                        ResponseItem::LocalShellCall { action, .. } => match action {
                            crate::models::responses::LocalShellAction::Exec(exec) => exec
                                .command
                                .iter()
                                .map(|part| part.chars().count())
                                .sum(),
                        },
                        ResponseItem::WebSearchCall { action, .. } => action
                            .as_ref()
                            .map(|action| match action {
                                crate::models::responses::WebSearchAction::Search {
                                    query,
                                    queries,
                                } => {
                                    query.as_ref().map(|q| q.chars().count()).unwrap_or(0)
                                        + queries
                                            .as_ref()
                                            .map(|queries| {
                                                queries
                                                    .iter()
                                                    .map(|query| query.chars().count())
                                                    .sum()
                                            })
                                            .unwrap_or(0)
                                }
                                crate::models::responses::WebSearchAction::OpenPage {
                                    url,
                                } => url.as_ref().map(|url| url.chars().count()).unwrap_or(0),
                                crate::models::responses::WebSearchAction::FindInPage {
                                    url,
                                    pattern,
                                } => {
                                    url.as_ref().map(|url| url.chars().count()).unwrap_or(0)
                                        + pattern
                                            .as_ref()
                                            .map(|pattern| pattern.chars().count())
                                            .unwrap_or(0)
                                }
                                crate::models::responses::WebSearchAction::Other => 0,
                            })
                            .unwrap_or(0),
                        ResponseItem::ImageGenerationCall {
                            revised_prompt,
                            result,
                            ..
                        } => {
                            revised_prompt
                                .as_ref()
                                .map(|text| text.chars().count())
                                .unwrap_or(0)
                                + result.chars().count()
                        }
                    })
                    .sum(),
                instructions_chars: request.instructions.chars().count(),
            },
        );
        tx.send(created_event(&response_id))
            .await
            .map_err(|_| AppError::internal("failed to send response.created"))?;
        tx.send(in_progress_event(&response_id))
            .await
            .map_err(|_| AppError::internal("failed to send response.in_progress"))?;

        let mut public_history = request.input.clone();
        let upstream_model = self
            .config
            .upstream_model
            .clone()
            .unwrap_or_else(|| request.model.clone());

        let mut accumulated_usage = AccumulatedUsage::default();
        let mut upstream_request_index = 0usize;
        let mut web_search_rounds = 0usize;
        loop {
            upstream_request_index += 1;
            let taken_messages = std::mem::take(&mut current_messages);
            let upstream_request = ChatCompletionRequest {
                model: upstream_model.clone(),
                messages: taken_messages,
                stream: true,
                tools: (!tools.is_empty()).then_some(tools.clone()),
                tool_choice: Some(request.tool_choice.clone()),
                parallel_tool_calls: false,
                reasoning_effort: reasoning_effort.clone(),
                response_format: response_format.clone(),
                stream_options: Some(StreamOptions { include_usage: true }),
                temperature: request.temperature,
                top_p: request.top_p,
                max_output_tokens: request.max_output_tokens,
                extra_body: self
                    .config
                    .upstream_chat_kwargs
                    .clone()
                    .into_iter()
                    .collect(),
            };
            self.monitor.emit(
                response_id.clone(),
                MonitorEventKind::UpstreamRequest {
                    request_index: upstream_request_index,
                    message_count: upstream_request.messages.len(),
                    prompt_chars: upstream_request
                        .messages
                        .iter()
                        .map(|message| {
                            message.role.chars().count()
                                + message
                                    .name
                                    .as_ref()
                                    .map(|name| name.chars().count())
                                    .unwrap_or(0)
                                + message
                                    .tool_call_id
                                    .as_ref()
                                    .map(|call_id| call_id.chars().count())
                                    .unwrap_or(0)
                                + message
                                    .reasoning_content
                                    .as_ref()
                                    .map(|text| text.chars().count())
                                    .unwrap_or(0)
                                + message
                                    .content
                                    .as_ref()
                                    .map(|content| content.to_string().chars().count())
                                    .unwrap_or(0)
                                + message
                                    .tool_calls
                                    .as_ref()
                                    .map(|tool_calls| {
                                        tool_calls
                                            .iter()
                                            .map(|tool_call| {
                                                serde_json::to_string(tool_call)
                                                    .unwrap_or_default()
                                                    .chars()
                                                    .count()
                                            })
                                            .sum::<usize>()
                                    })
                                    .unwrap_or(0)
                        })
                        .sum::<usize>()
                        + upstream_request
                            .tools
                            .as_ref()
                            .map(|tools| {
                                tools
                                    .iter()
                                    .map(|tool| {
                                        serde_json::to_string(tool)
                                            .unwrap_or_default()
                                            .chars()
                                            .count()
                                    })
                                    .sum::<usize>()
                            })
                            .unwrap_or(0)
                        + upstream_request
                            .extra_body
                            .values()
                            .map(|value| value.to_string().chars().count())
                            .sum::<usize>(),
                },
            );
            let mut stream = self
                .upstream
                .stream_chat_completion(&upstream_request)
                .await?;
            let mut state = StreamState::default();
            let mut turn_usage: Option<ChunkUsage> = None;
            loop {
                let chunk = match timeout(self.config.request_timeout, stream.next()).await {
                    Ok(Some(chunk)) => chunk,
                    Ok(None) => break,
                    Err(_) => return Err(AppError::upstream("upstream stream timed out".to_string())),
                };
                let chunk = chunk?;
                if chunk.usage.is_some() {
                    turn_usage = chunk.usage.clone();
                }
                for emission in state.apply_chunk(&chunk) {
                    match emission {
                        StreamEmission::OutputItemAdded(item) => {
                            self.monitor.emit(
                                response_id.clone(),
                                MonitorEventKind::ResponseItem {
                                    event: "response.output_item.added".to_string(),
                                    summary: summarize_response_item(&item),
                                    payload_preview: preview_json(&item),
                                },
                            );
                            tx.send(output_item_added_event(item)).await.map_err(|_| {
                                AppError::internal("failed to stream message start")
                            })?;
                        }
                        StreamEmission::OutputTextDelta(delta) => {
                            self.monitor.emit(
                                response_id.clone(),
                                MonitorEventKind::OutputTextDelta {
                                    delta: delta.clone(),
                                },
                            );
                            tx.send(output_text_delta_event(delta))
                                .await
                                .map_err(|_| AppError::internal("failed to stream text delta"))?;
                        }
                        StreamEmission::ReasoningItemAdded(item) => {
                            self.monitor.emit(
                                response_id.clone(),
                                MonitorEventKind::ResponseItem {
                                    event: "response.output_item.added".to_string(),
                                    summary: summarize_response_item(&item),
                                    payload_preview: preview_json(&item),
                                },
                            );
                            tx.send(output_item_added_event(item)).await.map_err(|_| {
                                AppError::internal("failed to stream reasoning start")
                            })?;
                        }
                        StreamEmission::ReasoningTextDelta(delta) => {
                            self.monitor.emit(
                                response_id.clone(),
                                MonitorEventKind::ReasoningTextDelta {
                                    delta: delta.clone(),
                                },
                            );
                            tx.send(reasoning_text_delta_event(delta))
                                .await
                                .map_err(|_| {
                                    AppError::internal("failed to stream reasoning delta")
                                })?;
                        }
                        StreamEmission::FunctionCallArgumentsDelta { call_id, delta } => {
                            tx.send(function_call_args_delta_event(call_id, delta))
                                .await
                                .map_err(|_| {
                                    AppError::internal(
                                        "failed to stream function call args delta",
                                    )
                                })?;
                        }
                    }
                }
            }
            if let Some(usage) = turn_usage {
                accumulated_usage.add(usage);
            }
            let finalized = state.finalize(&tool_registry)?;
            current_messages = upstream_request.messages;
            self.emit_completed_public_items(&response_id, &tx, &finalized, &mut public_history)
                .await?;
            if let Some(message) = finalized.internal_assistant_message.clone() {
                current_messages.push(message);
            }
            if finalized.tool_calls.is_empty() {
                break;
            }
            self.handle_tool_calls(
                &response_id,
                &finalized,
                &tx,
                &mut current_messages,
                &mut public_history,
            )
            .await?;
            if self.config.brave_api_key.is_some()
                && finalized
                    .tool_calls
                    .iter()
                    .all(|call| matches!(call.kind, ToolKind::WebSearch))
            {
                web_search_rounds += 1;
                if self.config.max_web_search_rounds > 0
                    && web_search_rounds >= self.config.max_web_search_rounds
                {
                    return Err(AppError::upstream("web search round limit exceeded"));
                }
                continue;
            }
            break;
        }

        if request.store {
            self.replay_store
                .insert(ReplayRecord {
                    model: request.model,
                    instructions: request.instructions,
                    visible_history: public_history,
                    internal_messages: current_messages,
                })
                .await;
        }

        tx.send(completed_event(&response_id, accumulated_usage.into_response_usage()))
            .await
            .map_err(|_| AppError::internal("failed to send response.completed"))?;
        self.monitor.emit(response_id, MonitorEventKind::Completed);
        Ok(())
    }

    async fn emit_completed_public_items(
        &self,
        response_id: &str,
        tx: &mpsc::Sender<SseEvent>,
        finalized: &FinalizedAssistantTurn,
        public_history: &mut Vec<ResponseItem>,
    ) -> AppResult<()> {
        if let Some(reasoning) = finalized.reasoning_item.clone() {
            public_history.push(reasoning.clone());
            self.monitor.emit(
                response_id.to_string(),
                MonitorEventKind::ResponseItem {
                    event: "response.output_item.done".to_string(),
                    summary: summarize_response_item(&reasoning),
                    payload_preview: preview_json(&reasoning),
                },
            );
            tx.send(output_item_done_event(reasoning))
                .await
                .map_err(|_| AppError::internal("failed to send reasoning done"))?;
        }
        if let Some(message) = finalized.message_item.clone() {
            if let ResponseItem::Message { ref content, .. } = message {
                let full_text: String = content
                    .iter()
                    .filter_map(|c| match c {
                        crate::models::responses::ContentItem::OutputText { text } => {
                            Some(text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !full_text.is_empty() {
                    tx.send(output_text_done_event(full_text))
                        .await
                        .map_err(|_| AppError::internal("failed to send output_text.done"))?;
                }
            }
            public_history.push(message.clone());
            self.monitor.emit(
                response_id.to_string(),
                MonitorEventKind::ResponseItem {
                    event: "response.output_item.done".to_string(),
                    summary: summarize_response_item(&message),
                    payload_preview: preview_json(&message),
                },
            );
            tx.send(output_item_done_event(message))
                .await
                .map_err(|_| AppError::internal("failed to send message done"))?;
        }
        Ok(())
    }

    async fn handle_tool_calls(
        &self,
        response_id: &str,
        finalized: &FinalizedAssistantTurn,
        tx: &mpsc::Sender<SseEvent>,
        current_messages: &mut Vec<ChatMessage>,
        public_history: &mut Vec<ResponseItem>,
    ) -> AppResult<()> {
        let can_search = self.config.brave_api_key.is_some();
        let has_web_search = can_search
            && finalized
                .tool_calls
                .iter()
                .any(|call| matches!(call.kind, ToolKind::WebSearch));
        let has_client_tool = finalized
            .tool_calls
            .iter()
            .any(|call| !matches!(call.kind, ToolKind::WebSearch) || !can_search);
        if has_web_search && has_client_tool {
            return Err(AppError::upstream(
                "mixed provider-side and client-side tool calls are not supported in v1",
            ));
        }
        if has_client_tool {
            for tool_call in &finalized.tool_calls {
                if let ResponseItem::FunctionCall {
                    ref call_id,
                    ref name,
                    ref arguments,
                    ..
                } = tool_call.public_item
                {
                    tx.send(function_call_args_done_event(
                        call_id.clone(),
                        name.clone(),
                        arguments.clone(),
                    ))
                    .await
                    .map_err(|_| {
                        AppError::internal("failed to send function call args done")
                    })?;
                }
                self.monitor.emit(
                    response_id.to_string(),
                    MonitorEventKind::ToolPhase {
                        phase: "client_tool_handoff".to_string(),
                        detail: summarize_response_item(&tool_call.public_item),
                    },
                );
                public_history.push(tool_call.public_item.clone());
                self.monitor.emit(
                    response_id.to_string(),
                    MonitorEventKind::ResponseItem {
                        event: "response.output_item.done".to_string(),
                        summary: summarize_response_item(&tool_call.public_item),
                        payload_preview: preview_json(&tool_call.public_item),
                    },
                );
                tx.send(output_item_done_event(tool_call.public_item.clone()))
                    .await
                    .map_err(|_| AppError::internal("failed to send tool call item"))?;
            }
            return Ok(());
        }
        for tool_call in &finalized.tool_calls {
            self.run_web_search(response_id, tool_call, tx, current_messages, public_history)
                .await?;
        }
        Ok(())
    }

    async fn run_web_search(
        &self,
        response_id: &str,
        tool_call: &ResolvedToolCall,
        tx: &mpsc::Sender<SseEvent>,
        current_messages: &mut Vec<ChatMessage>,
        public_history: &mut Vec<ResponseItem>,
    ) -> AppResult<()> {
        let ResponseItem::WebSearchCall {
            id,
            status: _,
            action,
        } = &tool_call.public_item
        else {
            return Err(AppError::internal("expected web_search_call item"));
        };
        let partial = ResponseItem::WebSearchCall {
            id: id.clone(),
            status: Some("in_progress".to_string()),
            action: None,
        };
        self.monitor.emit(
            response_id.to_string(),
            MonitorEventKind::ToolPhase {
                phase: "provider_tool_detected".to_string(),
                detail: summarize_response_item(&tool_call.public_item),
            },
        );
        self.monitor.emit(
            response_id.to_string(),
            MonitorEventKind::ResponseItem {
                event: "response.output_item.added".to_string(),
                summary: summarize_response_item(&partial),
                payload_preview: preview_json(&partial),
            },
        );
        tx.send(output_item_added_event(partial))
            .await
            .map_err(|_| AppError::internal("failed to send web_search start"))?;

        let query = extract_web_search_query(action, &tool_call.arguments)?;
        self.monitor.emit(
            response_id.to_string(),
            MonitorEventKind::ToolPhase {
                phase: "provider_tool_running".to_string(),
                detail: format!("web_search {query}"),
            },
        );
        let results = self.search.search(&query).await?;

        let completed = ResponseItem::WebSearchCall {
            id: id.clone(),
            status: Some("completed".to_string()),
            action: action.clone(),
        };
        public_history.push(completed.clone());
        self.monitor.emit(
            response_id.to_string(),
            MonitorEventKind::ResponseItem {
                event: "response.output_item.done".to_string(),
                summary: summarize_response_item(&completed),
                payload_preview: preview_json(&completed),
            },
        );
        tx.send(output_item_done_event(completed))
            .await
            .map_err(|_| AppError::internal("failed to send web_search done"))?;
        self.monitor.emit(
            response_id.to_string(),
            MonitorEventKind::ToolPhase {
                phase: "provider_tool_completed".to_string(),
                detail: format!("web_search result {}", preview_text(&results)),
            },
        );

        current_messages.push(ChatMessage {
            role: "tool".to_string(),
            content: Some(Value::String(results)),
            tool_call_id: tool_call.internal_call.id.clone(),
            name: None,
            reasoning_content: None,
            tool_calls: None,
        });
        Ok(())
    }
}

fn preview_json<T>(value: &T) -> String
where
    T: Serialize,
{
    const LIMIT: usize = 4_000;
    let rendered = serde_json::to_string_pretty(value)
        .unwrap_or_else(|err| format!("{{\"serialization_error\":\"{err}\"}}"));
    if rendered.chars().count() <= LIMIT {
        rendered
    } else {
        let end = rendered
            .char_indices()
            .nth(LIMIT)
            .map(|(index, _)| index)
            .unwrap_or(rendered.len());
        format!("{}...\n[truncated]", &rendered[..end])
    }
}

fn summarize_response_item(item: &ResponseItem) -> String {
    match item {
        ResponseItem::Message { role, content, .. } => {
            format!("{role}: {}", summarize_content(content))
        }
        ResponseItem::Reasoning { content, .. } => content
            .as_ref()
            .and_then(|items| items.first())
            .map(|item| match item {
                crate::models::responses::ReasoningContentItem::ReasoningText { text }
                | crate::models::responses::ReasoningContentItem::Text { text } => {
                    format!("reasoning: {}", preview_text(text))
                }
            })
            .unwrap_or_else(|| "reasoning".to_string()),
        ResponseItem::FunctionCall {
            name, arguments, ..
        } => {
            format!("function_call {name} {}", preview_text(arguments))
        }
        ResponseItem::FunctionCallOutput { call_id, output } => {
            format!(
                "function_call_output {call_id} {}",
                preview_text(&output.to_string())
            )
        }
        ResponseItem::CustomToolCall { name, input, .. } => {
            format!("custom_tool_call {name} {}", preview_text(input))
        }
        ResponseItem::CustomToolCallOutput {
            call_id, output, ..
        } => {
            format!(
                "custom_tool_call_output {call_id} {}",
                preview_text(&output.to_string())
            )
        }
        ResponseItem::ToolSearchCall { arguments, .. } => {
            format!("tool_search_call {}", preview_text(&arguments.to_string()))
        }
        ResponseItem::ToolSearchOutput { tools, .. } => {
            format!("tool_search_output {} tools", tools.len())
        }
        ResponseItem::LocalShellCall { action, .. } => match action {
            crate::models::responses::LocalShellAction::Exec(exec) => {
                format!("local_shell {}", exec.command.join(" "))
            }
        },
        ResponseItem::WebSearchCall { action, .. } => match action {
            Some(crate::models::responses::WebSearchAction::Search { query, .. }) => {
                format!("web_search {}", query.clone().unwrap_or_default())
            }
            Some(_) => "web_search".to_string(),
            None => "web_search in_progress".to_string(),
        },
        ResponseItem::ImageGenerationCall { id, .. } => format!("image_generation_call {id}"),
    }
}

fn summarize_content(content: &[crate::models::responses::ContentItem]) -> String {
    let mut text = String::new();
    for item in content {
        match item {
            crate::models::responses::ContentItem::InputText { text: item_text }
            | crate::models::responses::ContentItem::OutputText { text: item_text } => {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str(item_text);
            }
            crate::models::responses::ContentItem::InputImage { .. } => {
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push_str("[image]");
            }
        }
    }
    preview_text(&text)
}

fn preview_text(text: &str) -> String {
    const LIMIT: usize = 120;
    if text.chars().count() <= LIMIT {
        text.to_string()
    } else {
        let end = text
            .char_indices()
            .nth(LIMIT)
            .map(|(index, _)| index)
            .unwrap_or(text.len());
        format!("{}...", &text[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::preview_json;
    use super::preview_text;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn preview_text_truncates_on_char_boundary() {
        let text = format!("{}é", "a".repeat(119));
        assert_eq!(preview_text(&text), format!("{}é", "a".repeat(119)));

        let text = format!("{}éβ", "a".repeat(119));
        assert_eq!(preview_text(&text), format!("{}é...", "a".repeat(119)));
    }

    #[test]
    fn preview_json_truncates_on_char_boundary() {
        let value = json!({ "text": format!("{}éβ", "a".repeat(4_100)) });
        let preview = preview_json(&value);
        assert!(preview.ends_with("...\n[truncated]"));
        assert!(preview.is_char_boundary(preview.len()));
    }

    use super::AccumulatedUsage;
    use super::failure_event;
    use crate::models::chat::ChunkUsage;

    #[test]
    fn accumulated_usage_cached_tokens() {
        let mut usage = AccumulatedUsage::default();
        usage.add(ChunkUsage {
            prompt_tokens: 100,
            completion_tokens: 25,
            total_tokens: 125,
            prompt_tokens_details: Some(crate::models::chat::PromptTokensDetails {
                cached_tokens: 50,
            }),
            completion_tokens_details: None,
        });
        let result = usage.into_response_usage().unwrap();
        assert_eq!(result.input_tokens, 100);
        assert_eq!(result.input_tokens_details.unwrap().cached_tokens, 50);
    }

    #[test]
    fn accumulated_usage_reasoning_tokens() {
        let mut usage = AccumulatedUsage::default();
        usage.add(ChunkUsage {
            prompt_tokens: 100,
            completion_tokens: 25,
            total_tokens: 125,
            prompt_tokens_details: None,
            completion_tokens_details: Some(crate::models::chat::CompletionTokensDetails {
                reasoning_tokens: 30,
            }),
        });
        let result = usage.into_response_usage().unwrap();
        assert_eq!(result.output_tokens, 25);
        assert_eq!(result.output_tokens_details.unwrap().reasoning_tokens, 30);
    }

    #[test]
    fn accumulated_usage_zero_returns_none() {
        let usage = AccumulatedUsage::default();
        assert!(usage.into_response_usage().is_none());
    }

    use super::extract_web_search_query;
    use crate::models::responses::WebSearchAction;

    #[test]
    fn test_run_web_search_rejects_open_page() {
        let action = Some(WebSearchAction::OpenPage {
            url: Some("https://example.com".to_string()),
        });
        let args = json!({"query": "test"});
        let result = extract_web_search_query(&action, &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unsupported web_search action"));
    }

    #[test]
    fn test_run_web_search_rejects_find_in_page() {
        let action = Some(WebSearchAction::FindInPage {
            url: Some("https://example.com".to_string()),
            pattern: Some("test".to_string()),
        });
        let args = json!({"query": "test"});
        let result = extract_web_search_query(&action, &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unsupported web_search action"));
    }

    #[test]
    fn test_run_web_search_rejects_other_action() {
        let action = Some(WebSearchAction::Other);
        let args = json!({"query": "test"});
        let result = extract_web_search_query(&action, &args);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unsupported web_search action"));
    }

    #[test]
    fn test_extract_web_search_query_from_action() {
        let action = Some(WebSearchAction::Search {
            query: Some("rust async".to_string()),
            queries: None,
        });
        let args = json!({});
        let result = extract_web_search_query(&action, &args).unwrap();
        assert_eq!(result, "rust async");
    }

    #[test]
    fn test_extract_web_search_query_fallback_to_arguments() {
        let action = None;
        let args = json!({"query": "fallback query"});
        let result = extract_web_search_query(&action, &args).unwrap();
        assert_eq!(result, "fallback query");
    }

    #[test]
    fn test_extract_web_search_query_search_action_none_query_falls_back() {
        let action = Some(WebSearchAction::Search {
            query: None,
            queries: None,
        });
        let args = json!({"query": "from args"});
        let result = extract_web_search_query(&action, &args).unwrap();
        assert_eq!(result, "from args");
    }

    #[test]
    fn test_max_web_search_rounds_default() {
        let config = crate::config::Config::from_persisted(&crate::config::PersistedConfig::default()).unwrap();
        assert_eq!(config.max_web_search_rounds, 5);
    }

    #[test]
    fn failure_event_shape() {
        let error = crate::error::AppError::internal("test error");
        let event = failure_event(&error);
        assert_eq!(event.event, "response.failed");
        assert_eq!(event.data["type"], "response.failed");
        assert_eq!(event.data["response"]["error"]["code"], "gateway_error");
        assert!(event.data["response"]["error"]["message"]
            .as_str()
            .unwrap()
            .contains("test error"));
    }
}

fn extract_web_search_query(
    action: &Option<WebSearchAction>,
    arguments: &Value,
) -> AppResult<String> {
    match action {
        Some(WebSearchAction::Search { query, .. }) => {
            if let Some(q) = query {
                Ok(q.clone())
            } else {
                arguments
                    .get("query")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .ok_or_else(|| AppError::upstream("web_search call missing query"))
            }
        }
        Some(WebSearchAction::OpenPage { .. })
        | Some(WebSearchAction::FindInPage { .. })
        | Some(WebSearchAction::Other) => {
            Err(AppError::upstream("unsupported web_search action"))
        }
        None => arguments
            .get("query")
            .and_then(Value::as_str)
            .map(String::from)
            .ok_or_else(|| AppError::upstream("web_search call missing query")),
    }
}

fn created_event(response_id: &str) -> SseEvent {
    json_event(
        "response.created",
        ResponsesEnvelope {
            kind: "response.created".to_string(),
            payload: ResponseCreatedPayload {
                response: ResponseStub {
                    id: response_id.to_string(),
                },
            },
        },
    )
}

fn completed_event(response_id: &str, usage: Option<ResponseUsage>) -> SseEvent {
    json_event(
        "response.completed",
        ResponsesEnvelope {
            kind: "response.completed".to_string(),
            payload: ResponseCompletedPayload {
                response: ResponseCompleted {
                    id: response_id.to_string(),
                    usage,
                },
            },
        },
    )
}

#[derive(Default)]
struct AccumulatedUsage {
    input_tokens: i64,
    output_tokens: i64,
    total_tokens: i64,
    cached_input_tokens: i64,
    reasoning_output_tokens: i64,
}

impl AccumulatedUsage {
    fn add(&mut self, usage: ChunkUsage) {
        self.input_tokens += usage.prompt_tokens;
        self.output_tokens += usage.completion_tokens;
        self.total_tokens += usage.total_tokens;
        self.cached_input_tokens += usage
            .prompt_tokens_details
            .map(|d| d.cached_tokens)
            .unwrap_or(0);
        self.reasoning_output_tokens += usage
            .completion_tokens_details
            .map(|d| d.reasoning_tokens)
            .unwrap_or(0);
    }

    fn into_response_usage(self) -> Option<ResponseUsage> {
        if self.total_tokens == 0 {
            return None;
        }
        Some(ResponseUsage {
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            total_tokens: self.total_tokens,
            input_tokens_details: (self.cached_input_tokens > 0).then_some(
                ResponseInputTokensDetails {
                    cached_tokens: self.cached_input_tokens,
                },
            ),
            output_tokens_details: (self.reasoning_output_tokens > 0).then_some(
                ResponseOutputTokensDetails {
                    reasoning_tokens: self.reasoning_output_tokens,
                },
            ),
        })
    }
}

fn output_item_added_event(item: ResponseItem) -> SseEvent {
    json_event(
        "response.output_item.added",
        ResponsesEnvelope {
            kind: "response.output_item.added".to_string(),
            payload: OutputItemPayload { item },
        },
    )
}

fn output_item_done_event(item: ResponseItem) -> SseEvent {
    json_event(
        "response.output_item.done",
        ResponsesEnvelope {
            kind: "response.output_item.done".to_string(),
            payload: OutputItemPayload { item },
        },
    )
}

fn output_text_delta_event(delta: String) -> SseEvent {
    json_event(
        "response.output_text.delta",
        ResponsesEnvelope {
            kind: "response.output_text.delta".to_string(),
            payload: DeltaPayload { delta },
        },
    )
}

fn reasoning_text_delta_event(delta: String) -> SseEvent {
    json_event(
        "response.reasoning_summary_text.delta",
        ResponsesEnvelope {
            kind: "response.reasoning_summary_text.delta".to_string(),
            payload: ReasoningDeltaPayload {
                delta,
                content_index: 0,
            },
        },
    )
}

fn failure_event(error: &AppError) -> SseEvent {
    json_event(
        "response.failed",
        ResponsesEnvelope {
            kind: "response.failed".to_string(),
            payload: FailedPayload {
                response: FailedResponse {
                    error: FailedError {
                        code: "gateway_error".to_string(),
                        message: error.to_string(),
                    },
                },
            },
        },
    )
}

fn in_progress_event(response_id: &str) -> SseEvent {
    json_event(
        "response.in_progress",
        ResponsesEnvelope {
            kind: "response.in_progress".to_string(),
            payload: ResponseCreatedPayload {
                response: ResponseStub {
                    id: response_id.to_string(),
                },
            },
        },
    )
}

fn output_text_done_event(text: String) -> SseEvent {
    json_event(
        "response.output_text.done",
        ResponsesEnvelope {
            kind: "response.output_text.done".to_string(),
            payload: crate::models::responses::TextDonePayload {
                text,
                content_index: 0,
            },
        },
    )
}

fn function_call_args_delta_event(call_id: String, delta: String) -> SseEvent {
    json_event(
        "response.function_call_arguments.delta",
        ResponsesEnvelope {
            kind: "response.function_call_arguments.delta".to_string(),
            payload: crate::models::responses::FunctionCallArgsDeltaPayload { call_id, delta },
        },
    )
}

fn function_call_args_done_event(call_id: String, name: String, arguments: String) -> SseEvent {
    json_event(
        "response.function_call_arguments.done",
        ResponsesEnvelope {
            kind: "response.function_call_arguments.done".to_string(),
            payload: crate::models::responses::FunctionCallArgsDonePayload {
                call_id,
                name,
                arguments,
            },
        },
    )
}

fn json_event<T>(event: &str, payload: T) -> SseEvent
where
    T: Serialize,
{
    SseEvent {
        event: event.to_string(),
        data: serde_json::to_value(payload).unwrap_or(Value::Null),
    }
}
