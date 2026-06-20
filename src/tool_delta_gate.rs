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
//! touches the SSE channel or the monitor hub. Each method returns a
//! [`DeltaDecision`] the engine drives through its single emission path,
//! replacing the four duplicated `monitor.emit + send_event` sites this
//! consolidates.
//!
//! The decision is allocation-free on the hot path: the common single-delta
//! forward returns [`DeltaDecision::One`] (no `Vec`), and a flush MOVES the
//! already-allocated pending buffer out of the gate ([`DeltaDecision::Flush`])
//! so the engine iterates it in place — no fresh per-delta allocation either
//! way. This preserves the inline original's zero-allocation streaming.

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

/// What the engine should emit for a single gated delta. Kept allocation-free on
/// the hot path: `None`/`One` never touch the heap, and `Flush` MOVES the
/// already-buffered fragments out of the gate so they are iterated in place
/// rather than copied into a fresh `Vec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeltaDecision {
    /// Drop this delta (internal `analyzeImage`, or still buffering) — emit
    /// nothing.
    None,
    /// Forward exactly one delta (fast path, or an already-resolved client
    /// tool).
    One(DeltaEmission),
    /// A client tool's name just resolved: emit the buffered leading fragments
    /// in order under `call_id`, then the optional `trailing` triggering delta
    /// (its `(name, delta)` — it shares `call_id`, so the id is NOT duplicated
    /// here; the engine pairs it with `call_id`). `buffered` is the gate's own
    /// moved-out buffer (no copy). Used both for the in-stream resolve (with
    /// `trailing`) and the turn-end flush (without).
    Flush {
        call_id: String,
        buffered: Vec<(Option<String>, String)>,
        trailing: Option<(Option<String>, String)>,
    },
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
    /// allocation-free [`DeltaDecision`] the engine should emit.
    ///
    /// Fast path (image agent inactive): the single delta forwards unchanged
    /// (`One`). Active path: leading deltas buffer until the name resolves
    /// (`None` while buffering/dropping), then drop (internal `analyzeImage`) or
    /// flush-in-order + forward (`Flush`, moving the buffer out of the gate).
    pub(crate) fn on_delta(
        &mut self,
        call_id: String,
        name: Option<String>,
        delta: String,
    ) -> Result<DeltaDecision, PendingBufferOverflow> {
        // Fast path: when the image agent is inactive there is nothing to hide,
        // so stream the delta unchanged.
        if !self.vision_active {
            return Ok(DeltaDecision::One(DeltaEmission {
                call_id,
                name,
                delta,
            }));
        }

        // Image agent active — gate each call_id on its resolved tool name,
        // buffering leading deltas that arrive before the name is known so an
        // internal `analyzeImage` arg fragment can never leak.
        let is_analyze = name
            .as_deref()
            .map(|n| n.eq_ignore_ascii_case(ANALYZE_IMAGE_TOOL_NAME));
        // Borrowed lookup: the map keys on `&str`, so resolving an already-seen
        // call needs NO clone (unlike `entry(call_id.clone())`, which cloned on
        // every delta). The owned key is cloned only the first time a call_id is
        // seen — inherent, as the map must own its key.
        if !self.buffer.contains_key(call_id.as_str()) {
            self.buffer.insert(
                call_id.clone(),
                AnalyzeDeltaState::Pending {
                    buffered: Vec::new(),
                },
            );
        }
        let slot = self
            .buffer
            .get_mut(call_id.as_str())
            .expect("entry inserted above");
        match (slot, is_analyze) {
            // Already decided to drop this call's deltas (internal analyzeImage)
            // — drop this one too.
            (AnalyzeDeltaState::Drop, _) => Ok(DeltaDecision::None),
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
                Ok(DeltaDecision::None)
            }
            // Already decided to emit (a known client tool) — forward
            // immediately. `call_id` is MOVED into the emission (last use).
            (AnalyzeDeltaState::Emit, _) => Ok(DeltaDecision::One(DeltaEmission {
                call_id,
                name,
                delta,
            })),
            // Name resolved to a non-analyzeImage client tool: flush any buffered
            // leading fragments in order, then this delta, and remember to emit
            // the rest. The buffer is MOVED out of the gate (no copy); the engine
            // iterates it in place. When nothing was buffered (the common case:
            // the name arrived with the first delta) collapse to `One`, MOVING
            // `call_id` into the emission so this path allocates nothing.
            (slot, Some(false)) => {
                let buffered = match std::mem::replace(slot, AnalyzeDeltaState::Emit) {
                    AnalyzeDeltaState::Pending { buffered } => buffered,
                    _ => Vec::new(),
                };
                if buffered.is_empty() {
                    return Ok(DeltaDecision::One(DeltaEmission {
                        call_id,
                        name,
                        delta,
                    }));
                }
                self.pending_buffer_bytes = self
                    .pending_buffer_bytes
                    .saturating_sub(buffered_len(&buffered));
                // `call_id` lives on `Flush`; the trailing delta shares it, so we
                // carry only its `(name, delta)` rather than duplicating the id.
                Ok(DeltaDecision::Flush {
                    call_id,
                    buffered,
                    trailing: Some((name, delta)),
                })
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
                Ok(DeltaDecision::None)
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
    /// `function_call_arguments.done`. Returns [`DeltaDecision::None`] if the
    /// call has no pending buffer (already flushed/dropped, or never seen);
    /// otherwise a [`DeltaDecision::Flush`] that MOVES the buffer out (no copy)
    /// with no `trailing` delta.
    pub(crate) fn flush_pending_client_tool(&mut self, call_id: &str) -> DeltaDecision {
        match self.buffer.remove(call_id) {
            Some(AnalyzeDeltaState::Pending { buffered }) if !buffered.is_empty() => {
                self.pending_buffer_bytes = self
                    .pending_buffer_bytes
                    .saturating_sub(buffered_len(&buffered));
                DeltaDecision::Flush {
                    call_id: call_id.to_string(),
                    buffered,
                    trailing: None,
                }
            }
            _ => DeltaDecision::None,
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

    /// Flatten a decision into the ordered emissions it produces, so assertions
    /// stay expressed as the exact wire sequence (equivalent to the old
    /// `Vec<DeltaEmission>` return) regardless of the `None`/`One`/`Flush` shape.
    fn emissions(decision: DeltaDecision) -> Vec<DeltaEmission> {
        match decision {
            DeltaDecision::None => Vec::new(),
            DeltaDecision::One(emission) => vec![emission],
            DeltaDecision::Flush {
                call_id,
                buffered,
                trailing,
            } => buffered
                .into_iter()
                .chain(trailing)
                .map(|(name, delta)| DeltaEmission {
                    call_id: call_id.clone(),
                    name,
                    delta,
                })
                .collect(),
        }
    }

    #[test]
    fn inactive_gate_passes_every_delta_through_unchanged() {
        let mut gate = ToolDeltaGate::new(false);
        // Even an analyzeImage-named delta forwards verbatim when the agent is
        // inactive: a client-supplied tool of that name is a normal client tool.
        // The fast path returns a bare `One` (no Vec).
        let out = gate
            .on_delta(
                "c1".into(),
                Some(ANALYZE_IMAGE_TOOL_NAME.into()),
                "{".into(),
            )
            .unwrap();
        assert_eq!(
            out,
            DeltaDecision::One(em("c1", Some(ANALYZE_IMAGE_TOOL_NAME), "{"))
        );
        let out = gate.on_delta("c1".into(), None, "x".into()).unwrap();
        assert_eq!(out, DeltaDecision::One(em("c1", None, "x")));
        // Nothing buffered, so the post-loop flush is a no-op.
        assert_eq!(gate.flush_pending_client_tool("c1"), DeltaDecision::None);
    }

    #[test]
    fn active_gate_drops_analyze_image_once_name_resolves() {
        let mut gate = ToolDeltaGate::new(true);
        // Leading fragment arrives before the name: buffered, nothing emitted.
        assert_eq!(
            gate.on_delta("c1".into(), None, "{\"ima".into()).unwrap(),
            DeltaDecision::None
        );
        // Name resolves to analyzeImage: buffered leading fragment is discarded,
        // this delta dropped.
        assert_eq!(
            gate.on_delta(
                "c1".into(),
                Some(ANALYZE_IMAGE_TOOL_NAME.into()),
                "ge".into()
            )
            .unwrap(),
            DeltaDecision::None
        );
        // Subsequent deltas (even name-less) stay dropped.
        assert_eq!(
            gate.on_delta("c1".into(), None, "rest".into()).unwrap(),
            DeltaDecision::None
        );
        // Buffer was consumed into Drop, so the post-loop flush emits nothing.
        assert_eq!(gate.flush_pending_client_tool("c1"), DeltaDecision::None);
    }

    #[test]
    fn active_gate_drops_analyze_image_named_on_first_delta() {
        let mut gate = ToolDeltaGate::new(true);
        // Name known from the very first delta: dropped with no buffering.
        assert_eq!(
            gate.on_delta(
                "c1".into(),
                Some(ANALYZE_IMAGE_TOOL_NAME.to_ascii_uppercase()),
                "{}".into(),
            )
            .unwrap(),
            DeltaDecision::None
        );
        assert_eq!(gate.flush_pending_client_tool("c1"), DeltaDecision::None);
    }

    #[test]
    fn active_gate_flushes_buffered_client_deltas_in_order_then_forwards() {
        let mut gate = ToolDeltaGate::new(true);
        // Two leading fragments buffered before the name is known.
        assert_eq!(
            gate.on_delta("c1".into(), None, "{\"a".into()).unwrap(),
            DeltaDecision::None
        );
        assert_eq!(
            gate.on_delta("c1".into(), None, "\":1".into()).unwrap(),
            DeltaDecision::None
        );
        // Name resolves to a client tool: a single `Flush` carries the moved
        // buffer + the triggering delta. Emit order: buffered IN ORDER, then the
        // trigger.
        let decision = gate
            .on_delta("c1".into(), Some("lookup".into()), "}".into())
            .unwrap();
        assert_eq!(
            decision,
            DeltaDecision::Flush {
                call_id: "c1".into(),
                buffered: vec![(None, "{\"a".into()), (None, "\":1".into())],
                // trailing shares `call_id` — carries only (name, delta).
                trailing: Some((Some("lookup".into()), "}".into())),
            }
        );
        assert_eq!(
            emissions(decision),
            vec![
                em("c1", None, "{\"a"),
                em("c1", None, "\":1"),
                em("c1", Some("lookup"), "}"),
            ]
        );
        // Now in Emit: later deltas forward straight through as a bare `One`.
        let out = gate
            .on_delta("c1".into(), Some("lookup".into()), " more".into())
            .unwrap();
        assert_eq!(out, DeltaDecision::One(em("c1", Some("lookup"), " more")));
        // Buffer already drained by the flush; post-loop flush is a no-op.
        assert_eq!(gate.flush_pending_client_tool("c1"), DeltaDecision::None);
    }

    #[test]
    fn active_gate_flushes_name_only_client_tool_at_turn_end() {
        let mut gate = ToolDeltaGate::new(true);
        // A client tool whose args streamed entirely before its name: deltas
        // buffer and no delta ever carries the name, so nothing is emitted live.
        assert_eq!(
            gate.on_delta("c1".into(), None, "{\"q".into()).unwrap(),
            DeltaDecision::None
        );
        assert_eq!(
            gate.on_delta("c1".into(), None, "\":2}".into()).unwrap(),
            DeltaDecision::None
        );
        // Post-loop flush (engine drives this for each non-ImageAnalysis tool
        // call) moves the buffer out, no trailing delta — replayed in order.
        let decision = gate.flush_pending_client_tool("c1");
        assert_eq!(
            decision,
            DeltaDecision::Flush {
                call_id: "c1".into(),
                buffered: vec![(None, "{\"q".into()), (None, "\":2}".into())],
                trailing: None,
            }
        );
        assert_eq!(
            emissions(decision),
            vec![em("c1", None, "{\"q"), em("c1", None, "\":2}")]
        );
        // Buffer removed: a second flush yields nothing.
        assert_eq!(gate.flush_pending_client_tool("c1"), DeltaDecision::None);
    }

    #[test]
    fn distinct_call_ids_are_gated_independently() {
        let mut gate = ToolDeltaGate::new(true);
        // c1 buffers, c2 resolves immediately to a client tool.
        assert_eq!(
            gate.on_delta("c1".into(), None, "a".into()).unwrap(),
            DeltaDecision::None
        );
        let out = gate
            .on_delta("c2".into(), Some("lookup".into()), "b".into())
            .unwrap();
        assert_eq!(out, DeltaDecision::One(em("c2", Some("lookup"), "b")));
        // c1 then resolves to analyzeImage and is dropped — c2 unaffected.
        assert_eq!(
            gate.on_delta(
                "c1".into(),
                Some(ANALYZE_IMAGE_TOOL_NAME.into()),
                "c".into()
            )
            .unwrap(),
            DeltaDecision::None
        );
        assert_eq!(gate.flush_pending_client_tool("c1"), DeltaDecision::None);
        assert_eq!(gate.flush_pending_client_tool("c2"), DeltaDecision::None);
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
            assert_eq!(
                gate.on_delta(format!("c{i}"), None, chunk.clone()).unwrap(),
                DeltaDecision::None
            );
        }
        // The aggregate is now at the total cap; one more byte overflows.
        assert_eq!(
            gate.on_delta("overflow".into(), None, "z".into()),
            Err(PendingBufferOverflow)
        );
    }

    #[test]
    fn dropping_analyze_image_reclaims_full_total_budget() {
        // Genuinely sensitive to the per-call subtraction in the `Some(true)`
        // arm: fill the total pending budget EXACTLY, then drop one call and
        // prove its bytes were reclaimed by successfully buffering that exact
        // amount again. If reclamation were removed the final buffer would push
        // the aggregate to TOTAL + PER_CALL and overflow instead.
        let mut gate = ToolDeltaGate::new(true);
        let chunk = "y".repeat(MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL);
        let calls = MAX_PENDING_TOOL_DELTA_BYTES_TOTAL / MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL;
        // Fill to EXACTLY the total cap (TOTAL == calls * PER_CALL).
        for i in 0..calls {
            assert_eq!(
                gate.on_delta(format!("c{i}"), None, chunk.clone()).unwrap(),
                DeltaDecision::None
            );
        }
        // Sanity: with the budget full, any further nameless byte overflows.
        assert_eq!(
            gate.on_delta("probe".into(), None, "z".into()),
            Err(PendingBufferOverflow)
        );
        // Resolve call c0 to analyzeImage: its PER_CALL bytes leave the pool.
        assert_eq!(
            gate.on_delta("c0".into(), Some(ANALYZE_IMAGE_TOOL_NAME.into()), "".into())
                .unwrap(),
            DeltaDecision::None
        );
        // Exactly the reclaimed capacity is now free: a fresh call buffering a
        // full PER_CALL chunk must succeed (would overflow without reclaim).
        assert_eq!(
            gate.on_delta("reclaimed".into(), None, chunk.clone())
                .unwrap(),
            DeltaDecision::None
        );
        // And the budget is full again — one more byte overflows.
        assert_eq!(
            gate.on_delta("probe2".into(), None, "z".into()),
            Err(PendingBufferOverflow)
        );
    }

    #[test]
    fn flushing_client_tool_reclaims_its_pending_budget() {
        // The turn-end flush also returns a call's bytes to the pool. Fill the
        // budget to exactly full, flush one client tool, then prove the freed
        // capacity is reusable.
        let mut gate = ToolDeltaGate::new(true);
        let chunk = "y".repeat(MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL);
        let calls = MAX_PENDING_TOOL_DELTA_BYTES_TOTAL / MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL;
        for i in 0..calls {
            assert_eq!(
                gate.on_delta(format!("c{i}"), None, chunk.clone()).unwrap(),
                DeltaDecision::None
            );
        }
        // Flush c0 (a name-only client tool): emits its buffer AND frees it.
        let decision = gate.flush_pending_client_tool("c0");
        assert_eq!(
            decision,
            DeltaDecision::Flush {
                call_id: "c0".into(),
                buffered: vec![(None, chunk.clone())],
                trailing: None,
            }
        );
        // The reclaimed PER_CALL capacity is reusable; without the subtraction
        // this would overflow.
        assert_eq!(
            gate.on_delta("reclaimed".into(), None, chunk).unwrap(),
            DeltaDecision::None
        );
    }
}
