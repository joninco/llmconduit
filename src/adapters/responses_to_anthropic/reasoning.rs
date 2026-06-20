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
//! Rules (unchanged from G8 — pure structural extraction, no behavior change):
//! - Reasoning is buffered, not emitted live, so its final shape is decided
//!   once the stream's shape is known.
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
    /// Buffered reasoning text deltas, flushed (promoted or as thinking) once
    /// the stream shape is known.
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
    /// Whether there is any buffered reasoning (text or signature) to flush.
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

    /// Push a buffered reasoning text delta.
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
