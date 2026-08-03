//! Projection from Codex's private Responses-Lite stream into the public
//! Responses lifecycle used inside llmconduit.
//!
//! The private stream deliberately omits several fields that are mandatory on
//! the public wire. In particular, Codex accepts text deltas without item or
//! content indexes, function argument deltas without a call id/output index,
//! and completed items without either an item id or output index. This module
//! owns the state needed to fill those fields once, consistently, rather than
//! teaching every downstream converter about the private dialect.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::models::responses::{ContentItem, ReasoningSummaryItem, ResponseItem};

const DEFAULT_MAX_FUNCTION_ARGUMENT_BYTES: usize = 8 * 1024 * 1024;
const DEFAULT_MAX_FUNCTION_EVENTS: usize = 16 * 1024;
const DEFAULT_MAX_TOTAL_FUNCTION_BYTES: usize = 32 * 1024 * 1024;
const DEFAULT_MAX_OPEN_ITEMS: usize = 4096;
const DEFAULT_MAX_TEXT_BYTES: usize = 32 * 1024 * 1024;

/// Memory and cardinality ceilings for one private Responses stream.
#[derive(Debug, Clone, Copy)]
pub struct ProjectionLimits {
    pub max_function_argument_bytes: usize,
    pub max_function_events: usize,
    pub max_total_function_bytes: usize,
    pub max_open_items: usize,
    pub max_text_bytes: usize,
}

impl Default for ProjectionLimits {
    fn default() -> Self {
        Self {
            max_function_argument_bytes: DEFAULT_MAX_FUNCTION_ARGUMENT_BYTES,
            max_function_events: DEFAULT_MAX_FUNCTION_EVENTS,
            max_total_function_bytes: DEFAULT_MAX_TOTAL_FUNCTION_BYTES,
            max_open_items: DEFAULT_MAX_OPEN_ITEMS,
            max_text_bytes: DEFAULT_MAX_TEXT_BYTES,
        }
    }
}

/// One normalized canonical event. The engine can convert this directly into
/// its SSE event wrapper without exposing the private transport type here.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedEvent {
    pub event: String,
    pub data: Value,
}

impl ProjectedEvent {
    fn new(event: impl Into<String>, data: Value) -> Self {
        Self {
            event: event.into(),
            data,
        }
    }
}

/// A completed canonical output item and the stable output index assigned to
/// it. Callers use this instead of reparsing provider terminal resources.
#[derive(Debug, Clone, PartialEq)]
pub struct CompletedOutputItem {
    pub output_index: usize,
    pub item: ResponseItem,
}

/// Result of projecting one private event. A single private event can expand
/// into a complete public lifecycle (for example, a terminal-only function
/// item becomes added -> argument delta/done -> item done).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Projection {
    pub events: Vec<ProjectedEvent>,
    pub completed_items: Vec<CompletedOutputItem>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Message,
    Reasoning,
    Function,
}

impl ItemKind {
    fn prefix(self) -> &'static str {
        match self {
            Self::Message => "msg",
            Self::Reasoning => "rs",
            Self::Function => "fc",
        }
    }
}

#[derive(Debug, Clone)]
struct Slot {
    id: String,
    output_index: usize,
    kind: ItemKind,
    synthetic_id: bool,
}

#[derive(Debug, Default)]
struct MessageState {
    added: bool,
    part_added: bool,
    text_done: bool,
    part_done: bool,
    text: String,
}

#[derive(Debug, Default)]
struct SummaryState {
    part_added: bool,
    text_done: bool,
    part_done: bool,
    text: String,
}

#[derive(Debug, Default)]
struct ReasoningState {
    added: bool,
    summaries: BTreeMap<usize, SummaryState>,
}

#[derive(Debug, Default)]
struct FunctionState {
    added_seen: bool,
    name: Option<String>,
    call_id: Option<String>,
    deltas: Vec<String>,
    done_arguments: Option<String>,
    event_count: usize,
    retained_bytes: usize,
}

/// Stateful projector for one upstream response stream.
#[derive(Debug)]
pub struct CodexPrivateResponsesProjector {
    limits: ProjectionLimits,
    next_output_index: usize,
    slots: HashMap<String, Slot>,
    item_by_output_index: HashMap<usize, String>,
    completed_item_ids: HashSet<String>,
    messages: HashMap<String, MessageState>,
    reasoning: HashMap<String, ReasoningState>,
    functions: HashMap<String, FunctionState>,
    total_function_bytes: usize,
}

impl Default for CodexPrivateResponsesProjector {
    fn default() -> Self {
        Self::with_limits(ProjectionLimits::default())
    }
}

impl CodexPrivateResponsesProjector {
    pub fn with_limits(limits: ProjectionLimits) -> Self {
        Self {
            limits,
            next_output_index: 0,
            slots: HashMap::new(),
            item_by_output_index: HashMap::new(),
            completed_item_ids: HashSet::new(),
            messages: HashMap::new(),
            reasoning: HashMap::new(),
            functions: HashMap::new(),
            total_function_bytes: 0,
        }
    }

    /// Project one event from the private stream. Unknown non-lifecycle events
    /// are passed through unchanged; the caller remains responsible for its
    /// surface allowlist and terminal-resource policy.
    pub fn project(&mut self, event: impl Into<String>, data: Value) -> AppResult<Projection> {
        let event = event.into();
        match event.as_str() {
            "response.output_item.added" => self.project_item_added(data),
            "response.output_item.done" => self.project_item_done(data),
            "response.output_text.delta" => self.project_text_delta(data),
            "response.output_text.done" => self.project_text_done(data),
            "response.content_part.added" => self.project_content_part_added(data),
            "response.content_part.done" => self.project_content_part_done(data),
            "response.function_call_arguments.delta" => self.project_function_delta(data),
            "response.function_call_arguments.done" => self.project_function_done(data),
            "response.reasoning_summary_part.added" => self.project_summary_part_added(data),
            "response.reasoning_summary_text.delta" => self.project_summary_delta(data),
            "response.reasoning_summary_text.done" => self.project_summary_text_done(data),
            "response.reasoning_summary_part.done" => self.project_summary_part_done(data),
            _ => Ok(Projection {
                events: vec![ProjectedEvent::new(event, data)],
                completed_items: Vec::new(),
            }),
        }
    }

    /// Reject a terminal stream while any quarantined function call remains
    /// unresolved. Message/reasoning items may legitimately be absent, but a
    /// partial executable call must never escape as a completed turn.
    pub fn finish(&self) -> AppResult<()> {
        if self.functions.is_empty() {
            Ok(())
        } else {
            Err(projection_error(
                "private Responses stream ended with an unfinished function call",
            ))
        }
    }

    fn project_item_added(&mut self, mut data: Value) -> AppResult<Projection> {
        let item = data
            .get("item")
            .cloned()
            .ok_or_else(|| projection_error("output_item.added omitted item"))?;
        let kind = item_kind(&item)?;
        let generated_id;
        let generated = item.get("id").and_then(Value::as_str).is_none();
        let item_id = match item.get("id").and_then(Value::as_str) {
            Some(id) => Some(id),
            None => {
                generated_id = format!("{}_{}", kind.prefix(), Uuid::new_v4().simple());
                Some(generated_id.as_str())
            }
        };
        let mut output_index = optional_index(&data, "output_index")?;
        if generated && output_index.is_none() {
            output_index = Some(self.next_free_output_index());
        }
        let mut slot = self.resolve_slot(kind, item_id, output_index, true)?;
        if generated {
            slot.synthetic_id = true;
            if let Some(stored) = self.slots.get_mut(&slot.id) {
                stored.synthetic_id = true;
            }
        }
        set_item_id(&mut data, &slot.id)?;
        set_index(&mut data, "output_index", slot.output_index);

        match kind {
            ItemKind::Message => {
                let state = self.messages.entry(slot.id.clone()).or_default();
                if state.added {
                    return Err(projection_error("message output item was added twice"));
                }
                state.added = true;
                Ok(single_event("response.output_item.added", data))
            }
            ItemKind::Reasoning => {
                let state = self.reasoning.entry(slot.id.clone()).or_default();
                if state.added {
                    return Err(projection_error("reasoning output item was added twice"));
                }
                state.added = true;
                Ok(single_event("response.output_item.added", data))
            }
            ItemKind::Function => {
                let state = self.functions.entry(slot.id.clone()).or_default();
                if state.added_seen {
                    return Err(projection_error("function output item was added twice"));
                }
                state.added_seen = true;
                state.name = item.get("name").and_then(Value::as_str).map(str::to_string);
                state.call_id = item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                self.charge_function_event(&slot.id, 0)?;
                // Function events stay quarantined until the complete item is
                // validated by the engine's offered-tool/schema registry.
                Ok(Projection::default())
            }
        }
    }

    fn project_text_delta(&mut self, mut data: Value) -> AppResult<Projection> {
        let delta = required_string(&data, "delta", "output_text.delta")?.to_string();
        let slot = self.resolve_event_slot(ItemKind::Message, &data, true)?;
        let content_index = optional_index(&data, "content_index")?.unwrap_or(0);
        if content_index != 0 {
            return Err(projection_error(
                "private text lifecycle used more than one content part",
            ));
        }
        let mut events = Vec::new();
        self.ensure_message_started(&slot, &mut events, true)?;
        let state = self.messages.get_mut(&slot.id).expect("message state");
        if state.text_done || state.part_done {
            return Err(projection_error("text delta followed its done event"));
        }
        if state.text.len().saturating_add(delta.len()) > self.limits.max_text_bytes {
            return Err(projection_error(
                "private response text exceeded its memory limit",
            ));
        }
        state.text.push_str(&delta);
        set_identity(&mut data, &slot, Some(content_index));
        events.push(ProjectedEvent::new("response.output_text.delta", data));
        Ok(Projection {
            events,
            completed_items: Vec::new(),
        })
    }

    fn project_text_done(&mut self, mut data: Value) -> AppResult<Projection> {
        let text = required_string(&data, "text", "output_text.done")?.to_string();
        let slot = self.resolve_event_slot(ItemKind::Message, &data, false)?;
        let content_index = optional_index(&data, "content_index")?.unwrap_or(0);
        let mut events = Vec::new();
        self.ensure_message_started(&slot, &mut events, true)?;
        let state = self.messages.get_mut(&slot.id).expect("message state");
        if state.text_done {
            return Err(projection_error("text done was emitted twice"));
        }
        reconcile_text(&state.text, &text, "text delta and done content differed")?;
        state.text = text;
        state.text_done = true;
        set_identity(&mut data, &slot, Some(content_index));
        events.push(ProjectedEvent::new("response.output_text.done", data));
        Ok(Projection {
            events,
            completed_items: Vec::new(),
        })
    }

    fn project_content_part_added(&mut self, mut data: Value) -> AppResult<Projection> {
        if data.pointer("/part/type").and_then(Value::as_str) != Some("output_text") {
            return Ok(single_event("response.content_part.added", data));
        }
        let slot = self.resolve_event_slot(ItemKind::Message, &data, true)?;
        let content_index = optional_index(&data, "content_index")?.unwrap_or(0);
        let mut events = Vec::new();
        self.ensure_message_started(&slot, &mut events, false)?;
        let state = self.messages.get_mut(&slot.id).expect("message state");
        if state.part_added {
            return Err(projection_error("text content part was added twice"));
        }
        state.part_added = true;
        set_identity(&mut data, &slot, Some(content_index));
        events.push(ProjectedEvent::new("response.content_part.added", data));
        Ok(Projection {
            events,
            completed_items: Vec::new(),
        })
    }

    fn project_content_part_done(&mut self, mut data: Value) -> AppResult<Projection> {
        if data.pointer("/part/type").and_then(Value::as_str) != Some("output_text") {
            return Ok(single_event("response.content_part.done", data));
        }
        let slot = self.resolve_event_slot(ItemKind::Message, &data, false)?;
        let content_index = optional_index(&data, "content_index")?.unwrap_or(0);
        let text = data
            .pointer("/part/text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let mut events = Vec::new();
        self.ensure_message_started(&slot, &mut events, true)?;
        let state = self.messages.get_mut(&slot.id).expect("message state");
        if state.part_done {
            return Err(projection_error("text content part was completed twice"));
        }
        reconcile_text(&state.text, &text, "text content part differed from deltas")?;
        state.text = text;
        state.part_done = true;
        set_identity(&mut data, &slot, Some(content_index));
        events.push(ProjectedEvent::new("response.content_part.done", data));
        Ok(Projection {
            events,
            completed_items: Vec::new(),
        })
    }

    fn project_function_delta(&mut self, data: Value) -> AppResult<Projection> {
        let delta = required_string(&data, "delta", "function argument delta")?.to_string();
        let slot = self.resolve_event_slot(ItemKind::Function, &data, false)?;
        self.charge_function_event(&slot.id, delta.len())?;
        let state = self.functions.get_mut(&slot.id).expect("function state");
        if state.done_arguments.is_some() {
            return Err(projection_error("function argument delta followed done"));
        }
        state.deltas.push(delta);
        Ok(Projection::default())
    }

    fn project_function_done(&mut self, data: Value) -> AppResult<Projection> {
        let arguments = required_string(&data, "arguments", "function arguments done")?.to_string();
        let slot = self.resolve_event_slot(ItemKind::Function, &data, false)?;
        self.charge_function_event(&slot.id, arguments.len())?;
        let state = self.functions.get_mut(&slot.id).expect("function state");
        if state.done_arguments.replace(arguments).is_some() {
            return Err(projection_error(
                "function arguments done was emitted twice",
            ));
        }
        merge_known_string(&mut state.name, data.get("name"), "function name")?;
        merge_known_string(&mut state.call_id, data.get("call_id"), "function call_id")?;
        Ok(Projection::default())
    }

    fn project_summary_part_added(&mut self, mut data: Value) -> AppResult<Projection> {
        let slot = self.resolve_event_slot(ItemKind::Reasoning, &data, true)?;
        let summary_index = optional_index(&data, "summary_index")?.unwrap_or(0);
        let mut events = Vec::new();
        self.ensure_reasoning_started(&slot, &mut events)?;
        let summary = self
            .reasoning
            .get_mut(&slot.id)
            .expect("reasoning state")
            .summaries
            .entry(summary_index)
            .or_default();
        if summary.part_added {
            return Err(projection_error("reasoning summary part was added twice"));
        }
        summary.part_added = true;
        set_reasoning_identity(&mut data, &slot, summary_index);
        data.as_object_mut()
            .expect("event object")
            .entry("part".to_string())
            .or_insert_with(|| json!({"type":"summary_text","text":""}));
        events.push(ProjectedEvent::new(
            "response.reasoning_summary_part.added",
            data,
        ));
        Ok(Projection {
            events,
            completed_items: Vec::new(),
        })
    }

    fn project_summary_delta(&mut self, mut data: Value) -> AppResult<Projection> {
        let delta = required_string(&data, "delta", "reasoning summary delta")?.to_string();
        let slot = self.resolve_event_slot(ItemKind::Reasoning, &data, true)?;
        let summary_index = optional_index(&data, "summary_index")?.unwrap_or(0);
        let mut events = Vec::new();
        self.ensure_reasoning_started(&slot, &mut events)?;
        self.ensure_summary_part(&slot, summary_index, &mut events)?;
        let summary = self
            .reasoning
            .get_mut(&slot.id)
            .expect("reasoning state")
            .summaries
            .get_mut(&summary_index)
            .expect("summary state");
        if summary.text_done || summary.part_done {
            return Err(projection_error("reasoning summary delta followed done"));
        }
        if summary.text.len().saturating_add(delta.len()) > self.limits.max_text_bytes {
            return Err(projection_error(
                "reasoning summary exceeded its memory limit",
            ));
        }
        summary.text.push_str(&delta);
        set_reasoning_identity(&mut data, &slot, summary_index);
        events.push(ProjectedEvent::new(
            "response.reasoning_summary_text.delta",
            data,
        ));
        Ok(Projection {
            events,
            completed_items: Vec::new(),
        })
    }

    fn project_summary_text_done(&mut self, mut data: Value) -> AppResult<Projection> {
        let text = required_string(&data, "text", "reasoning summary done")?.to_string();
        let slot = self.resolve_event_slot(ItemKind::Reasoning, &data, false)?;
        let summary_index = optional_index(&data, "summary_index")?.unwrap_or(0);
        let mut events = Vec::new();
        self.ensure_reasoning_started(&slot, &mut events)?;
        self.ensure_summary_part(&slot, summary_index, &mut events)?;
        let summary = self
            .reasoning
            .get_mut(&slot.id)
            .expect("reasoning state")
            .summaries
            .get_mut(&summary_index)
            .expect("summary state");
        reconcile_text(&summary.text, &text, "reasoning summary text differed")?;
        if summary.text_done {
            return Err(projection_error(
                "reasoning summary text done was emitted twice",
            ));
        }
        summary.text = text;
        summary.text_done = true;
        set_reasoning_identity(&mut data, &slot, summary_index);
        events.push(ProjectedEvent::new(
            "response.reasoning_summary_text.done",
            data,
        ));
        Ok(Projection {
            events,
            completed_items: Vec::new(),
        })
    }

    fn project_summary_part_done(&mut self, mut data: Value) -> AppResult<Projection> {
        let slot = self.resolve_event_slot(ItemKind::Reasoning, &data, false)?;
        let summary_index = optional_index(&data, "summary_index")?.unwrap_or(0);
        let mut events = Vec::new();
        self.ensure_reasoning_started(&slot, &mut events)?;
        self.ensure_summary_part(&slot, summary_index, &mut events)?;
        let text = data
            .pointer("/part/text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let summary = self
            .reasoning
            .get_mut(&slot.id)
            .expect("reasoning state")
            .summaries
            .get_mut(&summary_index)
            .expect("summary state");
        reconcile_text(&summary.text, &text, "reasoning summary part differed")?;
        if summary.part_done {
            return Err(projection_error(
                "reasoning summary part done was emitted twice",
            ));
        }
        summary.text = text;
        summary.part_done = true;
        set_reasoning_identity(&mut data, &slot, summary_index);
        events.push(ProjectedEvent::new(
            "response.reasoning_summary_part.done",
            data,
        ));
        Ok(Projection {
            events,
            completed_items: Vec::new(),
        })
    }

    fn project_item_done(&mut self, data: Value) -> AppResult<Projection> {
        let item = data
            .get("item")
            .cloned()
            .ok_or_else(|| projection_error("output_item.done omitted item"))?;
        match item_kind(&item)? {
            ItemKind::Message => self.complete_message(data, item),
            ItemKind::Reasoning => self.complete_reasoning(data, item),
            ItemKind::Function => self.complete_function(data, item),
        }
    }

    fn complete_message(&mut self, mut data: Value, mut item: Value) -> AppResult<Projection> {
        let item_id = item.get("id").and_then(Value::as_str);
        let output_index = optional_index(&data, "output_index")?;
        let slot = self.resolve_slot(ItemKind::Message, item_id, output_index, true)?;
        set_direct_item_id(&mut item, &slot.id)?;
        let final_item: ResponseItem = serde_json::from_value(item.clone())
            .map_err(|_| projection_error("completed message item was malformed"))?;
        let final_text = message_output_text(&final_item)?;
        let mut events = Vec::new();
        self.ensure_message_started(&slot, &mut events, final_text.is_some())?;
        if let Some(final_text) = final_text {
            let state = self.messages.get_mut(&slot.id).expect("message state");
            reconcile_text(
                &state.text,
                final_text,
                "message item text differed from deltas",
            )?;
            if state.text.is_empty() && !final_text.is_empty() {
                state.text.push_str(final_text);
                events.push(text_delta_event(&slot, 0, final_text));
            }
            if !state.text_done {
                state.text_done = true;
                events.push(text_done_event(&slot, 0, final_text));
            }
            if !state.part_done {
                state.part_done = true;
                events.push(content_part_done_event(&slot, 0, final_text));
            }
        }
        data.as_object_mut()
            .ok_or_else(|| projection_error("output_item.done was not an object"))?
            .insert("item".to_string(), item);
        set_index(&mut data, "output_index", slot.output_index);
        events.push(ProjectedEvent::new("response.output_item.done", data));
        self.complete_slot(&slot)?;
        Ok(Projection {
            events,
            completed_items: vec![CompletedOutputItem {
                output_index: slot.output_index,
                item: final_item,
            }],
        })
    }

    fn complete_reasoning(&mut self, mut data: Value, mut item: Value) -> AppResult<Projection> {
        let item_id = item.get("id").and_then(Value::as_str);
        let output_index = optional_index(&data, "output_index")?;
        let slot = self.resolve_slot(ItemKind::Reasoning, item_id, output_index, true)?;
        set_direct_item_id(&mut item, &slot.id)?;
        let final_item: ResponseItem = serde_json::from_value(item.clone())
            .map_err(|_| projection_error("completed reasoning item was malformed"))?;
        let summaries = reasoning_summaries(&final_item)?;
        let mut events = Vec::new();
        self.ensure_reasoning_started(&slot, &mut events)?;
        for (summary_index, text) in summaries.iter().enumerate() {
            self.ensure_summary_part(&slot, summary_index, &mut events)?;
            let summary = self
                .reasoning
                .get_mut(&slot.id)
                .expect("reasoning state")
                .summaries
                .get_mut(&summary_index)
                .expect("summary state");
            reconcile_text(
                &summary.text,
                text,
                "reasoning item summary differed from deltas",
            )?;
            if summary.text.is_empty() && !text.is_empty() {
                summary.text.push_str(text);
                events.push(summary_delta_event(&slot, summary_index, text));
            }
            if !summary.text_done {
                summary.text_done = true;
                events.push(summary_text_done_event(&slot, summary_index, text));
            }
            if !summary.part_done {
                summary.part_done = true;
                events.push(summary_part_done_event(&slot, summary_index, text));
            }
        }
        data.as_object_mut()
            .ok_or_else(|| projection_error("output_item.done was not an object"))?
            .insert("item".to_string(), item);
        set_index(&mut data, "output_index", slot.output_index);
        events.push(ProjectedEvent::new("response.output_item.done", data));
        self.complete_slot(&slot)?;
        Ok(Projection {
            events,
            completed_items: vec![CompletedOutputItem {
                output_index: slot.output_index,
                item: final_item,
            }],
        })
    }

    fn complete_function(&mut self, mut data: Value, mut item: Value) -> AppResult<Projection> {
        let item_id = item.get("id").and_then(Value::as_str);
        let output_index = optional_index(&data, "output_index")?;
        let slot = self.resolve_slot(ItemKind::Function, item_id, output_index, true)?;
        set_direct_item_id(&mut item, &slot.id)?;
        let final_item: ResponseItem = serde_json::from_value(item.clone())
            .map_err(|_| projection_error("completed function item was malformed"))?;
        let ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        } = &final_item
        else {
            return Err(projection_error(
                "function item changed type during projection",
            ));
        };
        let state = self.functions.entry(slot.id.clone()).or_default();
        if let Some(known) = &state.name
            && known != name
        {
            return Err(projection_error(
                "function name changed across private events",
            ));
        }
        if let Some(known) = &state.call_id
            && known != call_id
        {
            return Err(projection_error(
                "function call_id changed across private events",
            ));
        }
        let accumulated = state.deltas.concat();
        if !accumulated.is_empty() {
            reconcile_json(
                &accumulated,
                arguments,
                "function argument deltas differed from item",
            )?;
        }
        if let Some(done) = &state.done_arguments {
            reconcile_json(done, arguments, "function argument done differed from item")?;
        }

        let mut events = Vec::new();
        let mut added_item = item.clone();
        added_item
            .as_object_mut()
            .expect("function item object")
            .insert("arguments".to_string(), Value::String(String::new()));
        events.push(ProjectedEvent::new(
            "response.output_item.added",
            json!({
                "type":"response.output_item.added",
                "output_index":slot.output_index,
                "item":added_item,
            }),
        ));
        if state.deltas.is_empty() {
            events.push(function_delta_event(&slot, call_id, name, arguments));
        } else {
            for delta in &state.deltas {
                events.push(function_delta_event(&slot, call_id, name, delta));
            }
        }
        events.push(function_done_event(&slot, call_id, name, arguments));
        data.as_object_mut()
            .ok_or_else(|| projection_error("output_item.done was not an object"))?
            .insert("item".to_string(), item);
        set_index(&mut data, "output_index", slot.output_index);
        events.push(ProjectedEvent::new("response.output_item.done", data));

        let state = self.functions.remove(&slot.id).expect("function state");
        self.total_function_bytes = self
            .total_function_bytes
            .saturating_sub(state.retained_bytes);
        self.complete_slot(&slot)?;
        Ok(Projection {
            events,
            completed_items: vec![CompletedOutputItem {
                output_index: slot.output_index,
                item: final_item,
            }],
        })
    }

    fn ensure_message_started(
        &mut self,
        slot: &Slot,
        events: &mut Vec<ProjectedEvent>,
        ensure_part: bool,
    ) -> AppResult<()> {
        let state = self.messages.entry(slot.id.clone()).or_default();
        if !state.added {
            state.added = true;
            events.push(ProjectedEvent::new(
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "output_index":slot.output_index,
                    "item":{"type":"message","id":slot.id,"role":"assistant","content":[]},
                }),
            ));
        }
        if ensure_part && !state.part_added {
            state.part_added = true;
            events.push(content_part_added_event(slot, 0));
        }
        Ok(())
    }

    fn ensure_reasoning_started(
        &mut self,
        slot: &Slot,
        events: &mut Vec<ProjectedEvent>,
    ) -> AppResult<()> {
        let state = self.reasoning.entry(slot.id.clone()).or_default();
        if !state.added {
            state.added = true;
            events.push(ProjectedEvent::new(
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "output_index":slot.output_index,
                    "item":{"type":"reasoning","id":slot.id,"summary":[]},
                }),
            ));
        }
        Ok(())
    }

    fn ensure_summary_part(
        &mut self,
        slot: &Slot,
        summary_index: usize,
        events: &mut Vec<ProjectedEvent>,
    ) -> AppResult<()> {
        let state = self.reasoning.entry(slot.id.clone()).or_default();
        let summary = state.summaries.entry(summary_index).or_default();
        if !summary.part_added {
            summary.part_added = true;
            events.push(summary_part_added_event(slot, summary_index));
        }
        Ok(())
    }

    fn resolve_event_slot(
        &mut self,
        kind: ItemKind,
        data: &Value,
        allow_new_without_identity: bool,
    ) -> AppResult<Slot> {
        let item_id = data.get("item_id").and_then(Value::as_str);
        let output_index = optional_index(data, "output_index")?;
        self.resolve_slot(kind, item_id, output_index, allow_new_without_identity)
    }

    fn resolve_slot(
        &mut self,
        kind: ItemKind,
        item_id: Option<&str>,
        output_index: Option<usize>,
        allow_new_without_identity: bool,
    ) -> AppResult<Slot> {
        let by_id = item_id.and_then(|id| self.slots.get(id)).cloned();
        let by_index = output_index
            .and_then(|index| self.item_by_output_index.get(&index))
            .and_then(|id| self.slots.get(id))
            .cloned();
        if let (Some(by_id), Some(by_index)) = (&by_id, &by_index)
            && by_id.id != by_index.id
        {
            return Err(projection_error(
                "private Responses item id and output index referred to different items",
            ));
        }
        if let Some(slot) = by_id.clone().or(by_index.clone()) {
            if slot.kind != kind {
                return Err(projection_error(
                    "private Responses item changed type across events",
                ));
            }
            if let Some(id) = item_id
                && id != slot.id
            {
                return Err(projection_error("private Responses item id changed"));
            }
            if let Some(index) = output_index
                && index != slot.output_index
            {
                return Err(projection_error("private Responses output index changed"));
            }
            return Ok(slot);
        }

        if output_index.is_none()
            && let Some(late_id) = item_id
            && by_id.is_none()
        {
            let candidates = self
                .slots
                .values()
                .filter(|slot| {
                    slot.kind == kind
                        && slot.synthetic_id
                        && !self.completed_item_ids.contains(&slot.id)
                })
                .cloned()
                .collect::<Vec<_>>();
            match candidates.as_slice() {
                [slot] => return Ok(slot.clone()),
                [] => {}
                _ => {
                    return Err(projection_error(format!(
                        "late private Responses item id {late_id} was ambiguous"
                    )));
                }
            }
        }

        if item_id.is_none() && output_index.is_none() {
            let candidates = self
                .slots
                .values()
                .filter(|slot| slot.kind == kind && !self.completed_item_ids.contains(&slot.id))
                .cloned()
                .collect::<Vec<_>>();
            match candidates.as_slice() {
                [slot] => return Ok(slot.clone()),
                [] if allow_new_without_identity => {}
                [] => {
                    return Err(projection_error(
                        "private Responses event could not be associated with an output item",
                    ));
                }
                _ => {
                    return Err(projection_error(
                        "private Responses event was ambiguous between open output items",
                    ));
                }
            }
        }

        if self.slots.len() >= self.limits.max_open_items {
            return Err(projection_error(
                "private Responses stream exceeded its output item limit",
            ));
        }
        let synthetic_id = item_id.is_none();
        let id = item_id
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}_{}", kind.prefix(), Uuid::new_v4().simple()));
        if self.completed_item_ids.contains(&id) {
            return Err(projection_error("private Responses item id was reused"));
        }
        let index = match output_index {
            Some(index) => index,
            None => self.next_free_output_index(),
        };
        if self.item_by_output_index.contains_key(&index) {
            return Err(projection_error(
                "private Responses output index was reused",
            ));
        }
        self.next_output_index = self.next_output_index.max(index.saturating_add(1));
        let slot = Slot {
            id: id.clone(),
            output_index: index,
            kind,
            synthetic_id,
        };
        self.item_by_output_index.insert(index, id.clone());
        self.slots.insert(id.clone(), slot.clone());
        match kind {
            ItemKind::Message => {
                self.messages.entry(id).or_default();
            }
            ItemKind::Reasoning => {
                self.reasoning.entry(id).or_default();
            }
            ItemKind::Function => {
                self.functions.entry(id).or_default();
            }
        }
        Ok(slot)
    }

    fn next_free_output_index(&mut self) -> usize {
        while self
            .item_by_output_index
            .contains_key(&self.next_output_index)
        {
            self.next_output_index = self.next_output_index.saturating_add(1);
        }
        self.next_output_index
    }

    fn complete_slot(&mut self, slot: &Slot) -> AppResult<()> {
        if !self.completed_item_ids.insert(slot.id.clone()) {
            return Err(projection_error("private Responses item completed twice"));
        }
        self.messages.remove(&slot.id);
        self.reasoning.remove(&slot.id);
        self.slots.remove(&slot.id);
        self.item_by_output_index.remove(&slot.output_index);
        Ok(())
    }

    fn charge_function_event(&mut self, item_id: &str, bytes: usize) -> AppResult<()> {
        let state = self.functions.get_mut(item_id).ok_or_else(|| {
            projection_error("function event was not associated with an open function")
        })?;
        if state.event_count >= self.limits.max_function_events {
            return Err(projection_error(
                "private function call exceeded its event count limit",
            ));
        }
        if state.retained_bytes.saturating_add(bytes) > self.limits.max_function_argument_bytes {
            return Err(projection_error(
                "private function arguments exceeded their memory limit",
            ));
        }
        if self.total_function_bytes.saturating_add(bytes) > self.limits.max_total_function_bytes {
            return Err(projection_error(
                "private function arguments exceeded the stream memory limit",
            ));
        }
        state.event_count += 1;
        state.retained_bytes += bytes;
        self.total_function_bytes += bytes;
        Ok(())
    }
}

fn projection_error(message: impl Into<String>) -> AppError {
    AppError::upstream(message).with_code("malformed_upstream_response")
}

fn item_kind(item: &Value) -> AppResult<ItemKind> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => Ok(ItemKind::Message),
        Some("reasoning") => Ok(ItemKind::Reasoning),
        Some("function_call") => Ok(ItemKind::Function),
        Some(other) => Err(projection_error(format!(
            "unsupported private Responses output item type {other}"
        ))),
        None => Err(projection_error(
            "private Responses output item omitted type",
        )),
    }
}

fn optional_index(data: &Value, key: &str) -> AppResult<Option<usize>> {
    match data.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .map(Some)
            .ok_or_else(|| projection_error(format!("{key} was not a non-negative integer"))),
    }
}

fn required_string<'a>(data: &'a Value, key: &str, context: &str) -> AppResult<&'a str> {
    data.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| projection_error(format!("{context} omitted string {key}")))
}

fn merge_known_string(
    destination: &mut Option<String>,
    source: Option<&Value>,
    context: &str,
) -> AppResult<()> {
    let Some(source) = source else { return Ok(()) };
    let source = source
        .as_str()
        .ok_or_else(|| projection_error(format!("{context} was not a string")))?;
    if destination.as_deref().is_some_and(|known| known != source) {
        return Err(projection_error(format!("{context} changed across events")));
    }
    *destination = Some(source.to_string());
    Ok(())
}

fn set_item_id(data: &mut Value, item_id: &str) -> AppResult<()> {
    let item = data
        .get_mut("item")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| projection_error("event item was not an object"))?;
    item.insert("id".to_string(), Value::String(item_id.to_string()));
    Ok(())
}

fn set_direct_item_id(item: &mut Value, item_id: &str) -> AppResult<()> {
    item.as_object_mut()
        .ok_or_else(|| projection_error("output item was not an object"))?
        .insert("id".to_string(), Value::String(item_id.to_string()));
    Ok(())
}

fn set_index(data: &mut Value, key: &str, index: usize) {
    if let Some(object) = data.as_object_mut() {
        object.insert(key.to_string(), Value::from(index as u64));
    }
}

fn set_identity(data: &mut Value, slot: &Slot, content_index: Option<usize>) {
    if let Some(object) = data.as_object_mut() {
        object.insert("item_id".to_string(), Value::String(slot.id.clone()));
        object.insert(
            "output_index".to_string(),
            Value::from(slot.output_index as u64),
        );
        if let Some(index) = content_index {
            object.insert("content_index".to_string(), Value::from(index as u64));
        }
    }
}

fn set_reasoning_identity(data: &mut Value, slot: &Slot, summary_index: usize) {
    if let Some(object) = data.as_object_mut() {
        object.insert("item_id".to_string(), Value::String(slot.id.clone()));
        object.insert(
            "output_index".to_string(),
            Value::from(slot.output_index as u64),
        );
        object.insert(
            "summary_index".to_string(),
            Value::from(summary_index as u64),
        );
    }
}

fn single_event(event: &'static str, data: Value) -> Projection {
    Projection {
        events: vec![ProjectedEvent::new(event, data)],
        completed_items: Vec::new(),
    }
}

fn reconcile_text(observed: &str, final_text: &str, context: &str) -> AppResult<()> {
    if !observed.is_empty() && observed != final_text {
        Err(projection_error(context))
    } else {
        Ok(())
    }
}

fn reconcile_json(observed: &str, final_text: &str, context: &str) -> AppResult<()> {
    let observed: Value = serde_json::from_str(observed)
        .map_err(|_| projection_error("private function arguments were malformed JSON"))?;
    let final_value: Value = serde_json::from_str(final_text)
        .map_err(|_| projection_error("completed function arguments were malformed JSON"))?;
    if observed == final_value {
        Ok(())
    } else {
        Err(projection_error(context))
    }
}

fn message_output_text(item: &ResponseItem) -> AppResult<Option<&str>> {
    let ResponseItem::Message { content, .. } = item else {
        return Err(projection_error("completed message changed type"));
    };
    let mut text = None;
    for part in content {
        if let ContentItem::OutputText { text: part_text } = part
            && text.replace(part_text.as_str()).is_some()
        {
            return Err(projection_error(
                "private message contained multiple output_text parts",
            ));
        }
    }
    Ok(text)
}

fn reasoning_summaries(item: &ResponseItem) -> AppResult<Vec<&str>> {
    let ResponseItem::Reasoning { summary, .. } = item else {
        return Err(projection_error("completed reasoning item changed type"));
    };
    Ok(summary
        .iter()
        .map(|part| match part {
            ReasoningSummaryItem::SummaryText { text } => text.as_str(),
        })
        .collect())
}

fn content_part_added_event(slot: &Slot, content_index: usize) -> ProjectedEvent {
    ProjectedEvent::new(
        "response.content_part.added",
        json!({
            "type":"response.content_part.added",
            "item_id":slot.id,
            "output_index":slot.output_index,
            "content_index":content_index,
            "part":{"type":"output_text","text":"","annotations":[]},
        }),
    )
}

fn text_delta_event(slot: &Slot, content_index: usize, delta: &str) -> ProjectedEvent {
    ProjectedEvent::new(
        "response.output_text.delta",
        json!({
            "type":"response.output_text.delta",
            "item_id":slot.id,
            "output_index":slot.output_index,
            "content_index":content_index,
            "delta":delta,
        }),
    )
}

fn text_done_event(slot: &Slot, content_index: usize, text: &str) -> ProjectedEvent {
    ProjectedEvent::new(
        "response.output_text.done",
        json!({
            "type":"response.output_text.done",
            "item_id":slot.id,
            "output_index":slot.output_index,
            "content_index":content_index,
            "text":text,
        }),
    )
}

fn content_part_done_event(slot: &Slot, content_index: usize, text: &str) -> ProjectedEvent {
    ProjectedEvent::new(
        "response.content_part.done",
        json!({
            "type":"response.content_part.done",
            "item_id":slot.id,
            "output_index":slot.output_index,
            "content_index":content_index,
            "part":{"type":"output_text","text":text,"annotations":[]},
        }),
    )
}

fn summary_part_added_event(slot: &Slot, summary_index: usize) -> ProjectedEvent {
    ProjectedEvent::new(
        "response.reasoning_summary_part.added",
        json!({
            "type":"response.reasoning_summary_part.added",
            "item_id":slot.id,
            "output_index":slot.output_index,
            "summary_index":summary_index,
            "part":{"type":"summary_text","text":""},
        }),
    )
}

fn summary_delta_event(slot: &Slot, summary_index: usize, delta: &str) -> ProjectedEvent {
    ProjectedEvent::new(
        "response.reasoning_summary_text.delta",
        json!({
            "type":"response.reasoning_summary_text.delta",
            "item_id":slot.id,
            "output_index":slot.output_index,
            "summary_index":summary_index,
            "delta":delta,
        }),
    )
}

fn summary_text_done_event(slot: &Slot, summary_index: usize, text: &str) -> ProjectedEvent {
    ProjectedEvent::new(
        "response.reasoning_summary_text.done",
        json!({
            "type":"response.reasoning_summary_text.done",
            "item_id":slot.id,
            "output_index":slot.output_index,
            "summary_index":summary_index,
            "text":text,
        }),
    )
}

fn summary_part_done_event(slot: &Slot, summary_index: usize, text: &str) -> ProjectedEvent {
    ProjectedEvent::new(
        "response.reasoning_summary_part.done",
        json!({
            "type":"response.reasoning_summary_part.done",
            "item_id":slot.id,
            "output_index":slot.output_index,
            "summary_index":summary_index,
            "part":{"type":"summary_text","text":text},
        }),
    )
}

fn function_delta_event(slot: &Slot, call_id: &str, name: &str, delta: &str) -> ProjectedEvent {
    ProjectedEvent::new(
        "response.function_call_arguments.delta",
        json!({
            "type":"response.function_call_arguments.delta",
            "item_id":slot.id,
            "output_index":slot.output_index,
            "call_id":call_id,
            "name":name,
            "delta":delta,
        }),
    )
}

fn function_done_event(slot: &Slot, call_id: &str, name: &str, arguments: &str) -> ProjectedEvent {
    ProjectedEvent::new(
        "response.function_call_arguments.done",
        json!({
            "type":"response.function_call_arguments.done",
            "item_id":slot.id,
            "output_index":slot.output_index,
            "call_id":call_id,
            "name":name,
            "arguments":arguments,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(projection: &Projection) -> Vec<&str> {
        projection
            .events
            .iter()
            .map(|event| event.event.as_str())
            .collect()
    }

    #[test]
    fn pinned_codex_minimal_text_fixture_gets_full_stable_lifecycle() {
        let mut projector = CodexPrivateResponsesProjector::default();
        // Copied from codex-rs/core/tests/common/responses.rs::ev_output_text_delta.
        let delta = projector
            .project(
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","delta":"hello"}),
            )
            .unwrap();
        assert_eq!(
            kinds(&delta),
            [
                "response.output_item.added",
                "response.content_part.added",
                "response.output_text.delta"
            ]
        );
        let item_id = delta.events[2].data["item_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(delta.events[2].data["output_index"], 0);
        assert_eq!(delta.events[2].data["content_index"], 0);

        // Copied from codex-rs/core/tests/common/responses.rs::ev_assistant_message;
        // notably there is no output_index.
        let done = projector
            .project(
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "item":{
                        "type":"message",
                        "role":"assistant",
                        "id":"provider-message-id",
                        "content":[{"type":"output_text","text":"hello"}]
                    }
                }),
            )
            .unwrap();
        assert_eq!(
            kinds(&done),
            [
                "response.output_text.done",
                "response.content_part.done",
                "response.output_item.done"
            ]
        );
        assert_eq!(done.events[2].data["output_index"], 0);
        assert_eq!(done.events[2].data["item"]["id"], item_id);
        assert_eq!(done.completed_items[0].output_index, 0);
        projector.finish().unwrap();
    }

    #[test]
    fn pinned_codex_minimal_function_fixture_associates_by_item_id_then_finishes() {
        let mut projector = CodexPrivateResponsesProjector::default();
        // Copied from codex-rs/codex-api/src/sse/responses.rs::
        // parses_tool_call_input_deltas: no call_id or output_index.
        assert!(
            projector
                .project(
                    "response.function_call_arguments.delta",
                    json!({
                        "type":"response.function_call_arguments.delta",
                        "item_id":"fc_1",
                        "delta":"{\"input\":\"value\"}"
                    }),
                )
                .unwrap()
                .events
                .is_empty()
        );

        // Copied from codex-rs/core/tests/common/responses.rs::ev_function_call;
        // notably the item has no id and the event has no output_index.
        let done = projector
            .project(
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "item":{
                        "type":"function_call",
                        "call_id":"call_1",
                        "name":"shell_command",
                        "arguments":"{\"input\":\"value\"}"
                    }
                }),
            )
            .unwrap();
        assert_eq!(
            kinds(&done),
            [
                "response.output_item.added",
                "response.function_call_arguments.delta",
                "response.function_call_arguments.done",
                "response.output_item.done"
            ]
        );
        for event in &done.events {
            assert_eq!(event.data["output_index"], 0);
        }
        assert_eq!(done.events[0].data["item"]["id"], "fc_1");
        assert_eq!(done.events[1].data["call_id"], "call_1");
        assert_eq!(done.completed_items[0].output_index, 0);
        projector.finish().unwrap();
    }

    #[test]
    fn pinned_codex_reasoning_summary_fixture_gets_ids_and_indexes() {
        let mut projector = CodexPrivateResponsesProjector::default();
        // Copied from codex-rs/core/tests/common/responses.rs::
        // ev_reasoning_item_added; notably no output_index.
        let added = projector
            .project(
                "response.output_item.added",
                json!({
                    "type":"response.output_item.added",
                    "item":{"type":"reasoning","id":"reason-1","summary":[]}
                }),
            )
            .unwrap();
        assert_eq!(added.events[0].data["output_index"], 0);

        // Copied from ev_reasoning_summary_text_delta; no item/output index.
        let delta = projector
            .project(
                "response.reasoning_summary_text.delta",
                json!({
                    "type":"response.reasoning_summary_text.delta",
                    "delta":"safe summary",
                    "summary_index":0
                }),
            )
            .unwrap();
        assert_eq!(
            kinds(&delta),
            [
                "response.reasoning_summary_part.added",
                "response.reasoning_summary_text.delta"
            ]
        );
        assert_eq!(delta.events[1].data["item_id"], "reason-1");
        assert_eq!(delta.events[1].data["output_index"], 0);

        let done = projector
            .project(
                "response.output_item.done",
                json!({
                    "type":"response.output_item.done",
                    "item":{
                        "type":"reasoning",
                        "id":"reason-1",
                        "summary":[{"type":"summary_text","text":"safe summary"}]
                    }
                }),
            )
            .unwrap();
        assert_eq!(
            kinds(&done),
            [
                "response.reasoning_summary_text.done",
                "response.reasoning_summary_part.done",
                "response.output_item.done"
            ]
        );
        assert_eq!(done.completed_items[0].output_index, 0);
    }

    #[test]
    fn identity_free_fragment_fails_when_parallel_calls_are_ambiguous() {
        let mut projector = CodexPrivateResponsesProjector::default();
        for id in ["fc_a", "fc_b"] {
            projector
                .project(
                    "response.output_item.added",
                    json!({
                        "type":"response.output_item.added",
                        "item":{"type":"function_call","id":id,"arguments":""}
                    }),
                )
                .unwrap();
        }
        let error = projector
            .project(
                "response.function_call_arguments.delta",
                json!({
                    "type":"response.function_call_arguments.delta",
                    "delta":"{}"
                }),
            )
            .unwrap_err();
        assert_eq!(error.code.as_deref(), Some("malformed_upstream_response"));
        assert!(error.message.contains("ambiguous"));
    }

    #[test]
    fn buffered_function_arguments_and_event_count_are_bounded() {
        let mut projector = CodexPrivateResponsesProjector::with_limits(ProjectionLimits {
            max_function_argument_bytes: 4,
            max_function_events: 2,
            max_total_function_bytes: 4,
            ..ProjectionLimits::default()
        });
        projector
            .project(
                "response.function_call_arguments.delta",
                json!({
                    "type":"response.function_call_arguments.delta",
                    "item_id":"fc_1",
                    "delta":"1234"
                }),
            )
            .unwrap();
        let error = projector
            .project(
                "response.function_call_arguments.delta",
                json!({
                    "type":"response.function_call_arguments.delta",
                    "item_id":"fc_1",
                    "delta":"5"
                }),
            )
            .unwrap_err();
        assert!(error.message.contains("memory limit"));
    }

    #[test]
    fn output_indexes_follow_first_appearance_across_item_kinds() {
        let mut projector = CodexPrivateResponsesProjector::default();
        let text = projector
            .project(
                "response.output_text.delta",
                json!({"type":"response.output_text.delta","delta":"x"}),
            )
            .unwrap();
        let reasoning = projector
            .project(
                "response.output_item.added",
                json!({"type":"response.output_item.added","item":{"type":"reasoning","id":"r","summary":[]}}),
            )
            .unwrap();
        let function = projector
            .project(
                "response.function_call_arguments.delta",
                json!({"type":"response.function_call_arguments.delta","item_id":"f","delta":"{}"}),
            )
            .unwrap();
        assert_eq!(text.events[0].data["output_index"], 0);
        assert_eq!(reasoning.events[0].data["output_index"], 1);
        assert!(function.events.is_empty());
        let completed = projector
            .project(
                "response.output_item.done",
                json!({"type":"response.output_item.done","item":{"type":"function_call","call_id":"c","name":"tool","arguments":"{}"}}),
            )
            .unwrap();
        assert_eq!(completed.completed_items[0].output_index, 2);
    }
}
