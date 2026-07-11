//! G8 reasoning egress state machine (T8).
//!
//! The promotion/suppression matrix for deferred reasoning lives here as one
//! typed state, instead of scattered across `reasoning_buffer`,
//! `reasoning_signature`, `content_started`, `has_tool_calls`, and repeated
//! `flush_*` conditionals in the stream converter. `AnthropicStreamConverter`
//! holds a `ReasoningEgressState` and delegates the buffer/hold/promote
//! decisions to it; the BLOCK EMISSION (thinking/text blocks, block indices,
//! `open_block`) stays on the converter, which owns that machinery.
//!
//! Rules:
//! - Unrequested reasoning is buffered so its final shape can be decided once
//!   the stream shape is known (the original G8 compatibility behavior).
//! - When an Anthropic request explicitly enables thinking, the converter emits
//!   reasoning live while this state retains the same text for signing.
//! - Reasoning arriving after text/tool output has begun is "late" and dropped
//!   (`is_late_reasoning`).
//! - At a terminal event, reasoning-only output is PROMOTED to a `text` block
//!   ONLY on a clean stop AND no content started AND no tool calls AND no
//!   signature (`should_promote`). Everything else flushes as a `thinking`
//!   block.

/// Owns the four cross-cutting reasoning-egress fields + the promote/hold
/// decisions (T8). The converter delegates transitions + queries to this;
/// block emission stays on the converter.
#[derive(Debug, Default)]
pub(super) struct ReasoningEgressState {
    /// Retained reasoning text deltas. Deferred mode flushes these once the
    /// stream shape is known; live mode emits each immediately and retains them
    /// only until the block closes with a deterministic signature.
    pub(super) reasoning_buffer: Vec<String>,
    /// Accumulated reasoning signature (genuine chain-of-thought marker).
    /// Pins the buffer to a `thinking` block (never promoted) when present.
    pub(super) reasoning_signature: Option<String>,
    /// The late-reasoning drop gate. Set ONLY by real text/tool output. NOT set
    /// by the additive `response.web_search_results` block (continuation
    /// reasoning after a search must buffer normally). Tracked separately via
    /// the converter's `web_search_count`.
    pub(super) content_started: bool,
    /// Whether the turn produced tool-call output. A tool-call terminal is not a
    /// clean-stop-only promotion (reasoning prefaced tools, not a final answer).
    pub(super) has_tool_calls: bool,
}

impl ReasoningEgressState {
    /// Whether there is any retained reasoning (text or signature) to flush or
    /// finish signing.
    pub(super) fn has_buffered(&self) -> bool {
        !self.reasoning_buffer.is_empty() || self.reasoning_signature.is_some()
    }

    /// Whether a reasoning delta arriving now is "late" (after text/tool output
    /// began) and must be dropped.
    pub(super) fn is_late_reasoning(&self) -> bool {
        self.content_started
    }

    /// Mark that real text/tool content has started (the late-reasoning gate).
    /// Idempotent.
    pub(super) fn note_content_started(&mut self) {
        self.content_started = true;
    }

    /// Mark that the turn produced tool-call output. Idempotent.
    pub(super) fn note_tool_calls(&mut self) {
        self.has_tool_calls = true;
    }

    /// Retain a reasoning text delta.
    pub(super) fn push_reasoning(&mut self, delta: &str) {
        self.reasoning_buffer.push(delta.to_string());
    }

    /// Accumulate a reasoning signature delta (multi-chunk concat in order).
    pub(super) fn push_signature(&mut self, signature: &str) {
        self.reasoning_signature
            .get_or_insert_with(String::new)
            .push_str(signature);
    }

    /// The promotion decision at the terminal event (G8 core matrix). Promote
    /// reasoning to a `text` block ONLY on a clean stop AND no content started
    /// AND no tool calls AND no signature. Everything else flushes as thinking.
    pub(super) fn should_promote(&self, clean_stop: bool) -> bool {
        clean_stop
            && !self.content_started
            && !self.has_tool_calls
            && self.reasoning_signature.is_none()
    }

    /// Take the buffered reasoning text (concatenated), consuming the buffer.
    pub(super) fn take_buffer(&mut self) -> String {
        std::mem::take(&mut self.reasoning_buffer).concat()
    }

    /// Take the accumulated signature, consuming it.
    pub(super) fn take_signature(&mut self) -> Option<String> {
        self.reasoning_signature.take()
    }
}
