//! test-support: stream conformance assertions.
//!
//! Reusable invariant checks for the Anthropic `/v1/messages` **streaming**
//! wire shape, shared by all
//! three surfaces that need to assert it:
//!   - `src/adapters/responses_to_anthropic/tests.rs` -- crate-internal unit
//!     tests, operate on the converter's own [`AnthropicStreamEvent`] output
//!     via [`assert_stream_conformant`] (reached as `super::conformance` /
//!     `crate::adapters::responses_to_anthropic::conformance`).
//!   - `tests/gateway.rs` -- integration crate, operates on parsed JSON SSE
//!     (the `data:` payload of each frame, e.g. via the existing
//!     `parse_anthropic_sse_events` helper) via [`assert_sse_conformant`].
//!   - `tests/port_streaming_peek.rs` -- integration crate, constructs
//!     [`crate::adapters::responses_to_anthropic::AnthropicStreamConverter`]
//!     directly and operates on its `AnthropicStreamEvent` output via
//!     [`assert_stream_conformant`].
//!
//! Both `tests/*.rs` files are separate crates that depend on `llmconduit` as
//! an ordinary library, so this module is `pub` and always compiled (NOT
//! `#[cfg(test)]`) -- a `#[cfg(test)]` item only exists in `llmconduit`'s own
//! test build, which the external integration-test crates never link against.
//!
//! Both public entry points normalize their input down to the same internal
//! `ShapeKind` sequence and run ONE shared invariant walk (`check_shapes`), so
//! the two wire forms (live converter output vs. parsed-JSON-over-the-wire)
//! can never silently drift apart.
//!
//! # Invariants (from the spec; all six are asserted on every surface except
//! where `Surface` says otherwise)
//! 1. Exactly ONE `message_delta`, and it carries a non-null `stop_reason`.
//! 2. NO `message_delta` before the first `content_block_start`.
//! 3. NO `message_delta` between a `content_block_delta` and its matching
//!    `content_block_stop` (never while ANY block is open).
//! 4. A `thinking` block emits a NON-EMPTY `signature_delta` before it closes.
//! 5. The last two events are `message_delta` then `message_stop`.
//! 6. `Surface::Error` instead ends with an `error` event (replaces 1 + 5).
//!
//! `message_start.input_tokens` is intentionally NOT checked here.

use crate::models::anthropic::AnthropicContentBlockStart;
use crate::models::anthropic::AnthropicDelta;
use crate::models::anthropic::AnthropicStreamEvent;
use serde_json::Value;
use std::collections::BTreeMap;

/// Which stream surface is being asserted.
///
/// Every surface shares invariants 2-4 (block-shape rules don't depend on
/// WHAT the blocks are); `Surface` only changes the TERMINAL-shape check:
/// every surface but [`Surface::Error`] must end `message_delta` then
/// `message_stop` (invariants 1 + 5), while `Error` must end with an `error`
/// event instead (a failed turn never gets a clean terminal `message_delta`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// Plain text answer: no thinking block, no tool call.
    TextOnly,
    /// Reasoning (`thinking` block) followed by a text answer.
    ReasoningText,
    /// A CLIENT-visible `tool_use` block. Deliberately NOT `web_search`:
    /// `web_search` never calls `record_output_delta` (it emits its blocks
    /// directly), so it can't exercise the no-progressive-delta invariants
    /// the way a real client tool call does -- see `mod.rs:534` / AGENTS.md.
    ClientToolUse,
    /// Server-side web search: `server_tool_use` + `web_search_tool_result`.
    WebSearch,
    /// A turn that ends in `response.failed` -- terminates with `error`
    /// instead of `message_delta` + `message_stop` (invariant 6).
    Error,
}

/// Assert `events` (the converter's own output enum) is wire-conformant for
/// `surface`. Panics naming the failing invariant and the event index.
pub fn assert_stream_conformant(events: &[AnthropicStreamEvent], surface: Surface) {
    if let Err(failure) = check_stream_conformant(events, surface) {
        panic!("{failure}");
    }
}

/// `Result`-returning form of [`assert_stream_conformant`], for callers (this
/// module's own self-tests, chiefly) that want to assert REJECTION without
/// relying on `#[should_panic]` message matching.
pub fn check_stream_conformant(
    events: &[AnthropicStreamEvent],
    surface: Surface,
) -> Result<(), String> {
    let shapes: Vec<ShapeKind> = events.iter().map(ShapeKind::from_event).collect();
    check_shapes(&shapes, surface)
}

/// Assert `events` (parsed Anthropic SSE `data:` JSON payloads -- e.g. via the
/// integration crates' `parse_anthropic_sse_events`) is wire-conformant for
/// `surface`. Panics naming the failing invariant and the event index.
pub fn assert_sse_conformant(events: &[Value], surface: Surface) {
    if let Err(failure) = check_sse_conformant(events, surface) {
        panic!("{failure}");
    }
}

/// `Result`-returning form of [`assert_sse_conformant`].
pub fn check_sse_conformant(events: &[Value], surface: Surface) -> Result<(), String> {
    let shapes: Vec<ShapeKind> = events.iter().map(ShapeKind::from_json).collect();
    check_shapes(&shapes, surface)
}

/// Normalized view of a single stream event -- just enough structure to check
/// every invariant -- independent of which of the two wire forms produced it.
/// Both public entry points reduce to a `Vec<ShapeKind>` so the invariant walk
/// in `check_shapes` is written exactly once.
#[derive(Debug)]
enum ShapeKind {
    MessageStart,
    ContentBlockStart {
        index: usize,
        block_type: String,
    },
    ContentBlockDelta {
        index: usize,
        delta_type: String,
        signature_non_empty: bool,
    },
    ContentBlockStop {
        index: usize,
    },
    MessageDelta {
        stop_reason_non_null: bool,
    },
    MessageStop,
    Ping,
    Error,
    /// Anything outside the Anthropic streaming vocabulary above. Carries no
    /// invariant weight (skipped during the walk) -- forward-compatible with
    /// an SSE event type the harness doesn't (yet) know about, rather than
    /// panicking on it.
    Other(String),
}

impl ShapeKind {
    fn label(&self) -> &str {
        match self {
            Self::MessageStart => "message_start",
            Self::ContentBlockStart { .. } => "content_block_start",
            Self::ContentBlockDelta { .. } => "content_block_delta",
            Self::ContentBlockStop { .. } => "content_block_stop",
            Self::MessageDelta { .. } => "message_delta",
            Self::MessageStop => "message_stop",
            Self::Ping => "ping",
            Self::Error => "error",
            Self::Other(label) => label,
        }
    }

    fn from_event(event: &AnthropicStreamEvent) -> Self {
        match event {
            AnthropicStreamEvent::MessageStart { .. } => Self::MessageStart,
            AnthropicStreamEvent::ContentBlockStart {
                index,
                content_block,
            } => Self::ContentBlockStart {
                index: *index,
                block_type: content_block_type_name(content_block).to_string(),
            },
            AnthropicStreamEvent::ContentBlockDelta { index, delta } => {
                let (delta_type, signature_non_empty) = match delta {
                    AnthropicDelta::TextDelta { .. } => ("text_delta", false),
                    AnthropicDelta::InputJsonDelta { .. } => ("input_json_delta", false),
                    AnthropicDelta::ThinkingDelta { .. } => ("thinking_delta", false),
                    AnthropicDelta::SignatureDelta { signature } => {
                        ("signature_delta", !signature.is_empty())
                    }
                };
                Self::ContentBlockDelta {
                    index: *index,
                    delta_type: delta_type.to_string(),
                    signature_non_empty,
                }
            }
            AnthropicStreamEvent::ContentBlockStop { index } => {
                Self::ContentBlockStop { index: *index }
            }
            AnthropicStreamEvent::MessageDelta { delta, .. } => Self::MessageDelta {
                stop_reason_non_null: delta.stop_reason.is_some(),
            },
            AnthropicStreamEvent::MessageStop => Self::MessageStop,
            AnthropicStreamEvent::Ping => Self::Ping,
            AnthropicStreamEvent::Error { .. } => Self::Error,
        }
    }

    /// Parses the `{"type": "...", ...}` shape produced by serializing
    /// [`AnthropicStreamEvent`] -- identically what a real Anthropic-conformant
    /// SSE source (vLLM native, or this gateway) puts on the wire. Lenient by
    /// design: a missing/unrecognized field degrades to a harmless default
    /// (`Other`, an absent index, an empty type) rather than panicking --
    /// malformed JSON is a DIFFERENT bug for a DIFFERENT test to catch; this
    /// harness only asserts event ORDERING/SHAPE invariants.
    fn from_json(value: &Value) -> Self {
        let event_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match event_type {
            "message_start" => Self::MessageStart,
            "content_block_start" => Self::ContentBlockStart {
                index: json_index(value),
                block_type: value
                    .get("content_block")
                    .and_then(|block| block.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
            },
            "content_block_delta" => {
                let delta = value.get("delta");
                let delta_type = delta
                    .and_then(|delta| delta.get("type"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let signature_non_empty = delta
                    .and_then(|delta| delta.get("signature"))
                    .and_then(Value::as_str)
                    .is_some_and(|signature| !signature.is_empty());
                Self::ContentBlockDelta {
                    index: json_index(value),
                    delta_type,
                    signature_non_empty,
                }
            }
            "content_block_stop" => Self::ContentBlockStop {
                index: json_index(value),
            },
            "message_delta" => Self::MessageDelta {
                stop_reason_non_null: value
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .is_some_and(|reason| !reason.is_null()),
            },
            "message_stop" => Self::MessageStop,
            "ping" => Self::Ping,
            "error" => Self::Error,
            other => Self::Other(other.to_string()),
        }
    }
}

fn json_index(value: &Value) -> usize {
    value
        .get("index")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX) as usize
}

fn content_block_type_name(block: &AnthropicContentBlockStart) -> &'static str {
    match block {
        AnthropicContentBlockStart::Text { .. } => "text",
        AnthropicContentBlockStart::ToolUse { .. } => "tool_use",
        AnthropicContentBlockStart::Thinking { .. } => "thinking",
        AnthropicContentBlockStart::ServerToolUse { .. } => "server_tool_use",
        AnthropicContentBlockStart::WebSearchToolResult { .. } => "web_search_tool_result",
    }
}

/// The shared invariant walk. Both public entry points reduce to this after
/// normalizing their input to `ShapeKind`. Returns the FIRST violated
/// invariant in event order, so callers get one focused failure instead of a
/// wall of (possibly redundant) downstream noise.
fn check_shapes(events: &[ShapeKind], surface: Surface) -> Result<(), String> {
    if events.is_empty() {
        return Err(format!(
            "conformance[{surface:?}]: empty event stream -- expected at least \
             message_start..message_stop"
        ));
    }

    let mut first_content_block_start_seen = false;
    let mut message_delta_count = 0usize;
    let mut message_delta_with_stop_reason = false;
    // index -> (is_thinking, saw_non_empty_signature_delta). Tracks every
    // OPEN block (started, not yet stopped) so invariant 3 ("never inside an
    // open block") and invariant 4 ("a thinking block must be signed before
    // it closes") hold regardless of how many blocks are open at once.
    let mut open_blocks: BTreeMap<usize, (bool, bool)> = BTreeMap::new();

    for (i, shape) in events.iter().enumerate() {
        match shape {
            ShapeKind::ContentBlockStart { index, block_type } => {
                first_content_block_start_seen = true;
                open_blocks.insert(*index, (block_type.as_str() == "thinking", false));
            }
            ShapeKind::ContentBlockDelta {
                index,
                delta_type,
                signature_non_empty,
            } => {
                if delta_type.as_str() == "signature_delta"
                    && *signature_non_empty
                    && let Some(entry) = open_blocks.get_mut(index)
                {
                    entry.1 = true;
                }
            }
            ShapeKind::ContentBlockStop { index } => {
                if let Some((is_thinking, signed)) = open_blocks.remove(index)
                    && is_thinking
                    && !signed
                {
                    return Err(format!(
                        "conformance[{surface:?}] invariant 4 violated at event #{i} \
                         (content_block_stop, index {index}): thinking block closed without a \
                         non-empty signature_delta"
                    ));
                }
            }
            ShapeKind::MessageDelta {
                stop_reason_non_null,
            } => {
                message_delta_count += 1;
                if !first_content_block_start_seen {
                    return Err(format!(
                        "conformance[{surface:?}] invariant 2 violated at event #{i} \
                         (message_delta): appeared before the first content_block_start"
                    ));
                }
                if !open_blocks.is_empty() {
                    let open: Vec<usize> = open_blocks.keys().copied().collect();
                    return Err(format!(
                        "conformance[{surface:?}] invariant 3 violated at event #{i} \
                         (message_delta): appeared while block index(es) {open:?} were still \
                         open (no matching content_block_stop yet)"
                    ));
                }
                if *stop_reason_non_null {
                    message_delta_with_stop_reason = true;
                }
            }
            ShapeKind::MessageStart
            | ShapeKind::MessageStop
            | ShapeKind::Ping
            | ShapeKind::Error
            | ShapeKind::Other(_) => {}
        }
    }

    match surface {
        Surface::Error => {
            if !matches!(events.last(), Some(ShapeKind::Error)) {
                return Err(format!(
                    "conformance[{surface:?}] invariant 6 violated: stream must end with an \
                     `error` event, got {:?} as the last of {} event(s)",
                    events.last().map(ShapeKind::label),
                    events.len()
                ));
            }
        }
        Surface::TextOnly
        | Surface::ReasoningText
        | Surface::ClientToolUse
        | Surface::WebSearch => {
            if message_delta_count != 1 {
                return Err(format!(
                    "conformance[{surface:?}] invariant 1 violated: expected exactly ONE \
                     message_delta, found {message_delta_count}"
                ));
            }
            if !message_delta_with_stop_reason {
                return Err(format!(
                    "conformance[{surface:?}] invariant 1 violated: the sole message_delta must \
                     carry a non-null stop_reason"
                ));
            }
            let len = events.len();
            let ends_correctly = len >= 2
                && matches!(events[len - 2], ShapeKind::MessageDelta { .. })
                && matches!(events[len - 1], ShapeKind::MessageStop);
            if !ends_correctly {
                let tail: Vec<&str> = events
                    .iter()
                    .rev()
                    .take(2)
                    .rev()
                    .map(ShapeKind::label)
                    .collect();
                return Err(format!(
                    "conformance[{surface:?}] invariant 5 violated: the last two events must be \
                     message_delta then message_stop, got {tail:?}"
                ));
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::anthropic::AnthropicErrorBody;
    use crate::models::anthropic::AnthropicMessageDeltaBody;
    use crate::models::anthropic::AnthropicMessageStart;
    use crate::models::anthropic::AnthropicUsage;

    fn message_start() -> AnthropicStreamEvent {
        AnthropicStreamEvent::MessageStart {
            message: AnthropicMessageStart {
                id: "msg_test".to_string(),
                kind: "message".to_string(),
                role: "assistant".to_string(),
                content: Vec::new(),
                model: "test-model".to_string(),
                stop_reason: None,
                stop_sequence: None,
                usage: AnthropicUsage {
                    input_tokens: Some(20),
                    output_tokens: Some(0),
                    output_tokens_details: None,
                    server_tool_use: None,
                },
            },
        }
    }

    fn block_start(index: usize, block: AnthropicContentBlockStart) -> AnthropicStreamEvent {
        AnthropicStreamEvent::ContentBlockStart {
            index,
            content_block: block,
        }
    }

    fn block_delta(index: usize, delta: AnthropicDelta) -> AnthropicStreamEvent {
        AnthropicStreamEvent::ContentBlockDelta { index, delta }
    }

    fn block_stop(index: usize) -> AnthropicStreamEvent {
        AnthropicStreamEvent::ContentBlockStop { index }
    }

    fn terminal_message_delta(stop_reason: &str, output_tokens: u64) -> AnthropicStreamEvent {
        AnthropicStreamEvent::MessageDelta {
            delta: AnthropicMessageDeltaBody {
                stop_reason: Some(stop_reason.to_string()),
                stop_sequence: None,
            },
            usage: AnthropicUsage {
                input_tokens: Some(20),
                output_tokens: Some(output_tokens),
                output_tokens_details: None,
                server_tool_use: None,
            },
        }
    }

    /// A PROGRESSIVE (non-terminal) usage `message_delta`, exactly as today's
    /// `record_output_delta` (`mod.rs:679`) emits it: `stop_reason: None`.
    fn progressive_message_delta(output_tokens: u64) -> AnthropicStreamEvent {
        AnthropicStreamEvent::MessageDelta {
            delta: AnthropicMessageDeltaBody {
                stop_reason: None,
                stop_sequence: None,
            },
            usage: AnthropicUsage {
                input_tokens: None,
                output_tokens: Some(output_tokens),
                output_tokens_details: None,
                server_tool_use: None,
            },
        }
    }

    /// Mirrors the native Messages sequence: message_start ->
    /// signed thinking block -> text block -> ONE terminal message_delta ->
    /// message_stop.
    fn conformant_reasoning_text_events() -> Vec<AnthropicStreamEvent> {
        vec![
            message_start(),
            block_start(
                0,
                AnthropicContentBlockStart::Thinking {
                    thinking: String::new(),
                },
            ),
            block_delta(
                0,
                AnthropicDelta::ThinkingDelta {
                    thinking: "We need to add".to_string(),
                },
            ),
            block_delta(
                0,
                AnthropicDelta::ThinkingDelta {
                    thinking: " 17 and 25.".to_string(),
                },
            ),
            block_delta(
                0,
                AnthropicDelta::SignatureDelta {
                    signature: "114a6ac498dd4ad38211901d11c3e48b".to_string(),
                },
            ),
            block_stop(0),
            block_start(
                1,
                AnthropicContentBlockStart::Text {
                    text: String::new(),
                },
            ),
            block_delta(
                1,
                AnthropicDelta::TextDelta {
                    text: "17 + 25".to_string(),
                },
            ),
            block_delta(
                1,
                AnthropicDelta::TextDelta {
                    text: " equals 42.".to_string(),
                },
            ),
            block_stop(1),
            terminal_message_delta("end_turn", 32),
            AnthropicStreamEvent::MessageStop,
        ]
    }

    /// Mirrors TODAY's broken converter shape (pre-C1..C4, see
    /// `converts_reasoning_then_text_response` in
    /// `src/adapters/responses_to_anthropic/tests.rs` for the real sequence
    /// this hand-built vector reproduces): progressive `message_delta`s
    /// appear before the first `content_block_start` (the buffered-reasoning
    /// progress usage), the thinking block closes with NO signature, and
    /// another progressive `message_delta` lands inside the open text block.
    fn non_conformant_todays_shape_events() -> Vec<AnthropicStreamEvent> {
        vec![
            message_start(),
            // Invariant 2 violation: progressive usage for buffered
            // reasoning, emitted before any content_block_start exists.
            progressive_message_delta(3),
            block_start(
                0,
                AnthropicContentBlockStart::Thinking {
                    thinking: String::new(),
                },
            ),
            block_delta(
                0,
                AnthropicDelta::ThinkingDelta {
                    thinking: "Thinking...".to_string(),
                },
            ),
            // Invariant 4 violation: closes with no signature_delta at all.
            block_stop(0),
            block_start(
                1,
                AnthropicContentBlockStart::Text {
                    text: String::new(),
                },
            ),
            block_delta(
                1,
                AnthropicDelta::TextDelta {
                    text: "Answer".to_string(),
                },
            ),
            // Invariant 3 violation: message_delta while block 1 is open.
            progressive_message_delta(5),
            block_stop(1),
            terminal_message_delta("end_turn", 5),
            AnthropicStreamEvent::MessageStop,
        ]
    }

    #[test]
    fn accepts_conformant_reasoning_text_stream() {
        assert_stream_conformant(&conformant_reasoning_text_events(), Surface::ReasoningText);
    }

    #[test]
    fn rejects_todays_broken_shape() {
        let failure = check_stream_conformant(
            &non_conformant_todays_shape_events(),
            Surface::ReasoningText,
        )
        .expect_err("today's broken shape must be rejected");
        // The FIRST violation (event order) wins: today's shape trips
        // invariant 2 (progressive delta before the first
        // content_block_start) before the walk ever reaches the unsigned
        // thinking block or the mid-block delta.
        assert!(
            failure.contains("invariant 2"),
            "expected invariant 2 to fire first, got: {failure}"
        );
    }

    #[test]
    #[should_panic(expected = "invariant 2")]
    fn assert_form_panics_on_todays_broken_shape() {
        assert_stream_conformant(
            &non_conformant_todays_shape_events(),
            Surface::ReasoningText,
        );
    }

    #[test]
    fn accepts_conformant_reasoning_text_stream_json_form() {
        let json_events: Vec<Value> = conformant_reasoning_text_events()
            .iter()
            .map(|event| serde_json::to_value(event).expect("serialize AnthropicStreamEvent"))
            .collect();
        assert_sse_conformant(&json_events, Surface::ReasoningText);
    }

    #[test]
    fn rejects_todays_broken_shape_json_form() {
        let json_events: Vec<Value> = non_conformant_todays_shape_events()
            .iter()
            .map(|event| serde_json::to_value(event).expect("serialize AnthropicStreamEvent"))
            .collect();
        let failure = check_sse_conformant(&json_events, Surface::ReasoningText)
            .expect_err("today's broken shape must be rejected (JSON form)");
        assert!(failure.contains("invariant 2"), "got: {failure}");
    }

    // -- Targeted single-invariant tests: each isolates ONE violation so a
    // later phase (C1-C4) can trust that a failure message names the RIGHT
    // invariant. --

    #[test]
    fn invariant1_rejects_more_than_one_message_delta() {
        let events = vec![
            message_start(),
            block_start(
                0,
                AnthropicContentBlockStart::Text {
                    text: String::new(),
                },
            ),
            block_delta(
                0,
                AnthropicDelta::TextDelta {
                    text: "hi".to_string(),
                },
            ),
            block_stop(0),
            terminal_message_delta("end_turn", 1),
            terminal_message_delta("end_turn", 1),
            AnthropicStreamEvent::MessageStop,
        ];
        let failure = check_stream_conformant(&events, Surface::TextOnly).expect_err("must reject");
        assert!(failure.contains("invariant 1"), "got: {failure}");
    }

    #[test]
    fn invariant1_rejects_null_stop_reason_on_the_sole_message_delta() {
        let events = vec![
            message_start(),
            block_start(
                0,
                AnthropicContentBlockStart::Text {
                    text: String::new(),
                },
            ),
            block_delta(
                0,
                AnthropicDelta::TextDelta {
                    text: "hi".to_string(),
                },
            ),
            block_stop(0),
            // stop_reason: None, but the ONLY message_delta in the stream.
            progressive_message_delta(1),
            AnthropicStreamEvent::MessageStop,
        ];
        let failure = check_stream_conformant(&events, Surface::TextOnly).expect_err("must reject");
        assert!(failure.contains("invariant 1"), "got: {failure}");
    }

    #[test]
    fn invariant2_rejects_message_delta_before_first_content_block_start() {
        let events = vec![
            message_start(),
            progressive_message_delta(1),
            AnthropicStreamEvent::MessageStop,
        ];
        let failure = check_stream_conformant(&events, Surface::TextOnly).expect_err("must reject");
        assert!(failure.contains("invariant 2"), "got: {failure}");
    }

    #[test]
    fn invariant3_rejects_message_delta_inside_open_block() {
        let events = vec![
            message_start(),
            block_start(
                0,
                AnthropicContentBlockStart::Text {
                    text: String::new(),
                },
            ),
            block_delta(
                0,
                AnthropicDelta::TextDelta {
                    text: "hi".to_string(),
                },
            ),
            progressive_message_delta(1),
            block_stop(0),
            terminal_message_delta("end_turn", 1),
            AnthropicStreamEvent::MessageStop,
        ];
        let failure = check_stream_conformant(&events, Surface::TextOnly).expect_err("must reject");
        assert!(failure.contains("invariant 3"), "got: {failure}");
    }

    #[test]
    fn invariant4_rejects_thinking_block_with_empty_signature() {
        let events = vec![
            message_start(),
            block_start(
                0,
                AnthropicContentBlockStart::Thinking {
                    thinking: String::new(),
                },
            ),
            block_delta(
                0,
                AnthropicDelta::ThinkingDelta {
                    thinking: "hm".to_string(),
                },
            ),
            block_delta(
                0,
                AnthropicDelta::SignatureDelta {
                    signature: String::new(), // empty!
                },
            ),
            block_stop(0),
            terminal_message_delta("end_turn", 1),
            AnthropicStreamEvent::MessageStop,
        ];
        let failure =
            check_stream_conformant(&events, Surface::ReasoningText).expect_err("must reject");
        assert!(failure.contains("invariant 4"), "got: {failure}");
    }

    #[test]
    fn invariant4_rejects_thinking_block_with_absent_signature() {
        let events = vec![
            message_start(),
            block_start(
                0,
                AnthropicContentBlockStart::Thinking {
                    thinking: String::new(),
                },
            ),
            block_delta(
                0,
                AnthropicDelta::ThinkingDelta {
                    thinking: "hm".to_string(),
                },
            ),
            // No signature_delta at all before the stop.
            block_stop(0),
            terminal_message_delta("end_turn", 1),
            AnthropicStreamEvent::MessageStop,
        ];
        let failure =
            check_stream_conformant(&events, Surface::ReasoningText).expect_err("must reject");
        assert!(failure.contains("invariant 4"), "got: {failure}");
    }

    #[test]
    fn invariant5_rejects_wrong_terminal_order() {
        let events = vec![
            message_start(),
            block_start(
                0,
                AnthropicContentBlockStart::Text {
                    text: String::new(),
                },
            ),
            block_delta(
                0,
                AnthropicDelta::TextDelta {
                    text: "hi".to_string(),
                },
            ),
            block_stop(0),
            terminal_message_delta("end_turn", 1),
            AnthropicStreamEvent::Ping, // not message_stop
        ];
        let failure = check_stream_conformant(&events, Surface::TextOnly).expect_err("must reject");
        assert!(failure.contains("invariant 5"), "got: {failure}");
    }

    #[test]
    fn error_surface_accepts_trailing_error_event() {
        let events = vec![
            message_start(),
            AnthropicStreamEvent::Error {
                error: AnthropicErrorBody {
                    kind: "api_error".to_string(),
                    message: "boom".to_string(),
                },
            },
        ];
        assert_stream_conformant(&events, Surface::Error);
    }

    #[test]
    fn error_surface_rejects_stream_not_ending_in_error() {
        let events = vec![
            message_start(),
            block_start(
                0,
                AnthropicContentBlockStart::Text {
                    text: String::new(),
                },
            ),
            block_stop(0),
            terminal_message_delta("end_turn", 0),
            AnthropicStreamEvent::MessageStop,
        ];
        let failure = check_stream_conformant(&events, Surface::Error).expect_err("must reject");
        assert!(failure.contains("invariant 6"), "got: {failure}");
    }

    #[test]
    fn rejects_empty_event_stream() {
        let failure = check_stream_conformant(&[], Surface::TextOnly).expect_err("must reject");
        assert!(failure.contains("empty"), "got: {failure}");
    }
}
