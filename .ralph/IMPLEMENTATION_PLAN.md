# IMPLEMENTATION_PLAN.md — llmconduit gap implementation

Implements the 7 core gaps in `GAPS.md`, plus the owner-directed EXTENDED RUN of the originally-deferred
G4 (image agent) + G7 (route config) + the descoped G3 keep-alive-peek. Specs: `.ralph/specs/*`
(historical design inputs — see "Spec status" below). Review gate: `.ralph/REVIEW_PROTOCOL.md`.

## Executive summary
**Status: core 7/7 ✅ + EXTENDED RUN COMPLETE ✅ (G3-peek, G7, G4 — owner-directed, all Codex-xhigh APPROVED). ALL 9 GAPS + P1 + G3-peek DONE, plus a post-run `reasoning_effort_map` rework (leaf-applied, reserved-key deleted), plus a per-gap thermo-nuclear code-quality review (10 gaps reviewed; bounded fixes in `07117b2`; 11 deferred follow-ups tracked as Topic 11 on branch `ralph/thermo-followups`).** Topic 11 is now COMPLETE (11/11 Codex-xhigh APPROVED). **Topic 12 added 2026-06-20** — a whole-codebase thermo-nuclear PROJECT review (16 parallel subsystem reviewers → Codex-xhigh adversarial verify) surfaced 11 VERIFIED findings (2 HIGH, 5 MEDIUM, 4 REVISED-LOW; 0 refuted) → **10 tasks (12.1–12.10, ⬜ PENDING)**; specs `.ralph/specs/U1..U10`, full report `/tmp/thermo-project-review.md`. Loop validated: build → cargo test/clippy/fmt → Codex-xhigh review → fix → re-review APPROVED.

## Spec status
`.ralph/specs/*.md` are **historical design inputs** written before implementation; their `OPEN QUESTION` /
`VERIFY FIRST` notes and unchecked acceptance boxes are resolved by the shipped code + tests below. They
are retained for provenance, not as live design sources. Final design lives in this file + the code.

## Working agreement (historical — the run is complete)
1. Study the gap's `.ralph/specs/<ID>.md` AND the referenced claude-relay impl in
   `/home/jon/git/claude-relay/claude_relay/` — adapt to Rust + canonical-Responses, do NOT transliterate.
2. Confirm with code search before assuming something is missing (some gaps are PARTIAL).
3. Obey AGENTS.md "Hard rules in the engine" — they are load-bearing.
4. Definition of Done per task = executable test green · `cargo test` whole suite green · `cargo clippy
   --all-targets` clean · `cargo fmt` · **Codex-xhigh review APPROVED** (`.ralph/REVIEW_PROTOCOL.md`) · commit.
5. Record discoveries/decisions back here; keep AGENTS.md operational-only.

---

## Implementation records

### Task 1 — P1 · effort → reasoning.effort · `1faba60` · Codex-xhigh APPROVED (R2)
Map Anthropic `output_config.effort` (adaptive thinking) onto canonical `reasoning.effort`; effort
strings pass through RAW (trimmed/lowercased) so the leaf can map/clamp per model. Tests: `tests/port_translation.rs`.

### Task 2 — G5 · debug-dump log rotation (mode-aware dirs) · `b610a53` · Codex-xhigh APPROVED (R2)
New `src/log_rotation.rs`; age-based cleanup of `upstream_request_log_path` dumps. Tests: `tests/port_logging.rs`.

### Task 3 — G1 · context-window-limit retry (regex classifier) · `ebe6b41` · Codex-xhigh APPROVED (R7)
Regex classifier over upstream 4xx/5xx bodies → retry once with reduced `max_completion_tokens`
(honoring a `min_completion_tokens` floor). 4 shape regexes, each with its distinctive leading literal
to avoid over-matching. Tests: `tests/port_errors.rs`.

### Task 4 — G3 · pre-flight context budgeting cap · `41d7428` · Codex-xhigh APPROVED (R6)
**Final design (terminal-layer estimate).** `lower_request` (already pre-spawn; `?` surfaces
validation/lowering errors before budgeting, so no new error path) produces `LoweredTurn`.
`estimate_input_tokens(&lowered, flatten_content)` builds the chat request from the lowered fields and
runs the SAME `sanitize_chat_request` the leaf runs (`engine::estimate_request_from_lowered`), then
`ceil(bytes/4)` of the post-sanitize lowered body (NOT the full wire body — additive leaf merges are
omitted; see below). Cap an EXPLICITLY-requested `max_output_tokens` to
`min(requested, context − est − 128)`; fixed 128 margin; `est+margin ≥ context` ⇒ HTTP 400 before any
`tokio::spawn`/upstream POST. **No floor, no synthesized cap, never raises; mutates only the typed field.**

**Why it stays safe (terminal layer):** counting the post-`sanitize_chat_request` body means nothing
transforms it further, so an over-count is impossible. Omissions are the ADDITIVE leaf merges
(`extra_body`/`upstream_chat_kwargs`, G2 family `chat_template_kwargs`, sampling scalars) AND the
`reasoning_effort` field (the leaf may clear/map it via the per-model `reasoning_effort_map`); all only
GROW the real payload or are omitted, so the estimate stays a safe lower bound → never a false 400.
G3 stays OUT of the kwargs-merge seam. Covers the FIRST upstream turn; later tool-loop turns rely on G1.

**Rejected earlier approach (do not revive):** an earlier attempt estimated an earlier representation
and thrashed across review rounds (raw `ToolSpec` vs `lower_tools`, dropped `ImageGenerationCall`,
`text.verbosity`, `reasoning.summary`, leaf content-flatten). Lesson: estimate at the TERMINAL layer.

**Context-length source:** parsed from upstream `/v1/models` entries (same keys as the Anthropic
`/v1/models` reshape: `max_input_tokens`, `context_length`, `context_window`, `max_context_length`,
`max_model_len`), cached in `UpstreamModelCatalog` (`context_limit_by_id`), budgeting gated on the limit
being known (no-op when unknown). Single `/v1/models` snapshot via `UpstreamClient::supported_model_catalog`.
Tests: `tests/port_server.rs` (8, oracle reuses `estimate_request_from_lowered`) + `src/upstream.rs` units.

### Task 5 — G2 · Kimi/DeepSeek family kwargs + per-model effort map · `d1e626e` (R3) + post-run `b6afa08`/`1a1797d`/`ee5fabc`
**Final design (leaf-applied).** Family `chat_template_kwargs` injected at the provider LEAF (after model
rewrite); `template_family` override via profile chain. Chat reasoning suppression is family-independent.
**Post-run rework:** per-model `reasoning_effort_map` (canonical level → request fragment) applied at the
leaf via `upstream::finalize_request_for_backend`, so a backend with its own effort vocabulary (e.g. GLM-5.2:
reads `chat_template_kwargs`, recognizes only "high" else "max", off via `enable_thinking:false`) receives
the right knob. Lowering passes the RAW canonical effort; the leaf maps it or clamps to {none,low,high}.
The earlier reserved-key magic used to thread effort engine→leaf was DELETED in favor of the existing
typed `reasoning_effort` field. Precedence: config < family < effort-map < client.

### Task 6 — G8 · reasoning promotion/suppression (Anthropic) · `8297ca6` · Codex-xhigh APPROVED (R3)
Reasoning buffered; promote only on clean `response.completed`; signature/length/incomplete → thinking;
late reasoning dropped; web_search surfaced via additive event. Tests: `tests/port_streaming_peek.rs` + `tests/port_translation.rs`.

### Task 7 — G6 · SSE per-frame buffer cap (DoS guard) · `881cfe1` · Codex-xhigh APPROVED
`eventsource-stream` 0.2.3 accumulated upstream SSE bytes unbounded; added a per-frame byte cap
(`SseFrameGuard`, EOL-grammar-correct, EOF-finalized, `max_sse_frame_bytes` default 8 MiB). Reference-oracle
differential test in `tests/port_streaming_peek.rs`.

### Task 8 — G3 keep-alive peek · `50720eb` · Codex-xhigh APPROVED (R4)
Found redundant with G8 + axum's streaming; contracted via mutation-verified tests (no new code).

### Task 9 — G7 · glob routes + `--model-route` CLI + TOML config · `5dceac6` · Codex-xhigh APPROVED (R5)
Glob route keys (declaration order = match order), `--model-route NAME=URL[,UPSTREAM_MODEL]` CLI flag
(malformed = clean startup `Err`), TOML config with identical YAML semantics. Precedence:
exact id > exact route > glob route > canonical key > default. Tests: `tests/port_config.rs`.

### Task 10 — G4 · image agent (vision offload) · `0a5ba94` · Codex-xhigh APPROVED (R10)
`VisionClient` seam (`src/vision.rs`), strip/cache images to `[Image #N]` placeholders, server-tool
dispatcher, per-session LRU+TTL `ImageCache`, gating. Tests: `tests/gateway.rs` image-agent suite.

---

## Completed tasks
| Task | Gap | Commit | Review |
|-|-|-|-|
| 1 | P1 output_config.effort → reasoning.effort | `1faba60` | Codex-xhigh APPROVED (R2) |
| 2 | G5 debug-dump log rotation (mode-aware dirs) | `b610a53` | Codex-xhigh APPROVED (R2) |
| 3 | G1 context-window-limit retry (regex classifier) | `ebe6b41` | Codex-xhigh APPROVED (R7) |
| 4 | G3 pre-flight context budgeting cap (terminal-layer estimate) | `41d7428` | Codex-xhigh APPROVED (R6) |
| 5 | G2 Kimi/DeepSeek family kwargs + per-model effort map | `d1e626e` + `b6afa08`/`1a1797d`/`ee5fabc` | Codex-xhigh APPROVED (R3 + rework) |
| 6 | G8 reasoning promotion/suppression (Anthropic) | `8297ca6` | Codex-xhigh APPROVED (R3) |
| 7 | G6 SSE per-frame buffer cap (DoS guard) | `881cfe1` | Codex-xhigh APPROVED |
| 8 | G3 keep-alive peek (redundant w/ G8+axum; tests) | `50720eb` | Codex-xhigh APPROVED (R4) |
| 9 | G7 glob routes + `--model-route` CLI + TOML config | `5dceac6` | Codex-xhigh APPROVED (R5) |
| 10 | G4 image agent (vision offload) | `0a5ba94` | Codex-xhigh APPROVED (R10) |

## Discoveries (lessons — read before related work)
- **Effort normalization is single-sourced at the leaf.** Lowering (`responses_to_chat::normalize_reasoning_effort`)
  passes the raw canonical level through (trim+lowercase); the upstream leaf
  (`upstream::finalize_request_for_backend`) maps it per-model (`reasoning_effort_map`) or clamps to
  {none,low,high}. Earlier, the clamp lived in lowering AND a reserved-key marker threaded the raw value
  engine→leaf — that "magic" was DELETED in favor of the existing typed `reasoning_effort` field (a code-judo
  move: the spoof + debug-leak surfaces it caused vanish by construction). **Relevant to:** any future
  effort/thinking handling.
- **Context-overflow classifier (G1) is regex-based** in `upstream.rs::classify_context_overflow`.
  Each of the 4 shape regexes MUST carry its shape's DISTINCTIVE leading literal; matching on generic
  anchors overmatches unrelated 4xx bodies. G1 extracts limits REACTIVELY. **Relevant to G3** (proactive
  complement; reuses `min_completion_tokens`).
- **G3 pre-flight estimate: count the bytes the LEAF POSTs (post-`sanitize_chat_request`), not any earlier
  representation.** Estimating earlier representations is whack-a-mole — every layer between the estimate
  and the socket can reopen the divergence. Build a `ChatCompletionRequest` from the lowered fields and run
  the SAME `sanitize_chat_request` (`engine::estimate_request_from_lowered`), then `ceil(bytes/4)`. Omit the
  ADDITIVE leaf merges AND `reasoning_effort` (which the leaf may clear/map) — all only shrink or are
  additive, so the estimate stays a safe lower bound. Keep G3 OUT of the kwargs-merge seam.

---

## Topic 11 — Thermo-nuclear code-quality follow-ups

> **Source:** `/ralph-guide-update` on 2026-06-19, from the per-gap thermo-nuclear review
> (`/tmp/thermo-synthesis.md`, raw verdicts `/tmp/thermo-gap-review.md`).
> Bounded fixes already shipped in `07117b2`; these are the DEFERRED items, grouped into 11 specs.
> Branch: `ralph/thermo-followups`. Review gate: `.ralph/REVIEW_PROTOCOL.md` (Codex-xhigh) per task.
> **Sequencing:** T1 → (T2, T9); T7 → T8; T5 ↔ T6 coordinate; T10, T11 independent. T1 first (it
> builds the typed resolver T2/T9 consume).

## STATUS (✅ COMPLETE — all 11 APPROVED — orchestrator resume session `thermo-followups-resume`)

**DONE (Codex-xhigh APPROVED + committed):** T1, T2, T7, T8, T9, T6, T5, T3, T4, T10, T11 (11 of 11 ✅).
Topic 11 thermo-nuclear follow-ups COMPLETE. No deferred/halted items; every task converged to a clean
Codex-xhigh APPROVED.
**Review log:** `/tmp/thermo-followup-review.md` holds 11 verdicts (T1×2, T2×3, T7×2, T8×1, T9×4, T6×2, T5×2, T3×4, T4×2, T10×2, T11×2).
**Per-task loop** = implement → fmt/test/clippy → commit → Codex-xhigh review → fix/re-review ≤3
rounds → append verdict to `/tmp/thermo-followup-review.md` → update this plan. STOP when all 11
APPROVED (see `.ralph/GOAL.md`).

### Task 11.1 — Leaf-side profile resolution (template_family + upstream_chat_kwargs)
**Priority:** HIGH · **Spec:** `.ralph/specs/T1-leaf-profile-resolution.md` · **Commit:** `cdb293d`
**Status:** ✅ Codex-xhigh APPROVED (R2) + live-verified on :5022. implemented; Codex-xhigh R1 found 4 findings — F1 (HIGH, case-sensitive policy
lookup) + F2 (MEDIUM, max-token alias shadowing) + F4 (LOW, wrapper visibility doc) fixed in a
follow-up commit; F3 (MEDIUM, single-resolver dedup) split to T2 (see below).
**Final design:** `template_family` + `upstream_chat_kwargs` profile resolution moved from the
engine (pre-routing) to `upstream::finalize_request_for_backend`, mirroring
`reasoning_effort_policies`. New `BackendChatRequest` wrapper (carries `client_chat_template_kwargs`
— the one value not re-derivable at the leaf) + `BackendFinalizationPolicies` (effort + family +
kwargs, global + per-model, built once via `from_config`). `ChatCompletionRequest` no longer
carries `#[serde(skip)]` side-channel fields. The `UpstreamClient::stream_chat_completion` trait
method takes `&BackendChatRequest`; dispatch (`request_for_provider`, `routed_request`, failover/
routing) threads the wrapper. Per-model policy lookup uses `policy_for_model` (exact then
canonical-key-unique, matching `Config::model_profile`). `merge_upstream_chat_kwargs` preserves
max-token alias shadowing. `config::route_matches` is the shared route-match primitive
(`matches_model_route` is a thin caller). Provider-vs-profile kwargs precedence preserved (provider
kwargs merge in `request_for_provider` request-wins; the leaf gap-fills profile+global with the
same semantics). G3 estimate unchanged. **Touches the effort leaf → live-verify `claude --effort
high/max/off` on :5022.**
**Files:** `src/config.rs`, `src/engine.rs`, `src/upstream.rs`, `src/models/chat.rs`, tests.
**Blocks:** 11.2, 11.9.
**Deferred to T2:** the full model-precedence ladder dedup. `normalize_upstream_model` (engine)
still re-derives the 5-step ladder against its own `UpstreamModelCatalog` (which also carries G3
context limits) rather than delegating to `RoutingModelCatalog::resolve`. T2 deletes
`request_model_genuinely_resolves` and returns a typed backend-candidate plan from the real
routing layer, which collapses the ladder as part of its scope. T1 extracted only the shared
`route_matches` boolean primitive. The spec acceptance criterion "single typed resolver" is
co-owned with T2 by this deferral.

### Task 11.2 — Typed routing-candidate plan (delete G4 side-channel vision gating)
**Priority:** HIGH · **Spec:** `.ralph/specs/T2-routing-candidate-plan.md` · **Commit:** `f56fbe9`
**Status:** Codex-xhigh APPROVED (R3). Deleted `request_model_genuinely_resolves` +
side-channel gating resolution. `upstream::BackendCandidatePlan { candidates }` is the
single source of truth for the candidate set; `UpstreamClient::backend_candidate_plan`
builds it (routing: from `catalog.resolve`; failover: per-provider effective models;
default: passthrough), and `candidate_backend_models` default-projects from it (one
method per client, no duplicated enumeration). The `genuine` signal is a byproduct of
the ONE `normalize_upstream_model` walk (now returns `(String, bool)`), threaded
`stream_responses` → `activate_image_agent` → `backend_is_native_vision` — NOT a
re-derived side-channel. `genuine` is false ONLY on a real default-fallback (blank OR
non-blank collapsing to a differing catalog default); true for exact/route/canonical/
no-default-passthrough/catalog-unavailable. G4 decision-table semantics + PROFILE-ONLY
lookup preserved. Round-8 #1 covered by `gating_table_unmatched_request_override_does_
not_apply_to_default` (stale alias) + `gating_table_blank_request_override_does_not_
apply_to_default` (blank model, R1 regression guard). `resolve_request_model` →
`(String, bool)`; 3 http.rs label callers take `.0`. Mock upstream overrides
`backend_candidate_plan`. **Deferred to T9:** the `normalize_upstream_model` ladder
DEDUP vs `RoutingModelCatalog::resolve` — `UpstreamModelCatalog` carries G3
`context_limit_by_id`; T9 moves G3 budgeting behind route/provider resolution, at which
point this fn delegates to the routing catalog and the ladder collapses. T2 collapsed
the gating side-channel only.
**Files:** `src/engine.rs`, `src/upstream.rs`, `src/http.rs`, `tests/gateway.rs`.
**Depends on:** 11.1.

### Task 11.3 — Extract ToolDeltaGate from run_turn
**Priority:** HIGH · **Spec:** `.ralph/specs/T3-tooldeltagate-extraction.md` · **Commits:** `39dad35` → `592631c` → `857efb6` → `b03118e`
**Status:** ✅ Codex-xhigh APPROVED (R4). New `src/tool_delta_gate.rs` (`ToolDeltaGate`, pure decision
machine, no tx/async/monitor deps) owns the `analyzeImage` delta-buffer state (Pending/Drop/Emit, per-call
+ total DoS caps, budget reclaim); engine drives it via one `drive_delta_decision`. Consolidated 5 literal
duplicate emit sites → 1 `Gateway::emit_function_call_delta`. `engine.rs` −134, `run_turn` −136 (~14%); 10
new gate unit tests. R1 (Vec-per-delta alloc MEDIUM + weak reclaim test LOW) → R2 (`DeltaDecision`
None/One/Flush, flush MOVES the buffer; real reclaim test) → R3 (String-clone MEDIUM → borrowed lookup,
moves) → R4 (last double-alloc LOW → `id.as_deref()`). Behavior byte-identical, emit order unchanged,
gateway image-agent 39/39 + all suites green throughout.
**Files:** `src/engine.rs`, `src/tool_delta_gate.rs` (new), `src/lib.rs`.

### Task 11.4 — Split vision.rs + image-agent test suite
**Priority:** MEDIUM · **Spec:** `.ralph/specs/T4-vision-module-split.md` · **Commits:** `1994993` + `b52659c`
**Status:** ✅ Codex-xhigh APPROVED (R2). `src/vision.rs` (1,364) → `src/vision/{mod 53, cache 309,
strip 566, client 291}.rs` + new top-level `src/redaction.rs` (287, `pub(crate)`, re-exported from
`vision/mod.rs` so `crate::vision::redact_*` still resolves). Image-agent suite (47 tests) +
`MockVisionClient` → new `tests/image_agent.rs` (2,277); `tests/gateway.rs` −2,364; shared helpers →
`tests/common/mod.rs`. 34 `vision::*` call sites resolve, no public API change. R1 found 1× MEDIUM (the
moved `test_gateway_with_vision` had ADDED a `set_finalization_policies` call absent from the pre-T4
original — a non-pure-move behavior change); fixed in `b52659c` by plain removal (root-caused: both mocks
default finalization to EMPTY, so parity restored). Other moved-helper changes confirmed behavior-neutral.
**Files:** `src/vision/{mod,cache,strip,client}.rs`, `src/redaction.rs` (new), `src/lib.rs`, `tests/gateway.rs`, `tests/image_agent.rs` (new), `tests/common/mod.rs`.

### Task 11.5 — Bytes-specialized SSE guard (cap before copy)
**Priority:** HIGH · **Spec:** `.ralph/specs/T5-sse-guard-bytes.md` · **Commits:** `b8db7f0` + `f223927`
**Status:** ✅ Codex-xhigh APPROVED (R2). `bounded_sse_byte_stream` specialized to `Bytes`; the guard
caps the BORROWED chunk and forwards the same ref-counted `Bytes` (no `copy_from_slice`); scanner reads
logical `carry ++ chunk` via a private `ByteSource` trait + `JoinedBuf` view, materializing only the
≤3-byte carry. Removes the O(chunk) pre-rejection allocation. R1 found 1× MEDIUM — the memory-bound
regression *test* didn't actually guard the reject path; fixed in `f223927` with a `#[cfg(test)]`
thread-local-armed counting `#[global_allocator]` probe (catches any ≥64 KiB alloc in a guarded region)
+ a bounded-stream same-allocation (`as_ptr` eq) test; sensitivity proven by reintroducing each old
pattern. Production behavior unchanged; allocator confined to the lib test binary (zero release cost).
**Files:** `src/sse_guard/{mod,tests}.rs`.

### Task 11.6 — Extract SSE guard module + shrink port_streaming.rs
**Priority:** MEDIUM · **Spec:** `.ralph/specs/T6-sse-guard-extract.md` · **Commits:** `83b9be1` + `0bae3ac`
**Status:** ✅ Codex-xhigh APPROVED (R2). Extracted the SSE grammar state machine + `SseFrameGuard`
(now `pub(crate)`) into `src/sse_guard/{mod,tests}.rs`; 29 guard tests relocated as module unit tests
(0 dropped); `src/upstream.rs` 5003→4199, `tests/port_streaming.rs` 1436→180 (acceptance-only), the
`DEFAULT_MAX_SSE_FRAME_BYTES` single-source preserved. R1 found 3× LOW (dead_code accessor →
`#[cfg(test)]`; 2082-line file → split `mod.rs` 562 / `tests.rs` 1522; "Codex round" archaeology
removed); all fixed in `0bae3ac`. `max_frame_bytes()` is `#[cfg(test)]` — **T5 drops that cfg** once
production reads the floor. Zero guard behavior change (verbatim move, Codex-verified via `diff -u`).
**Files:** `src/upstream.rs`, `src/sse_guard/{mod,tests}.rs` (new), `src/config.rs`, `tests/port_streaming.rs`.

### Task 11.7 — Typed terminal reason in the canonical response
**Priority:** MEDIUM · **Spec:** `.ralph/specs/T7-typed-terminal-reason.md` · **Commit:** `1b98467`
**Status:** Codex-xhigh APPROVED (R2). `TerminalReason` enum (Stop/Length/ToolCall/ContentFilter/Other;
`ToolCall` serde-renamed to `tool_calls`) carried on `ResponseResource.terminal_reason`; engine sets it
from `last_finish_reason` via `from_finish_reason`; `is_incomplete` derived from `reason == Length`.
`flush_reasoning_terminal` gates promotion on `clean_stop` (`reason.is_clean_stop()`, Stop only), not
`event_type == "response.completed"`. `response_terminal_reason` maps present-but-unrecognized → `Other`
(non-clean); the event-type fallback fires only when the field is absent. Regression tests:
`reasoning_only_at_content_filter_stays_thinking`, `reasoning_only_at_tool_calls_stays_thinking` (R1 —
proves the `tool_calls` wire spelling parses + gates non-clean). G8 behavior preserved; `finalize()`
synthetic emission unchanged.
**Files:** `src/models/responses.rs`, `src/engine.rs`, `src/adapters/responses_to_anthropic.rs`,
`tests/port_response_translation.rs`.
**Blocks:** 11.8.

### Task 11.8 — Extract ReasoningEgressState from responses_to_anthropic
**Priority:** MEDIUM · **Spec:** `.ralph/specs/T8-reasoning-egress-state.md` · **Commit:** `702f4fd`
**Status:** Codex-xhigh APPROVED (R1). Pure structural extraction (no behavior change). `ReasoningEgressState`
(reasoning.rs, 96 lines) owns `reasoning_buffer`/`reasoning_signature`/`content_started`/`has_tool_calls` +
the promote/hold/late-drop decision matrix (`should_promote`, `is_late_reasoning`, `note_content_started`,
`note_tool_calls`, `push_reasoning`, `push_signature`, `take_buffer`, `take_signature`, `has_buffered`); the
converter holds one + delegates. Block emission stays on the converter. Module split:
`responses_to_anthropic/{mod.rs(826), collector.rs(226), reasoning.rs(96), tests.rs(991)}` — all under 1k.
`AnthropicStreamCollector` re-exported. G8 behavior preserved; `finalize()` + progressive usage unchanged.
**Files:** `src/adapters/responses_to_anthropic/{mod,reasoning,collector,tests}.rs`.
**Depends on:** 11.7.

### Task 11.9 — Move G3 budgeting behind route/provider resolution + single request builder
**Priority:** HIGH · **Spec:** `.ralph/specs/T9-budgeting-layer-move.md` · **Commit:** `6b901fe`
**Status:** Codex-xhigh APPROVED (R4). G3 budgeting now budgets against the CONSERVATIVE MIN of
the per-candidate context windows in `BackendCandidatePlan` (extended: `candidates: Vec<BackendCandidate { model, context_limit }>`), not the pre-routing `resolved_model` alone. `RoutingUpstreamClient::backend_candidate_plan` attaches each candidate's per-provider limit from a new `RoutingProviderModelCatalog.context_limit_by_id` (populated in `refresh_catalog` from the same `/v1/models` snapshot); provider-identity scoping (chain index 0 only gets `primary_limit`; fallback/route candidates `None`) prevents wrong-window borrow. `candidate_context_floor` = min of known limits; unknown ⇒ no-op; empty ⇒ no-op. Engine-union fallback gated to `Config::is_plain_single_provider` only (routing/top-level-failover no-op when plan has no limit). Single builder `build_upstream_chat_request` + `UpstreamRequestAdditives` replace both the shadow `estimate_request_from_lowered` literal and the `run_turn` dispatch literal; `for_estimate` uses real `resolved_model` (threaded) + lower-bound-safe empties. Independent oracle `estimate_from_recorded` builds its own literal + `sanitize_chat_request` (now pub) + ceil(bytes/4) — no call to the production estimator (breaks G3 MEDIUM #19 self-reference). New tests: `preflight_routing_caps_against_provider_context_window`, `preflight_top_level_failover_no_ops_without_candidate_limit`. `estimate_request_from_lowered` private. **Deferred:** `RoutingResolution::Route` candidates carry `None` (route providers are synthetic; routing catalog doesn't load their /v1/models — pre-T9 no-op, not a regression); `normalize_upstream_model` ladder dedup (T2 deferral) remains for id resolution.
**Files:** `src/engine.rs`, `src/upstream.rs`, `src/config.rs`, `tests/port_server.rs`, `tests/common/mod.rs`, `tests/gateway.rs`.
**Depends on:** 11.1.

### Task 11.10 — AppError failover policy + upstream retry logging
**Priority:** MEDIUM · **Spec:** `.ralph/specs/T10-error-policy-and-logging.md` · **Commits:** `770a19a` + `960e63c`
**Status:** ✅ Codex-xhigh APPROVED (R2). Removed `failover_eligible: bool` + `is_failover_eligible()`
from `AppError`; replaced with a `pub(crate) enum FailoverDisposition { Failover (default), Terminal }`
(private field, read via `failover_disposition()`; terminal built via `upstream_with_disposition`). Enum
lives in `error.rs` (avoids an error→upstream cycle). Eligibility truth table proven UNCHANGED (every
generic ctor → Failover; only the persisted-G1-overflow site → Terminal; failover loop `Terminal` ⇔ old
`!is_failover_eligible()`); before-first-chunk rule untouched. G1 shrink-and-retry POST now logged via a
new `logged_send_chat_request` helper routing both the first + retry POST (first-POST logging parity
preserved); `analyze-log` needs no change (the reduced `max_tokens` shows as a `$.max_tokens` diff). R1
LOW (pub visibility) fixed in `960e63c`. 3 tests added.
**Files:** `src/error.rs`, `src/upstream.rs`, `tests/port_errors.rs`.

### Task 11.11 — Streaming + logging test-quality cleanups
**Priority:** LOW–MEDIUM · **Spec:** `.ralph/specs/T11-streaming-test-quality.md` · **Commits:** `31fc5f9` + `8c12092`
**Status:** ✅ Codex-xhigh APPROVED (R2). (1) G3-peek keepalive parameterized 1→3 ingress routes via an
`IngressRoute` table + `assert_idle_stream_emits_keepalive_ping`; scheduler-magic replaced with
advance(16s)-then-read, each read bounded by a paused-time `tokio::time::timeout` (absent ping → clean
panic, not a hang); mutation-verified per route. (2) G5 removal-race: new
`cleanup_dump_files_with_remover(dir, max_age, now, remove)` DI seam; the injected remover fails its first
call / succeeds the second → order-independent proof the loop continues past an `Err`. (3) G3
catalog-parser dedup: `extract_model_context_limits` deleted; limits now stored in
`RoutingModelCatalog.union_context_limit_by_id` (populated in `register_routing_model`'s first-seen branch
over the same `entry_context_limit` → byte-identical; `entry_context_limit` remains the single key
parser); each of the 5 keys now has isolated positive test coverage + a precedence entry. (4) G7
`port_config.rs` 1383 → `port_config.rs` 514 + new `port_config_routing.rs` 633 (27 → 27 tests, zero
coverage change). R1 (2 MEDIUM keepalive-hang + race-order, 1 LOW shadowed key) fixed in `8c12092`.
**Files:** `tests/port_streaming_peek.rs`, `tests/port_logging.rs`, `src/upstream.rs`, `tests/port_config.rs`, `tests/port_config_routing.rs` (new), `tests/common/mod.rs`.
**Depends on:** 11.1 (for the catalog-parser dedup item).

## Topic 12 — Thermo-nuclear PROJECT-review follow-ups (Round 2)

> **Source:** `/ralph-guide-update` on 2026-06-20, from the whole-codebase thermo-nuclear PROJECT review
> (`/tmp/thermo-project-review.md`; 16 parallel subsystem reviewers → Codex-xhigh adversarial verify →
> synthesis). 11 VERIFIED findings (2 HIGH, 5 MEDIUM, 4 REVISED-LOW; 0 refuted) → 10 tasks.
> Specs: `.ralph/specs/U1..U10`. Branch: `ralph/thermo-followups`. Review gate: `.ralph/REVIEW_PROTOCOL.md`
> (Codex-xhigh) per task; DoD = executable test green · `cargo test` · `cargo clippy --all-targets` ·
> `cargo fmt` · Codex-xhigh APPROVED · commit.
> **Sequencing:** 12.1 → 12.2 (both HIGH, `stop` semantics — land 12.1 first so the typed `stop` field is
> populated consistently); then MEDIUM 12.3–12.7; then LOW 12.8–12.10. All otherwise independent +
> parallelizable. **12.2 bundles** report findings 2+4 (the merge-helper collapse closes the drift that hid
> the missing `stop` arm); **12.9 bundles** both sides of the `tool_calls` wire-string contract. Obey
> AGENTS.md "Hard rules in the engine"; do NOT re-raise the adjudicated invalid findings below.

## STATUS (🔄 IN PROGRESS — 4/10)

**DONE:** 12.1 (`7d80dc6`), 12.2 (`f47357b`), 12.3 (`70ad24f`), 12.4 (`4cd2b44`).
**TODO (sequenced):** 12.5, 12.6, 12.7 (MEDIUM) → 12.8, 12.9, 12.10 (LOW).
Per-task loop = read spec → implement → fmt/test/clippy → commit → Codex-xhigh review → fix/re-review ≤3
rounds → record verdict + mark task done here. STOP when all 10 APPROVED.

### Task 12.1 — Anthropic stop_sequences honor OPENAI_MAX_STOP=4 hard-rule (400)
**Priority:** HIGH · **Spec:** `.ralph/specs/U1-stop-sequences-hardrule.md` · **Status:** ✅ DONE `7d80dc6` (Codex-xhigh APPROVED, round 2). Routed `stop_sequences` through `normalize_stop` into typed `ResponsesRequest.stop`; removed `extra_body["stop"]` smuggling. Round-1 HIGH (configured `stop` default shadowing typed `stop` → dup wire key) fixed by adding the `"stop" => request.stop.is_some()` arm to `chat_request_field_is_set` (the 12.2 arm, landed early) + `merge_{upstream,fallback}_chat_kwargs_does_not_shadow_typed_stop` regressions. **12.2 now narrows to the helper-collapse + remaining wire/alias tests — its `"stop"` arm is already in place.**
**Thermo finding:** Anthropic `stop_sequences` are mapped RAW into `extra_body["stop"]` (`src/adapters/anthropic_to_responses.rs:39-45`) while typed `ResponsesRequest.stop` stays `None` (`:79`); the OPENAI_MAX_STOP_SEQUENCES=4 → 400 ceiling in `normalize_stop` (`src/models/chat.rs:84-101`) only runs on the typed field (`src/engine.rs:1383`), so >4 sequences silently bypass the "400, not truncate" hard rule.
**Fix:** Route `request.stop_sequences` through `crate::models::chat::normalize_stop` inside `convert_request` and assign the result to the typed `ResponsesRequest.stop`; delete the `extra_body.insert("stop", …)` smuggling at `:39-45`. `convert_request` already returns `AppResult` so the >4 → `AppError::bad_request` propagates as a 400. Empty / all-empty lists collapse to `None` via the same normalizer.
**Files:** src/adapters/anthropic_to_responses.rs, src/models/chat.rs, tests/port_translation.rs
**Acceptance:** ≤4 non-empty sequences land in `result.stop` and `result.extra_body.get("stop")` is `None`; >4 sequences return BAD_REQUEST at convert time; empty/all-empty list yields `result.stop == None`; `port_translation.rs::anthropic_stop_sequences_move_to_extra_body` rewritten to assert the typed field + a new >4 → 400 test added; `cargo test` green; `cargo clippy --all-targets` clean; `cargo fmt`; Codex-xhigh APPROVED.
**Sequencing:** FIRST in Topic 12 (HIGH, wire-contract fix). No dependencies; only consumes existing `normalize_stop`/`OPENAI_MAX_STOP_SEQUENCES` and the existing typed `ResponsesRequest.stop` field.

### Task 12.2 — Upstream `stop` field-set arm + collapse duplicate chat-kwargs merge helpers
**Priority:** HIGH · **Spec:** `.ralph/specs/U2-upstream-stop-arm-and-merge-collapse.md` · **Status:** ✅ DONE `f47357b` (Codex-xhigh APPROVED, round 2). Collapsed `merge_upstream_chat_kwargs` + `merge_fallback_chat_kwargs` into one `merge_chat_kwargs_gap_fill` that ALWAYS applies the max-token-alias skip, shared by both leaf-finalize (`:2011`) and provider-fallback (`:810`) call sites; second helper deleted. The `"stop" => request.stop.is_some()` arm was pre-landed under 12.1 (`7d80dc6`), so this task narrowed to the helper collapse. config.rs strip list untouched. Tests: leaf-finalize wire-path test asserts client stop survives + no `extra_body["stop"]` + single `"stop"` in `serde_json::to_value`; in-module `request_for_provider` tests assert typed-stop-wins + provider-`max_tokens` alias-skip on the fallback path. Round-1 MEDIUM (provider-fallback only leaf/helper-tested) fixed by adding the dedicated `request_for_provider` tests.
**Thermo finding:** `chat_request_field_is_set` (`src/upstream.rs:2268-2285`) has no `"stop"` arm → falls to `_ => false`; a config `upstream_chat_kwargs.stop` gap-fills into the `#[serde(flatten)]` `extra_body` (`chat.rs:48`) alongside the typed `stop` (`chat.rs:47`), emitting a DUPLICATE `"stop"` wire key at the `reqwest .json` POST (`upstream.rs:573`) and dropping the client value on last-key-wins parsers; the near-identical `merge_upstream_chat_kwargs` (`:2037-2065`) vs `merge_fallback_chat_kwargs` (`:2251-2266`) fork — differing only by the max-token-alias guard (`:2047-2054`) the fallback variant lacks — is what hid it and leaks the alias collision on the `/v1/responses` provider-fallback path (engine.rs:1409, call site upstream.rs:810).
**Fix:** Add `"stop" => request.stop.is_some(),` to `chat_request_field_is_set`. Collapse the two gap-fill helpers into one `merge_chat_kwargs_gap_fill(request, defaults)` that ALWAYS applies the max-token-alias skip (no-op when no alias present), called by both leaf-finalize (`upstream.rs:2011`) and provider-merge (`upstream.rs:810`); delete the second helper. Do NOT add a `stop` strip to `config.rs:349-352` (the typed-field arm is the fix). Verify on the wire path that exactly one `"stop"` (the client value) is emitted with no dup key.
**Files:** src/upstream.rs, src/config.rs, tests/port_translation.rs, tests/port_routing.rs
**Acceptance:** `"stop" => request.stop.is_some()` arm added; two merge helpers collapsed to one always-alias-skip helper shared by both call sites with the second definition deleted; config.rs:349-352 strip list unmodified; leaf-finalize test asserts client stop survives + no `extra_body["stop"]` + single `"stop"` in `serde_json::to_value`; provider-fallback test asserts same with no dup key; provider-fallback test with client `max_completion_tokens` + provider `max_tokens` asserts the provider alias does not land; `cargo test` green; `cargo clippy --all-targets` clean; `cargo fmt`; Codex-xhigh APPROVED.
**Sequencing:** Sequence AFTER 12.1 (U1, the `anthropic_to_responses` `stop_sequences` typed-field HIGH) — both touch `stop` semantics; land 12.1 first so the typed `stop` field is populated consistently.

### Task 12.3 — Restore MonitorHub zero-overhead: lazy emit_with choke-point
**Priority:** MEDIUM · **Spec:** `.ralph/specs/U3-monitor-zero-overhead-emit-with.md` · **Status:** ✅ DONE `70ad24f` (Codex-xhigh APPROVED, round 3). Added `MonitorHub::emit_with(id, FnOnce() -> MonitorEventKind)` that early-returns before the build closure when disabled; `emit` delegates to it. Converted all eager engine.rs sites (RequestStarted count/`input_chars` fold, per-item ResponseItem/ToolPhase summarize+preview, the 3 `is_enabled()`-guarded payload previews, Failed events) into closures; call sites pass borrowed `&str` ids so the `String` alloc defers past `!enabled`; the `trailing_tool_output_items` loop is gated on `is_enabled()` (reverse-walk + Vec alloc). New disabled/enabled `emit_with` unit tests prove the closure is never invoked on `disabled()` and is invoked + reaches `snapshot()`/`subscribe()` when enabled. Round-1 (eager id clone + eager Failed) and round-2 (disabled-path trailing-tool Vec alloc) MEDIUMs fixed.
**Thermo finding:** Eager `MonitorEventKind` construction runs on the DISABLED hot path — `RequestStarted`'s ten `input.iter().filter().count()` passes + `input_chars` fold (`src/engine.rs:1061-1299`) and per-item `summarize_response_item`/`preview_json` (full serde + 4KB image redaction, `src/engine.rs:2579-2634`) execute at unguarded sites `:1310,1528,1560,2093,2148,2239,2249,2322,2329,2384` BEFORE `MonitorHub::emit` early-returns when disabled (`src/monitor.rs:257-260`), violating "MonitorHub::disabled() = zero-overhead no-op".
**Fix:** Add `MonitorHub::emit_with(id, impl FnOnce() -> MonitorEventKind)` to `src/monitor.rs` that checks `!self.enabled` and returns BEFORE invoking the closure, otherwise routes the owned kind through the identical sequence/prune/image-redaction/broadcast path as `emit` (`:257-285`). Convert the eager `engine.rs` sites to `emit_with(..., || …)` so traversal/summarize/preview run only when enabled, and fold the three already-guarded payload-preview sites (`:1300,:1412,:1855`) into `emit_with` to leave one disabled-check mechanism. Trivial delta sites may stay on `emit`. `MonitorEventKind` already derives `Clone + Serialize` (`src/monitor.rs:14`).
**Files:** src/monitor.rs, src/engine.rs
**Acceptance:** `emit_with` early-returns before the closure when disabled, identical broadcast/redaction path when enabled; disabled-path unit test proves the closure is NEVER invoked (panic/AtomicBool sentinel) on `MonitorHub::disabled()` and IS invoked on enabled hub reaching `snapshot()`/`subscribe()`; all eager `engine.rs` sites moved into closures (RequestStarted, ToolPhase loop, per-item ResponseItem/ToolPhase); three guarded preview sites converted to `emit_with` with `is_enabled()` wrappers removed and byte-identical 128KB/4KB previews + `sanitize_chat_request`/`flatten_content` preserved; image-URI redaction choke point not bypassed; existing `src/monitor.rs` snapshot tests + debug-UI coverage in `tests/gateway.rs` and `tests/port_streaming_peek.rs` green unchanged; `cargo test` green; `cargo clippy --all-targets` clean; `cargo fmt`; Codex-xhigh APPROVED.
**Sequencing:** No deps; independent of other Topic-12 tasks (touches only monitor.rs + engine.rs emit sites).

### Task 12.4 — Delete dead config resolve_upstream_chat_kwargs methods + retarget 8 precedence tests
**Priority:** MEDIUM · **Spec:** `.ralph/specs/U4-config-dead-resolve-and-wrong-precedence-tests.md` · **Status:** ✅ DONE `4cd2b44` (Codex-xhigh APPROVED, round 2). Deleted all three dead `pub fn`s (`rg` → zero matches); retargeted the 8 kwargs + 2 family tests onto `BackendFinalizationPolicies::from_config` → `finalize_request_for_backend` (asserting the wire `extra_body`), renamed `request_model_profile_overrides_upstream_model_profile_kwargs` → `leaf_resolves_only_final_model_profile_not_request_alias`; family-name-sniffing models renamed to neutral ids so the per-model `template_family` override (not name sniff) drives injection; port_config family test rewritten through the public leaf seam. `model_profiles_for_resolved_model`/system-prompt-prefix path untouched. Round-1 MEDIUM (no test exercised a non-empty global base) fixed by adding a non-empty `global_upstream_chat_kwargs` with a conflicting nested key + global-only key to `resolves_profile_specific_upstream_chat_kwargs` (per-model wins on conflict, global-only survives, unprofiled model gets only the global base).
**Thermo finding:** `Config::resolve_upstream_chat_kwargs` + `resolve_upstream_chat_kwargs_for_resolved_model` (src/config.rs:833-848) are DEAD in production (post-T1 the leaf finalizes via `BackendFinalizationPolicies::resolve_chat_kwargs`, src/upstream.rs:1858 ← finalize_request_for_backend:2011) yet 8 config tests (config.rs:2085/2220/2291/2380/2466/2606/2660/2851) assert their multi-profile merge precedence — a path the gateway no longer runs; `resolve_template_family` (config.rs:940) is likewise test-only (callers: config.rs:2120/2125/2140 + tests/port_config.rs:321/326).
**Fix:** Delete ONLY the two `resolve_upstream_chat_kwargs*` methods and `resolve_template_family`; KEEP `model_profiles_for_resolved_model` (config.rs:987) and `resolve_system_prompt_prefix_for_resolved_model` (config.rs:970) — still live via engine.rs:864. Retarget (not delete) the 8 kwargs tests + 2 family tests onto `BackendFinalizationPolicies::from_config(&config)` → `resolve_chat_kwargs`/`finalize_request_for_backend`, asserting the REAL leaf precedence (at-most-one per-model policy via `policy_for_model` over the global base; `config < family < effort-map < client`) instead of the old 3-profile config merge. Rewrite `tests/port_config.rs::template_family_still_resolves_through_profile_chain` through the public `llmconduit::upstream::finalize_request_for_backend` seam (the private `resolve_family_override`/`resolve_chat_kwargs` are not cross-crate callable). Wire output stays byte-identical.
**Files:** src/config.rs, tests/port_config.rs
**Acceptance:** three `pub fn`s deleted, `rg 'resolve_upstream_chat_kwargs|resolve_template_family' src/ tests/` → zero matches; `model_profiles_for_resolved_model`/system-prompt-prefix path untouched and passing; 8 kwargs tests + 2 family tests retargeted onto `BackendFinalizationPolicies::from_config`/`finalize_request_for_backend` asserting single-per-model-over-global precedence (the 3-profile `request_model_profile_overrides_upstream_model_profile_kwargs` renamed to describe real leaf behavior); port_config family test rewritten through the public leaf seam; production wire output byte-identical; `cargo test` green; `cargo clippy --all-targets` clean; `cargo fmt`; Codex-xhigh APPROVED.
**Sequencing:** Depends on T1 leaf APIs already shipped (`from_config`, `finalize_request_for_backend`, `BackendChatRequest::new` all `pub`). Independent of other Topic-12 tasks; config.rs + tests only.

### Task 12.5 — Test coverage for WEB_SEARCH_ROUNDS_HARD_CEILING=25
**Priority:** MEDIUM · **Spec:** `.ralph/specs/U5-web-search-ceiling-coverage.md` · **Status:** ⬜ PENDING
**Thermo finding:** The AGENTS.md hard rule `WEB_SEARCH_ROUNDS_HARD_CEILING=25` and its `.min(25)` config cap (`src/engine.rs:1772-1781`) have ZERO test coverage; every Config literal sets `max_web_search_rounds:5` (`tests/gateway.rs:3034`) and none queues >limit forced rounds — while the sibling `IMAGE_ANALYSIS_ROUNDS_HARD_CEILING=8` IS tested (`tests/image_agent.rs:844`).
**Fix:** Test-only, NO production change. Add a `tests/gateway.rs` test mirroring `image_agent_round_ceiling_terminates_loop`: queue a forced `web_search` `tool_call_chunk` every round for N>limit with `MockSearch::default()` canned results, assert `response.failed` ("web search round limit exceeded") plus a bounded `upstream.requests().await.len()` (==5 under default config). Add a SECOND test via `test_gateway_with_config` with `max_web_search_rounds>25` (e.g. 100) and >25 forced rounds, asserting termination by exactly round 25 — proving the `.min(WEB_SEARCH_ROUNDS_HARD_CEILING)` cap overrides the higher configured value. Do NOT lower the ceiling or touch `src/engine.rs`.
**Files:** tests/gateway.rs
**Acceptance:** new forced-loop test asserts `response.failed` + bounded upstream round count (==5 default); second test sets `max_web_search_rounds>25` and asserts termination at exactly round 25 (`.min(25)` cap); both keep `brave_api_key: Some(..)`; `git diff --name-only` touches only `tests/` (engine.rs `25` literal + `.min(...)` unchanged); `cargo test` green; `cargo clippy --all-targets` clean; `cargo fmt`; Codex-xhigh APPROVED.
**Sequencing:** Independent test-only task; no deps on other Topic-12 tasks. Reuses existing `tests/gateway.rs` and `tests/common/mod.rs` helpers (no helper changes required).

### Task 12.6 — Single canonical hop-by-hop header filter for both proxy halves
**Priority:** MEDIUM · **Spec:** `.ralph/specs/U6-hop-by-hop-header-dedup.md` · **Status:** ⬜ PENDING
**Thermo finding:** `is_hop_by_hop_header` + `header_name_eq` are byte-identical duplicates in both `/v1/completions` proxy halves — `src/http.rs:937-954` (response) and `src/upstream.rs:2376-2393` (request) — so the two lists can silently drift and strip different header sets.
**Fix:** Hoist one canonical `is_hop_by_hop_header` + `header_name_eq` pair into a shared `pub(crate)` location (e.g. new `src/proxy_headers.rs` registered in `src/lib.rs`), call it from both `should_proxy_response_header` (http.rs) and `should_proxy_request_header` (upstream.rs), and delete the second copy. Keep the 8-element RFC list and order, the ASCII-case-insensitive compare, and each direction's extra filters (request also drops authorization/host/content-length; response drops content-length) byte-identical. Add a parity test asserting both directions strip the same hop-by-hop set and pass a representative passthrough header.
**Files:** src/http.rs, src/upstream.rs, src/lib.rs, src/proxy_headers.rs (new)
**Acceptance:** exactly one `fn is_hop_by_hop_header` + one `fn header_name_eq` in the crate (grep returns one hit each); both halves call the canonical pair; hop-by-hop list contents+order and `eq_ignore_ascii_case` unchanged; request-side authorization/host/content-length and response-side content-length filters preserved; wire behavior byte-identical (no header changes strip/passthrough state in either direction); parity test fails on divergence; cargo test green; cargo clippy --all-targets clean; cargo fmt; Codex-xhigh APPROVED.
**Sequencing:** Independent — no deps on other Topic 12 tasks; touches only the two proxy header helpers plus a new module + lib.rs registration.

### Task 12.7 — ToolDeltaGate per-call cap: O(1) running byte count (kill O(n^2) re-sum)
**Priority:** MEDIUM · **Spec:** `.ralph/specs/U7-tool-delta-gate-running-bytes.md` · **Status:** ⬜ PENDING
**Thermo finding:** The `Pending`/`None` buffering arm re-sums the whole buffer via `buffered_len(buffered)` on every nameless delta to enforce the per-call cap (`src/tool_delta_gate.rs:229`, helper `:54-56`), making one pending call O(n^2) in delta count — 1-byte fragments under the 256 KiB cap permit 262,144 deltas → ~34.36B fragment visits (bounded DoS behind `vision_active` + operator backend).
**Fix:** Carry a running `bytes: usize` inside the `AnalyzeDeltaState::Pending` variant (`:44-47`), initialized to `0` at the sole construction site (`:162-164`). Change the per-call cap check at `:229` to compare `bytes + delta_bytes > MAX_PENDING_TOOL_DELTA_BYTES_PER_CALL` (O(1), no `buffered_len`) and increment `bytes` alongside the existing `buffered.push(...)` at `:234-235`. The cross-call total cap already uses the O(1) `pending_buffer_bytes` counter; the three once-per-call terminal subtractions (`:180-182`, `:212-214`, `:256-258`) run once and stay correct. Cap boundaries stay byte-identical; engine callers (`engine.rs:1618`, `:1709`) untouched.
**Files:** src/tool_delta_gate.rs
**Acceptance:** Per-call cap reads a running `bytes` field with no `buffered_len` call on the per-delta path (invariant `bytes == buffered_len(buffered)` after each push); per-call cap trips at the same 256 KiB boundary and total cap at the same 1 MiB boundary, byte-identical; eight existing tests stay green unchanged (incl. `per_call_pending_byte_cap_overflows`, `total_pending_byte_cap_overflows_across_calls`, both reclaim tests); ADD a test that many 1-byte nameless deltas accumulate to exactly the per-call cap (all `None`) and the next byte overflows (`Err(PendingBufferOverflow)`); ADD a test that empty-string deltas add `0` and stay no-ops; allocation-free hot path and `vision_active=false` pass-through preserved; `cargo test` green; `cargo clippy --all-targets` clean; `cargo fmt`; Codex-xhigh APPROVED.
**Sequencing:** Depends on T3 (ToolDeltaGate extraction, FINAL). Self-contained within `src/tool_delta_gate.rs`; no engine or wire change; parallelizable with other Topic-12 tasks.

### Task 12.8 — Replace flaky wall-clock sleep() test sync with deterministic Notify
**Priority:** LOW · **Spec:** `.ralph/specs/U8-image-agent-flaky-sleep.md` · **Status:** ⬜ PENDING
**Thermo finding:** Two flaky wall-clock `sleep()` test-sync points — `image_agent_cancellation_drops_vision_work` (`tests/image_agent.rs:379`, 50ms) and `upstream_request_log_redacts_image_data_when_agent_disabled` (`tests/image_agent.rs:1629`, 100ms, race admitted in-comment) — instead of the bounded `Notify`/timeout idiom already in `tests/gateway.rs:5764-5773`.
**Fix:** Add `entered`/`dropped` `Arc<Notify>` fields + accessors to `MockVisionClient` (`tests/common/mod.rs:301`): notify `entered` at the top of `analyze` after recording the request (`:334`), and fire `dropped` from a drop guard inside `analyze` (mirroring `NotifyOnDrop` at `tests/gateway.rs:179-187`). Rewrite the cancellation test to `await timeout(1s, entered)` before `drop(stream)` and `timeout(1s, dropped)` after, replacing the 50ms sleep. Replace the 100ms sleep in the redaction test with a bounded poll-until-non-empty wrapped in `timeout(1s, …)`. Test-only; no `src/` change and no wire-byte change; all existing assertions preserved.
**Files:** tests/common/mod.rs, tests/image_agent.rs
**Acceptance:** `MockVisionClient` gains `entered`/`dropped` `Arc<Notify>` (entered fired post-request-record, dropped via drop guard) with `.notified()` accessors, `block_on`/`requests`/`push_outcome` unchanged; cancellation test awaits `timeout(1s, entered)` before drop and `timeout(1s, dropped)` after, 50ms sleep removed, `len()==1` assertion kept; redaction test replaces 100ms sleep with bounded `timeout(1s, …)` poll-until-non-empty, all three content assertions + cleanup kept; no `tokio::time::sleep`/`thread::sleep` left in either test (grep-checkable); both tests still fail on property regression; `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.
**Sequencing:** Independent, test-only; no deps on other Topic-12 tasks. Parallelizable.

### Task 12.9 — TerminalReason tool_calls wire-string: delegate consumer + pin producer test
**Priority:** LOW · **Spec:** `.ralph/specs/U9-terminal-reason-wire-contract.md` · **Status:** ⬜ PENDING
**Thermo finding:** Both ends of the `TerminalReason` wire-string contract are fragile: the producer's load-bearing `#[serde(rename = "tool_calls")]` (`src/models/responses.rs:519`) is unpinned by any test (drop it → engine emits `"tool_call"`, converter falls to `Other`, whole suite blind), and the consumer `response_terminal_reason` (`src/adapters/responses_to_anthropic/mod.rs:814-826`) hand-rolls a duplicate string→variant match (lines 819-825) that canonical `TerminalReason::from_finish_reason` (`responses.rs:538-546`) already provides — a new variant silently falls to `Other`.
**Fix:** One pass dedups both sides. Replace the consumer's inline `match reason { ... }` with `.and_then(Value::as_str).map(|r| TerminalReason::from_finish_reason(Some(r)))` — byte-identical (`from_finish_reason(Some("other")) == Other`; absent ⇒ `None`, preserving the T7 R1 PRESENT-but-unknown→`Other` invariant in the `mod.rs:807-813` doc comment). Add a producer unit test in `responses.rs mod tests` (~:724) asserting `serde_json::to_value` of each variant yields exactly `"stop"/"length"/"tool_calls"/"content_filter"/"other"`, locking the load-bearing `tool_calls` rename. Keep `TerminalReason` Serialize-only; G8 promotion behavior unchanged.
**Files:** src/models/responses.rs, src/adapters/responses_to_anthropic/mod.rs, tests/port_response_translation.rs
**Acceptance:** Consumer delegates to `from_finish_reason` (no hand-rolled match remains in `responses_to_anthropic/`); PRESENT-but-unknown→`Other` and absent→`None` preserved; new producer serialization unit test pins all five wire strings (esp. `ToolCall`→`"tool_calls"`); `reasoning_only_at_tool_calls_stays_thinking` and all G8 promotion tests still pass; `#[serde(rename = "tool_calls")]` retained; cargo test green; cargo clippy --all-targets clean; cargo fmt; Codex-xhigh APPROVED.
**Sequencing:** Depends on T7 (FINAL/APPROVED). No conflict with other Topic-12 tasks; no ordering constraint — independent.

### Task 12.10 — Chat-lowering dedup: single tool-name authority + data-driven web_search placeholder
**Priority:** LOW · **Spec:** `.ralph/specs/U10-chat-lowering-dedup.md` · **Status:** ⬜ PENDING
**Thermo finding:** Duplicate-tool-name rejection implemented twice with divergent keying — `lower_tools` raw case-sensitive `seen_names` HashMap (`src/adapters/responses_to_chat.rs:461`, checked `:578-585`) vs `build_tool_registry` lowercased (`:643-647`); and `web_search_placeholder_result` (`:741-780`) is a ~40-line nested match re-typing one base sentence ~6×.
**Fix:** Delete the `seen_names` HashMap (`:461`) and its check (`:578-585`) so `build_tool_registry`'s stricter case-insensitive check is the single duplicate-name authority; move/rewrite `duplicate_tool_name_rejected` (`:1100`) to assert via `lower_request`/registry and add a case-insensitive (`echo`/`ECHO`) case. Collapse `web_search_placeholder_result` to one base template (action label `""`/`" open_page"`/`" find_in_page"`) plus a data-driven `Vec<String>` of present fragments (`Query:`/`URL:`/`Pattern:`) joined `". "` and appended with a leading space — keep the `query.or_else(first queries)` selection and append no trailing period. Both are byte-identical-on-the-wire pure refactors.
**Files:** src/adapters/responses_to_chat.rs (lower_tools, build_tool_registry, web_search_placeholder_result + #[cfg(test)] module)
**Acceptance:** `seen_names` HashMap + check removed, `build_tool_registry` case-insensitive rejection unchanged and sole authority; duplicate-name rejection still fires end-to-end via `lower_request` with message prefix `duplicate tool name is not supported: ` (+ new case-insensitive test); `web_search_placeholder_result` rebuilt from one base template + joined fragment Vec with NO trailing period and identical `query.or_else(first queries)` selection; placeholder output byte-identical for Search/OpenPage/FindInPage(all field combos)/Other/None asserted by full-string equality; `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.
**Sequencing:** Independent — no deps on other Topic-12 tasks; touches only the lowering adapter.

## Thermo-nuclear review — invalid findings (NOT tasks)
- **G8 `emit_thinking` suppression:** toggle does not exist in the codebase; G8 spec acceptance
  criterion is explicitly conditional ("if an emit-thinking toggle is added"). Re-derive if/when
  the toggle is added.
- **G5 `.jsonl` exclusion:** explicit G5 spec acceptance criterion ("Only `*.json` / `*.ndjson` are
  eligible; other extensions are skipped"). By design.
