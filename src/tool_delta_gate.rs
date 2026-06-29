//! Streamed tool-call delta gate.
//!
//! An upstream may stream `function_call_arguments` deltas for a tool call BEFORE
//! the function name arrives (sparse chunks). Some of those calls must never reach
//! the client:
//! - a server-side tool the gateway runs internally (`analyzeImage`, G4), and
//! - a HALLUCINATED tool — a name the model emitted that was NOT in the offered
//!   tool set this turn (E1). Both its name and its streamed argument fragments
//!   must stay hidden so the engine can soft-reject + repair without the client
//!   ever seeing the bad call.
//!
//! Because a leading argument fragment can arrive while the name is still `None`,
//! the gate BUFFERS leading deltas per `call_id` until the engine can classify the
//! resolved name, then either DROPS them (hidden tool) or FLUSHES them in order (a
//! client-visible tool, whose later deltas then forward straight through).
//!
//! Classification lives in the engine (it owns the `ToolRegistry`); the gate is a
//! pure decision machine driven by a tri-state `hidden: Option<bool>`:
//! `None` = name not yet resolved (buffer), `Some(true)` = hidden (drop),
//! `Some(false)` = client-visible (flush + forward). It never touches the SSE
//! channel or the monitor hub. Each method returns a [`DeltaDecision`] the engine
//! drives through its single emission path.
//!
//! The decision is allocation-free on the hot path: a resolved, client-visible
//! tool with nothing buffered (the common case — the name arrives with the first
//! delta) returns [`DeltaDecision::One`] with NO map entry, and a flush MOVES the
//! already-allocated pending buffer out of the gate ([`DeltaDecision::Flush`]) so
//! the engine iterates it in place — no fresh per-delta allocation either way.

use std::collections::HashMap;

/// Per-call cap on still-name-unknown buffered tool-call argument bytes (G4
/// round-2 #4 DoS guard). 256 KiB is far above any real leading
/// arguments-before-name fragment (tool names arrive within the first chunk or
/// two) while bounding a single hostile call.
const MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL: usize = 256 * 1024;

/// Total cap across ALL still-pending buffers in one upstream turn (G4 round-2
/// #4). Bounds the aggregate even if an upstream opens many nameless calls.
const MAX_PENDING_TOOL_DELTA_BYTES_TOTAL: usize = 1024 * 1024;

/// Buffered leading `function_call_arguments` deltas for one `call_id` whose tool
/// name is not yet resolved. Once the engine classifies the name, the buffer is
/// either dropped (hidden) or flushed in order (visible).
#[derive(Debug, Default)]
struct PendingDeltas {
    buffered: Vec<(Option<String>, String)>,
    /// Running sum of `buffered`'s delta-payload byte lengths. Maintained O(1) on
    /// every push so the per-call cap check never re-sums the whole buffer
    /// (invariant: `bytes == sum(len(delta) for buffered)`).
    bytes: usize,
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
    /// Drop this delta (hidden tool, or still buffering) — emit nothing.
    None,
    /// Forward exactly one delta (fast path, or an already-resolved visible
    /// tool).
    One(DeltaEmission),
    /// A client-visible tool's name just resolved: emit the buffered leading
    /// fragments in order under `call_id`, then the optional `trailing`
    /// triggering delta (its `(name, delta)` — it shares `call_id`, so the id is
    /// NOT duplicated here; the engine pairs it with `call_id`). `buffered` is the
    /// gate's own moved-out buffer (no copy). Used both for the in-stream resolve
    /// (with `trailing`) and the turn-end flush (without).
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
/// Construct one per upstream turn via [`ToolDeltaGate::new`]. Leading deltas
/// (name not yet resolved) are buffered per `call_id`; once the engine classifies
/// the resolved name the buffer is dropped (hidden tool) or flushed (visible
/// tool). A turn with no tool calls never touches the gate at all.
#[derive(Default)]
pub(crate) struct ToolDeltaGate {
    /// Per-`call_id` buffered leading deltas, populated only while a call's name
    /// is unresolved. A resolved name removes the entry (drop or flush), and a
    /// name-first delta never creates one.
    buffer: HashMap<String, PendingDeltas>,
    /// Total bytes held across all still-pending (name-unknown) buffers this
    /// turn. Bounded so a hostile/buggy upstream streaming endless arguments
    /// before ever sending a tool name cannot grow memory without limit.
    pending_buffer_bytes: usize,
}

impl ToolDeltaGate {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Process one streamed `function_call_arguments` delta, returning the
    /// allocation-free [`DeltaDecision`] the engine should emit. `hidden` is the
    /// engine's classification of the (possibly still-`None`) tool name:
    /// - `None` — name not yet resolved: buffer this leading fragment.
    /// - `Some(true)` — hidden tool (unknown/hallucinated or server-side): drop
    ///   any buffered fragments and this delta. The client never sees it.
    /// - `Some(false)` — client-visible tool: flush any buffered fragments in
    ///   order, then forward this delta.
    pub(crate) fn on_delta(
        &mut self,
        call_id: String,
        name: Option<String>,
        delta: String,
        hidden: Option<bool>,
    ) -> Result<DeltaDecision, PendingBufferOverflow> {
        match hidden {
            // Name not yet resolved: buffer this leading fragment until the engine
            // can classify it (the name arrives in this or a later chunk) or the
            // turn ends. Enforce a per-call AND total pending-byte cap so an
            // upstream that streams endless args-before-name cannot exhaust
            // memory; exceeding it fails the turn cleanly.
            None => {
                let delta_bytes = delta.len();
                let current = self
                    .buffer
                    .get(call_id.as_str())
                    .map(|pending| pending.bytes)
                    .unwrap_or(0);
                if current + delta_bytes > MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL
                    || self.pending_buffer_bytes + delta_bytes > MAX_PENDING_TOOL_DELTA_BYTES_TOTAL
                {
                    return Err(PendingBufferOverflow);
                }
                let pending = self.buffer.entry(call_id).or_default();
                pending.buffered.push((name, delta));
                pending.bytes += delta_bytes;
                self.pending_buffer_bytes += delta_bytes;
                Ok(DeltaDecision::None)
            }
            // Resolved to a HIDDEN tool (unknown/hallucinated OR a server-side
            // tool like `analyzeImage`): discard any buffered leading fragments
            // and drop this delta. Buffered bytes leave the pending pool.
            Some(true) => {
                if let Some(pending) = self.buffer.remove(call_id.as_str()) {
                    self.pending_buffer_bytes =
                        self.pending_buffer_bytes.saturating_sub(pending.bytes);
                }
                Ok(DeltaDecision::None)
            }
            // Resolved to a client-VISIBLE tool: flush any buffered leading
            // fragments in order, then this delta. With nothing buffered (the
            // common case — the name arrived with the first delta) there is no map
            // entry, so collapse to the allocation-free `One`, MOVING `call_id`
            // into the emission.
            Some(false) => {
                if let Some(pending) = self.buffer.remove(call_id.as_str()) {
                    self.pending_buffer_bytes =
                        self.pending_buffer_bytes.saturating_sub(pending.bytes);
                    Ok(DeltaDecision::Flush {
                        call_id,
                        buffered: pending.buffered,
                        // `trailing` shares `call_id` — carries only (name, delta).
                        trailing: Some((name, delta)),
                    })
                } else {
                    Ok(DeltaDecision::One(DeltaEmission {
                        call_id,
                        name,
                        delta,
                    }))
                }
            }
        }
    }

    /// Flush the still-pending buffer for a client-VISIBLE tool whose arguments
    /// streamed entirely before its name (G4 round-2 #5).
    ///
    /// Such a tool's name arrived name-only, so no delta ever triggered the flush
    /// in [`Self::on_delta`]; its leading deltas are still buffered. The engine
    /// calls this for each finalized client-visible tool call (in
    /// `finalized.tool_calls` order) so the client receives all of its tool-arg
    /// deltas, in order, before the public items and the
    /// `function_call_arguments.done`. Returns [`DeltaDecision::None`] if the call
    /// has no pending buffer (already flushed/dropped, or never seen); otherwise a
    /// [`DeltaDecision::Flush`] that MOVES the buffer out (no copy) with no
    /// `trailing` delta.
    ///
    /// The engine does NOT call this for a hidden/hallucinated call (it is never
    /// in `finalized.tool_calls`), so a buffered unknown-tool call whose name
    /// arrived name-only is simply abandoned when the gate drops at turn end —
    /// its leading deltas never reach the client.
    pub(crate) fn flush_pending_client_tool(&mut self, call_id: &str) -> DeltaDecision {
        match self.buffer.remove(call_id) {
            Some(pending) if !pending.buffered.is_empty() => {
                self.pending_buffer_bytes = self.pending_buffer_bytes.saturating_sub(pending.bytes);
                DeltaDecision::Flush {
                    call_id: call_id.to_string(),
                    buffered: pending.buffered,
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
    /// stay expressed as the exact wire sequence regardless of the
    /// `None`/`One`/`Flush` shape.
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
    fn visible_tool_named_on_first_delta_forwards_without_buffering() {
        let mut gate = ToolDeltaGate::new();
        // The common case: the name arrives with the first delta and is visible.
        // The fast path returns a bare `One` (no Vec, no map entry).
        let out = gate
            .on_delta("c1".into(), Some("lookup".into()), "{".into(), Some(false))
            .unwrap();
        assert_eq!(out, DeltaDecision::One(em("c1", Some("lookup"), "{")));
        let out = gate
            .on_delta("c1".into(), Some("lookup".into()), "}".into(), Some(false))
            .unwrap();
        assert_eq!(out, DeltaDecision::One(em("c1", Some("lookup"), "}")));
        // Nothing buffered, so the post-loop flush is a no-op.
        assert_eq!(gate.flush_pending_client_tool("c1"), DeltaDecision::None);
    }

    #[test]
    fn hidden_tool_dropped_once_name_resolves() {
        let mut gate = ToolDeltaGate::new();
        // Leading fragment arrives before the name: buffered, nothing emitted.
        assert_eq!(
            gate.on_delta("c1".into(), None, "{\"ima".into(), None)
                .unwrap(),
            DeltaDecision::None
        );
        // Name resolves to a hidden tool (server-side analyzeImage, or an unknown
        // hallucinated name): buffered leading fragment discarded, this delta
        // dropped.
        assert_eq!(
            gate.on_delta(
                "c1".into(),
                Some("analyzeImage".into()),
                "ge".into(),
                Some(true)
            )
            .unwrap(),
            DeltaDecision::None
        );
        // Subsequent deltas stay dropped (the name stays resolved hidden).
        assert_eq!(
            gate.on_delta(
                "c1".into(),
                Some("analyzeImage".into()),
                "rest".into(),
                Some(true)
            )
            .unwrap(),
            DeltaDecision::None
        );
        // Buffer was consumed, so the post-loop flush emits nothing.
        assert_eq!(gate.flush_pending_client_tool("c1"), DeltaDecision::None);
    }

    #[test]
    fn hidden_tool_named_on_first_delta_drops_without_buffering() {
        let mut gate = ToolDeltaGate::new();
        // Hidden name known from the first delta: dropped with no buffering.
        assert_eq!(
            gate.on_delta("c1".into(), Some("Grep".into()), "{}".into(), Some(true))
                .unwrap(),
            DeltaDecision::None
        );
        assert_eq!(gate.flush_pending_client_tool("c1"), DeltaDecision::None);
    }

    #[test]
    fn visible_tool_flushes_buffered_deltas_in_order_then_forwards() {
        let mut gate = ToolDeltaGate::new();
        // Two leading fragments buffered before the name is known.
        assert_eq!(
            gate.on_delta("c1".into(), None, "{\"a".into(), None)
                .unwrap(),
            DeltaDecision::None
        );
        assert_eq!(
            gate.on_delta("c1".into(), None, "\":1".into(), None)
                .unwrap(),
            DeltaDecision::None
        );
        // Name resolves to a visible tool: a single `Flush` carries the moved
        // buffer + the triggering delta. Emit order: buffered IN ORDER, then the
        // trigger.
        let decision = gate
            .on_delta("c1".into(), Some("lookup".into()), "}".into(), Some(false))
            .unwrap();
        assert_eq!(
            decision,
            DeltaDecision::Flush {
                call_id: "c1".into(),
                buffered: vec![(None, "{\"a".into()), (None, "\":1".into())],
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
        // Now resolved: later deltas forward straight through as a bare `One`.
        let out = gate
            .on_delta(
                "c1".into(),
                Some("lookup".into()),
                " more".into(),
                Some(false),
            )
            .unwrap();
        assert_eq!(out, DeltaDecision::One(em("c1", Some("lookup"), " more")));
        // Buffer already drained by the flush; post-loop flush is a no-op.
        assert_eq!(gate.flush_pending_client_tool("c1"), DeltaDecision::None);
    }

    #[test]
    fn visible_name_only_tool_flushed_at_turn_end() {
        let mut gate = ToolDeltaGate::new();
        // A visible tool whose args streamed entirely before its name: deltas
        // buffer and no delta ever carries the name, so nothing is emitted live.
        assert_eq!(
            gate.on_delta("c1".into(), None, "{\"q".into(), None)
                .unwrap(),
            DeltaDecision::None
        );
        assert_eq!(
            gate.on_delta("c1".into(), None, "\":2}".into(), None)
                .unwrap(),
            DeltaDecision::None
        );
        // Post-loop flush (engine drives this for each visible tool call) moves the
        // buffer out, no trailing delta — replayed in order.
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
    fn name_only_call_buffer_is_only_released_by_an_explicit_flush() {
        // A call whose args streamed entirely before its name leaves its leading
        // deltas buffered (no `on_delta` ever resolved them). NOTHING was emitted
        // live for it. Whether those buffered deltas reach the client is therefore
        // the ENGINE's choice: it calls `flush_pending_client_tool` ONLY for
        // visible `finalized.tool_calls`, and SKIPS it for a rejected/hallucinated
        // call (and for the whole batch when it is tainted) — so a hidden
        // name-only call's buffer is simply abandoned and never streamed.
        let mut gate = ToolDeltaGate::new();
        assert_eq!(
            gate.on_delta("c1".into(), None, "{\"path".into(), None)
                .unwrap(),
            DeltaDecision::None
        );
        assert_eq!(
            gate.on_delta("c1".into(), None, "\":\"x\"}".into(), None)
                .unwrap(),
            DeltaDecision::None
        );
        // No live emission happened; the buffer is only released if the engine
        // explicitly flushes (which it does NOT for a hidden/rejected call).
        let decision = gate.flush_pending_client_tool("c1");
        assert!(matches!(decision, DeltaDecision::Flush { .. }));
    }

    #[test]
    fn distinct_call_ids_are_gated_independently() {
        let mut gate = ToolDeltaGate::new();
        // c1 buffers, c2 resolves immediately to a visible tool.
        assert_eq!(
            gate.on_delta("c1".into(), None, "a".into(), None).unwrap(),
            DeltaDecision::None
        );
        let out = gate
            .on_delta("c2".into(), Some("lookup".into()), "b".into(), Some(false))
            .unwrap();
        assert_eq!(out, DeltaDecision::One(em("c2", Some("lookup"), "b")));
        // c1 then resolves to a hidden tool and is dropped — c2 unaffected.
        assert_eq!(
            gate.on_delta("c1".into(), Some("Grep".into()), "c".into(), Some(true))
                .unwrap(),
            DeltaDecision::None
        );
        assert_eq!(gate.flush_pending_client_tool("c1"), DeltaDecision::None);
        assert_eq!(gate.flush_pending_client_tool("c2"), DeltaDecision::None);
    }

    #[test]
    fn per_call_pending_byte_cap_overflows() {
        let mut gate = ToolDeltaGate::new();
        // One nameless delta just over the per-call cap fails the turn.
        let huge = "x".repeat(MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL + 1);
        assert_eq!(
            gate.on_delta("c1".into(), None, huge, None),
            Err(PendingBufferOverflow)
        );
    }

    #[test]
    fn total_pending_byte_cap_overflows_across_calls() {
        let mut gate = ToolDeltaGate::new();
        // Fill close to the total cap across several calls, each under the
        // per-call cap, then tip over the aggregate limit.
        let chunk = "y".repeat(MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL);
        let calls = MAX_PENDING_TOOL_DELTA_BYTES_TOTAL / MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL;
        for i in 0..calls {
            assert_eq!(
                gate.on_delta(format!("c{i}"), None, chunk.clone(), None)
                    .unwrap(),
                DeltaDecision::None
            );
        }
        // The aggregate is now at the total cap; one more byte overflows.
        assert_eq!(
            gate.on_delta("overflow".into(), None, "z".into(), None),
            Err(PendingBufferOverflow)
        );
    }

    #[test]
    fn dropping_hidden_tool_reclaims_full_total_budget() {
        // Genuinely sensitive to the per-call subtraction in the `Some(true)`
        // arm: fill the total pending budget EXACTLY, then drop one call and
        // prove its bytes were reclaimed by successfully buffering that exact
        // amount again. If reclamation were removed the final buffer would push
        // the aggregate to TOTAL + PER_CALL and overflow instead.
        let mut gate = ToolDeltaGate::new();
        let chunk = "y".repeat(MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL);
        let calls = MAX_PENDING_TOOL_DELTA_BYTES_TOTAL / MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL;
        for i in 0..calls {
            assert_eq!(
                gate.on_delta(format!("c{i}"), None, chunk.clone(), None)
                    .unwrap(),
                DeltaDecision::None
            );
        }
        // Sanity: with the budget full, any further nameless byte overflows.
        assert_eq!(
            gate.on_delta("probe".into(), None, "z".into(), None),
            Err(PendingBufferOverflow)
        );
        // Resolve call c0 to a hidden tool: its PER_CALL bytes leave the pool.
        assert_eq!(
            gate.on_delta("c0".into(), Some("Grep".into()), "".into(), Some(true))
                .unwrap(),
            DeltaDecision::None
        );
        // Exactly the reclaimed capacity is now free: a fresh call buffering a
        // full PER_CALL chunk must succeed (would overflow without reclaim).
        assert_eq!(
            gate.on_delta("reclaimed".into(), None, chunk.clone(), None)
                .unwrap(),
            DeltaDecision::None
        );
        // And the budget is full again — one more byte overflows.
        assert_eq!(
            gate.on_delta("probe2".into(), None, "z".into(), None),
            Err(PendingBufferOverflow)
        );
    }

    #[test]
    fn flushing_visible_tool_reclaims_its_pending_budget() {
        // The turn-end flush also returns a call's bytes to the pool. Fill the
        // budget to exactly full, flush one visible tool, then prove the freed
        // capacity is reusable.
        let mut gate = ToolDeltaGate::new();
        let chunk = "y".repeat(MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL);
        let calls = MAX_PENDING_TOOL_DELTA_BYTES_TOTAL / MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL;
        for i in 0..calls {
            assert_eq!(
                gate.on_delta(format!("c{i}"), None, chunk.clone(), None)
                    .unwrap(),
                DeltaDecision::None
            );
        }
        // Flush c0 (a name-only visible tool): emits its buffer AND frees it.
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
            gate.on_delta("reclaimed".into(), None, chunk, None)
                .unwrap(),
            DeltaDecision::None
        );
    }

    #[test]
    fn per_call_cap_accumulates_across_many_small_deltas() {
        // Exercises the running-`bytes` accumulation path (vs. the single-huge
        // delta in `per_call_pending_byte_cap_overflows`): MANY 1-byte nameless
        // deltas fill EXACTLY to the per-call cap, all returning `None`, and the
        // very next 1-byte delta overflows at the identical boundary.
        let mut gate = ToolDeltaGate::new();
        for _ in 0..MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL {
            assert_eq!(
                gate.on_delta("c1".into(), None, "x".into(), None).unwrap(),
                DeltaDecision::None
            );
        }
        // Buffer now holds exactly the per-call cap; one more byte trips it.
        assert_eq!(
            gate.on_delta("c1".into(), None, "x".into(), None),
            Err(PendingBufferOverflow)
        );
    }

    #[test]
    fn empty_deltas_add_zero_to_running_per_call_bytes() {
        // Empty-string nameless deltas push a zero-byte fragment and add 0 to the
        // running `bytes`, so they are no-ops against the per-call cap: after an
        // arbitrary number of them a full PER_CALL-byte chunk still buffers.
        let mut gate = ToolDeltaGate::new();
        for _ in 0..1000 {
            assert_eq!(
                gate.on_delta("c1".into(), None, "".into(), None).unwrap(),
                DeltaDecision::None
            );
        }
        // A full per-call chunk still fits (zero-byte fragments moved nothing).
        let chunk = "y".repeat(MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL);
        assert_eq!(
            gate.on_delta("c1".into(), None, chunk, None).unwrap(),
            DeltaDecision::None
        );
        // And the call is now exactly at the per-call cap — one more byte trips.
        assert_eq!(
            gate.on_delta("c1".into(), None, "z".into(), None),
            Err(PendingBufferOverflow)
        );
    }
}
