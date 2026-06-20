# IMPLEMENTATION_PLAN.md — llmconduit gap implementation

Implements the 7 core gaps in `GAPS.md`, plus the owner-directed EXTENDED RUN of the originally-deferred
G4 (image agent) + G7 (route config) + the descoped G3 keep-alive-peek. Specs: `.ralph/specs/*`
(historical design inputs — see "Spec status" below). Review gate: `.ralph/REVIEW_PROTOCOL.md`.

## Executive summary
**Status: core 7/7 ✅ + EXTENDED RUN COMPLETE ✅ (G3-peek, G7, G4 — owner-directed, all Codex-xhigh APPROVED). ALL 9 GAPS + P1 + G3-peek DONE, plus a post-run `reasoning_effort_map` rework (leaf-applied, reserved-key deleted), plus a per-gap thermo-nuclear code-quality review (10 gaps reviewed; bounded fixes in `07117b2`; 11 deferred follow-ups tracked as Topic 11 on branch `ralph/thermo-followups`).** Loop validated: build → cargo test/clippy/fmt → Codex-xhigh review → fix → re-review APPROVED.

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

## STATUS (T4 APPROVED — orchestrator resume session `thermo-followups-resume`)

**DONE (Codex-xhigh APPROVED + committed):** T1, T2, T7, T8, T9, T6, T5, T3, T4 (9 of 11).
**REMAINING (in dependency order):** T10 → T11. Serial (`--agents 1`); T10/T11 independent. Each
remaining task gets its OWN fresh agent (clean context per task).
  - **T10** (AppError failover policy + G1 retry logging): independent.
  - **T11** (streaming/logging test-quality + catalog-parser dedup, depends on T1): independent.
**Review log:** `/tmp/thermo-followup-review.md` holds 9 verdicts (T1×2, T2×3, T7×2, T8×1, T9×4, T6×2, T5×2, T3×4, T4×2).
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
**Priority:** MEDIUM · **Spec:** `.ralph/specs/T10-error-policy-and-logging.md`
Remove `failover_eligible` from `AppError` (upstream-attempt disposition/variant); log the G1
shrink-and-retry POST (logged-send helper).
**Files:** `src/error.rs`, `src/upstream.rs`, `analyze-log`.

### Task 11.11 — Streaming + logging test-quality cleanups
**Priority:** LOW–MEDIUM · **Spec:** `.ralph/specs/T11-streaming-test-quality.md`
G3-peek keepalive parameterization across all 3 routes + scheduler-magic harness cleanup; G5
removal-race test seam; G3 catalog-parser dedup (`extract_model_context_limits`); G7
`port_config.rs` split.
**Files:** `tests/port_streaming_peek.rs`, `tests/port_logging.rs`, `src/upstream.rs`, `tests/port_config.rs`.
**Depends on:** 11.1 (for the catalog-parser dedup item).

## Thermo-nuclear review — invalid findings (NOT tasks)
- **G8 `emit_thinking` suppression:** toggle does not exist in the codebase; G8 spec acceptance
  criterion is explicitly conditional ("if an emit-thinking toggle is added"). Re-derive if/when
  the toggle is added.
- **G5 `.jsonl` exclusion:** explicit G5 spec acceptance criterion ("Only `*.json` / `*.ndjson` are
  eligible; other extensions are skipped"). By design.
