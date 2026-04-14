use serde::Serialize;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;
use tokio::sync::broadcast;

#[derive(Debug, Clone, Serialize)]
pub enum MonitorEventKind {
    RequestStarted {
        model: String,
        input_items: usize,
        tool_count: usize,
        turn_count: usize,
        user_messages: usize,
        assistant_messages: usize,
        system_messages: usize,
        developer_messages: usize,
        reasoning_items: usize,
        function_calls: usize,
        function_outputs: usize,
        tool_items: usize,
        input_chars: usize,
        instructions_chars: usize,
    },
    UpstreamRequest {
        request_index: usize,
        message_count: usize,
        prompt_chars: usize,
    },
    ResponseItem {
        event: String,
        summary: String,
        payload_preview: String,
    },
    OutputTextDelta {
        delta: String,
    },
    ReasoningTextDelta {
        delta: String,
    },
    ToolPhase {
        phase: String,
        detail: String,
    },
    Completed,
    Failed {
        message: String,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorEvent {
    pub response_id: String,
    pub timestamp_ms: u128,
    pub kind: MonitorEventKind,
}

#[derive(Clone)]
pub struct MonitorHub {
    tx: broadcast::Sender<MonitorEvent>,
}

impl MonitorHub {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self { tx }
    }

    pub fn emit(&self, response_id: impl Into<String>, kind: MonitorEventKind) {
        let _ = self.tx.send(MonitorEvent {
            response_id: response_id.into(),
            timestamp_ms: now_ms(),
            kind,
        });
    }

    pub fn subscribe(&self) -> broadcast::Receiver<MonitorEvent> {
        self.tx.subscribe()
    }
}

fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0)
}
