//! G4 server-tool delta gate.
//!
//! When the image agent is active for a turn, the upstream may stream
//! `function_call_arguments` deltas for the internal `analyzeImage` tool. Those
//! must never reach the client: the tool is a server-side image classifier, not
//! a client-visible tool call. But sparse upstream chunks can stream argument
//! fragments BEFORE the function name arrives, so a leading `analyzeImage`
//! fragment would otherwise leak (the name is still `None` at that point).
//!
//! [`ToolDeltaGate`] owns the per-`call_id` delta-buffer state machine: leading
//! deltas are buffered while the name is unknown, then either DROPPED (internal
//! `analyzeImage`) or FLUSHED in order (a real client tool, whose later deltas
//! then forward straight through). It is a pure decision machine — it never
//! touches the SSE channel or the monitor hub. Each method returns the ordered
//! [`DeltaEmission`]s the engine should forward, so the engine has a single
//! emission path instead of the four duplicated `monitor.emit + send_event`
//! sites this consolidates.

use std::collections::HashMap;

use crate::vision::ANALYZE_IMAGE_TOOL_NAME;

/// Per-call cap on still-name-unknown buffered tool-call argument bytes (G4
/// round-2 #4 DoS guard). 256 KiB is far above any real leading
/// arguments-before-name fragment (tool names arrive within the first chunk or
/// two) while bounding a single hostile call.
const MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL: usize = 256 * 1024;

/// Total cap across ALL still-pending buffers in one upstream turn (G4 round-2
/// #4). Bounds the aggregate even if an upstream opens many nameless calls.
const MAX_PENDING_TOOL_DELTA_BYTES_TOTAL: usize = 1024 * 1024;

/// Per-call_id streaming state for G4 `analyzeImage` delta hiding (review #1).
/// A tool call starts `Pending` (name unknown) with its leading argument deltas
/// buffered; once the name resolves it transitions to `Drop` (internal
/// `analyzeImage` — buffer discarded, all deltas suppressed) or `Emit` (a client
/// tool — buffer flushed in order, later deltas forwarded).
enum AnalyzeDeltaState {
    Pending {
        buffered: Vec<(Option<String>, String)>,
    },
    Drop,
    Emit,
}

/// Total buffered argument bytes held in a `Pending` buffer (delta payloads
/// only; the optional name is bookkeeping).
fn buffered_len(buffered: &[(Option<String>, String)]) -> usize {
    buffered.iter().map(|(_, delta)| delta.len()).sum()
}

/// One `function_call_arguments` delta the engine should forward to the client
/// (and mirror to the monitor hub). The gate yields these in the exact order
/// they must be emitted; the engine drives them through its single emission
/// helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeltaEmission {
    pub call_id: String,
    pub name: Option<String>,
    pub delta: String,
}

/// Returned when an upstream streams more buffered argument bytes before a tool
/// name than the DoS caps allow. The engine maps this to `AppError::upstream`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PendingBufferOverflow;

/// Per-upstream-turn gate over streamed `function_call_arguments` deltas.
///
/// Construct one per upstream turn via [`ToolDeltaGate::new`]. When the image
/// agent is inactive the gate is a pass-through (deltas forward unchanged); when
/// active it buffers/drops/flushes per the `analyzeImage` rules above.
pub(crate) struct ToolDeltaGate {
    /// Whether the image agent is active for this turn. When `false` the gate is
    /// a pure pass-through and `buffer`/`pending_buffer_bytes` stay empty.
    vision_active: bool,
    /// Per-`call_id` buffering decision. Only populated when `vision_active`.
    buffer: HashMap<String, AnalyzeDeltaState>,
    /// Total bytes held across all still-`Pending` (name-unknown) buffers this
    /// turn. Bounded so a hostile/buggy upstream streaming endless arguments
    /// before ever sending a tool name cannot grow memory without limit.
    pending_buffer_bytes: usize,
}

impl ToolDeltaGate {
    /// `vision_active` mirrors `vision_session.is_some()` for the turn.
    pub(crate) fn new(vision_active: bool) -> Self {
        Self {
            vision_active,
            buffer: HashMap::new(),
            pending_buffer_bytes: 0,
        }
    }

    /// Process one streamed `function_call_arguments` delta, returning the
    /// ordered emissions the engine should forward.
    ///
    /// Fast path (image agent inactive): the single delta forwards unchanged.
    /// Active path: leading deltas buffer until the name resolves, then drop
    /// (internal `analyzeImage`) or flush-in-order + forward (client tool).
    pub(crate) fn on_delta(
        &mut self,
        call_id: String,
        name: Option<String>,
        delta: String,
    ) -> Result<Vec<DeltaEmission>, PendingBufferOverflow> {
        // Fast path: when the image agent is inactive there is nothing to hide,
        // so stream the delta unchanged.
        if !self.vision_active {
            return Ok(vec![DeltaEmission {
                call_id,
                name,
                delta,
            }]);
        }

        // Image agent active — gate each call_id on its resolved tool name,
        // buffering leading deltas that arrive before the name is known so an
        // internal `analyzeImage` arg fragment can never leak.
        let is_analyze = name
            .as_deref()
            .map(|n| n.eq_ignore_ascii_case(ANALYZE_IMAGE_TOOL_NAME));
        let entry = self
            .buffer
            .entry(call_id.clone())
            .or_insert(AnalyzeDeltaState::Pending {
                buffered: Vec::new(),
            });
        match (entry, is_analyze) {
            // Already decided to drop this call's deltas (internal analyzeImage)
            // — drop this one too.
            (AnalyzeDeltaState::Drop, _) => Ok(Vec::new()),
            // Name resolved to analyzeImage now: discard any buffered leading
            // fragments and mark drop. The buffered bytes leave the pending
            // pool.
            (slot, Some(true)) => {
                if let AnalyzeDeltaState::Pending { buffered } = slot {
                    self.pending_buffer_bytes = self
                        .pending_buffer_bytes
                        .saturating_sub(buffered_len(buffered));
                }
                *slot = AnalyzeDeltaState::Drop;
                Ok(Vec::new())
            }
            // Already decided to emit (a known client tool) — forward
            // immediately.
            (AnalyzeDeltaState::Emit, _) => Ok(vec![DeltaEmission {
                call_id,
                name,
                delta,
            }]),
            // Name resolved to a non-analyzeImage client tool: flush any buffered
            // leading fragments in order, then this delta, and remember to emit
            // the rest.
            (slot, Some(false)) => {
                let buffered = match std::mem::replace(slot, AnalyzeDeltaState::Emit) {
                    AnalyzeDeltaState::Pending { buffered } => buffered,
                    _ => Vec::new(),
                };
                self.pending_buffer_bytes = self
                    .pending_buffer_bytes
                    .saturating_sub(buffered_len(&buffered));
                let mut emissions = Vec::with_capacity(buffered.len() + 1);
                for (buffered_name, buffered_delta) in buffered {
                    emissions.push(DeltaEmission {
                        call_id: call_id.clone(),
                        name: buffered_name,
                        delta: buffered_delta,
                    });
                }
                emissions.push(DeltaEmission {
                    call_id,
                    name,
                    delta,
                });
                Ok(emissions)
            }
            // Name still unknown: buffer this delta until it resolves (or the
            // turn ends). Enforce a per-call AND total pending-byte cap so an
            // upstream that streams endless args-before-name cannot exhaust
            // memory; exceeding it fails the turn cleanly.
            (AnalyzeDeltaState::Pending { buffered }, None) => {
                let delta_bytes = delta.len();
                if buffered_len(buffered) + delta_bytes > MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL
                    || self.pending_buffer_bytes + delta_bytes > MAX_PENDING_TOOL_DELTA_BYTES_TOTAL
                {
                    return Err(PendingBufferOverflow);
                }
                buffered.push((name, delta));
                self.pending_buffer_bytes += delta_bytes;
                Ok(Vec::new())
            }
        }
    }

    /// Flush the still-`Pending` buffer for a CLIENT tool whose arguments
    /// streamed entirely before its name (G4 round-2 #5).
    ///
    /// Such a tool's name arrived name-only, so no delta ever triggered the
    /// flush in [`Self::on_delta`]; its leading deltas are still buffered. The
    /// engine calls this for each finalized non-`ImageAnalysis` tool call (in
    /// `finalized.tool_calls` order) so the client receives all of its tool-arg
    /// deltas, in order, before the public items and the
    /// `function_call_arguments.done`. Returns nothing if the call has no
    /// pending buffer (already flushed/dropped, or never seen).
    pub(crate) fn flush_pending_client_tool(&mut self, call_id: &str) -> Vec<DeltaEmission> {
        if let Some(AnalyzeDeltaState::Pending { buffered }) = self.buffer.remove(call_id) {
            buffered
                .into_iter()
                .map(|(buffered_name, buffered_delta)| DeltaEmission {
                    call_id: call_id.to_string(),
                    name: buffered_name,
                    delta: buffered_delta,
                })
                .collect()
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn em(call_id: &str, name: Option<&str>, delta: &str) -> DeltaEmission {
        DeltaEmission {
            call_id: call_id.to_string(),
            name: name.map(str::to_string),
            delta: delta.to_string(),
        }
    }

    #[test]
    fn inactive_gate_passes_every_delta_through_unchanged() {
        let mut gate = ToolDeltaGate::new(false);
        // Even an analyzeImage-named delta forwards verbatim when the agent is
        // inactive: a client-supplied tool of that name is a normal client tool.
        let out = gate
            .on_delta(
                "c1".into(),
                Some(ANALYZE_IMAGE_TOOL_NAME.into()),
                "{".into(),
            )
            .unwrap();
        assert_eq!(out, vec![em("c1", Some(ANALYZE_IMAGE_TOOL_NAME), "{")]);
        let out = gate.on_delta("c1".into(), None, "x".into()).unwrap();
        assert_eq!(out, vec![em("c1", None, "x")]);
        // Nothing buffered, so the post-loop flush is a no-op.
        assert!(gate.flush_pending_client_tool("c1").is_empty());
    }

    #[test]
    fn active_gate_drops_analyze_image_once_name_resolves() {
        let mut gate = ToolDeltaGate::new(true);
        // Leading fragment arrives before the name: buffered, nothing emitted.
        assert!(
            gate.on_delta("c1".into(), None, "{\"ima".into())
                .unwrap()
                .is_empty()
        );
        // Name resolves to analyzeImage: buffered leading fragment is discarded,
        // this delta dropped.
        assert!(
            gate.on_delta(
                "c1".into(),
                Some(ANALYZE_IMAGE_TOOL_NAME.into()),
                "ge".into()
            )
            .unwrap()
            .is_empty()
        );
        // Subsequent deltas (even name-less) stay dropped.
        assert!(
            gate.on_delta("c1".into(), None, "rest".into())
                .unwrap()
                .is_empty()
        );
        // Buffer was consumed into Drop, so the post-loop flush emits nothing.
        assert!(gate.flush_pending_client_tool("c1").is_empty());
    }

    #[test]
    fn active_gate_drops_analyze_image_named_on_first_delta() {
        let mut gate = ToolDeltaGate::new(true);
        // Name known from the very first delta: dropped with no buffering.
        assert!(
            gate.on_delta(
                "c1".into(),
                Some(ANALYZE_IMAGE_TOOL_NAME.to_ascii_uppercase()),
                "{}".into(),
            )
            .unwrap()
            .is_empty()
        );
        assert!(gate.flush_pending_client_tool("c1").is_empty());
    }

    #[test]
    fn active_gate_flushes_buffered_client_deltas_in_order_then_forwards() {
        let mut gate = ToolDeltaGate::new(true);
        // Two leading fragments buffered before the name is known.
        assert!(
            gate.on_delta("c1".into(), None, "{\"a".into())
                .unwrap()
                .is_empty()
        );
        assert!(
            gate.on_delta("c1".into(), None, "\":1".into())
                .unwrap()
                .is_empty()
        );
        // Name resolves to a client tool: buffered fragments flush IN ORDER,
        // then the triggering delta — all forwarded.
        let out = gate
            .on_delta("c1".into(), Some("lookup".into()), "}".into())
            .unwrap();
        assert_eq!(
            out,
            vec![
                em("c1", None, "{\"a"),
                em("c1", None, "\":1"),
                em("c1", Some("lookup"), "}"),
            ]
        );
        // Now in Emit: later deltas forward straight through, one-for-one.
        let out = gate
            .on_delta("c1".into(), Some("lookup".into()), " more".into())
            .unwrap();
        assert_eq!(out, vec![em("c1", Some("lookup"), " more")]);
        // Buffer already drained by the flush; post-loop flush is a no-op.
        assert!(gate.flush_pending_client_tool("c1").is_empty());
    }

    #[test]
    fn active_gate_flushes_name_only_client_tool_at_turn_end() {
        let mut gate = ToolDeltaGate::new(true);
        // A client tool whose args streamed entirely before its name: deltas
        // buffer and no delta ever carries the name, so nothing is emitted live.
        assert!(
            gate.on_delta("c1".into(), None, "{\"q".into())
                .unwrap()
                .is_empty()
        );
        assert!(
            gate.on_delta("c1".into(), None, "\":2}".into())
                .unwrap()
                .is_empty()
        );
        // Post-loop flush (engine drives this for each non-ImageAnalysis tool
        // call) replays the buffered deltas in order.
        let out = gate.flush_pending_client_tool("c1");
        assert_eq!(out, vec![em("c1", None, "{\"q"), em("c1", None, "\":2}")]);
        // Buffer removed: a second flush yields nothing.
        assert!(gate.flush_pending_client_tool("c1").is_empty());
    }

    #[test]
    fn distinct_call_ids_are_gated_independently() {
        let mut gate = ToolDeltaGate::new(true);
        // c1 buffers, c2 resolves immediately to a client tool.
        assert!(
            gate.on_delta("c1".into(), None, "a".into())
                .unwrap()
                .is_empty()
        );
        let out = gate
            .on_delta("c2".into(), Some("lookup".into()), "b".into())
            .unwrap();
        assert_eq!(out, vec![em("c2", Some("lookup"), "b")]);
        // c1 then resolves to analyzeImage and is dropped — c2 unaffected.
        assert!(
            gate.on_delta(
                "c1".into(),
                Some(ANALYZE_IMAGE_TOOL_NAME.into()),
                "c".into()
            )
            .unwrap()
            .is_empty()
        );
        assert!(gate.flush_pending_client_tool("c1").is_empty());
        assert!(gate.flush_pending_client_tool("c2").is_empty());
    }

    #[test]
    fn per_call_pending_byte_cap_overflows() {
        let mut gate = ToolDeltaGate::new(true);
        // One nameless delta just over the per-call cap fails the turn.
        let huge = "x".repeat(MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL + 1);
        assert_eq!(
            gate.on_delta("c1".into(), None, huge),
            Err(PendingBufferOverflow)
        );
    }

    #[test]
    fn total_pending_byte_cap_overflows_across_calls() {
        let mut gate = ToolDeltaGate::new(true);
        // Fill close to the total cap across several calls, each under the
        // per-call cap, then tip over the aggregate limit.
        let chunk = "y".repeat(MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL);
        let calls = MAX_PENDING_TOOL_DELTA_BYTES_TOTAL / MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL;
        for i in 0..calls {
            assert!(
                gate.on_delta(format!("c{i}"), None, chunk.clone())
                    .unwrap()
                    .is_empty()
            );
        }
        // The aggregate is now at the total cap; one more byte overflows.
        assert_eq!(
            gate.on_delta("overflow".into(), None, "z".into()),
            Err(PendingBufferOverflow)
        );
    }

    #[test]
    fn dropping_analyze_image_frees_pending_budget_for_other_calls() {
        let mut gate = ToolDeltaGate::new(true);
        // Buffer a large nameless fragment, then resolve it to analyzeImage so
        // its bytes leave the pending pool.
        let chunk = "y".repeat(MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL);
        assert!(
            gate.on_delta("img".into(), None, chunk.clone())
                .unwrap()
                .is_empty()
        );
        assert!(
            gate.on_delta(
                "img".into(),
                Some(ANALYZE_IMAGE_TOOL_NAME.into()),
                "".into()
            )
            .unwrap()
            .is_empty()
        );
        // Because the budget was reclaimed, another call may buffer the same
        // amount without overflowing the total cap.
        assert!(
            gate.on_delta("other".into(), None, chunk)
                .unwrap()
                .is_empty()
        );
    }
}
