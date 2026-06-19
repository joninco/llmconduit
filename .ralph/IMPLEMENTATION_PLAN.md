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

### Task 11.1 — Leaf-side profile resolution (template_family + upstream_chat_kwargs)
**Priority:** HIGH · **Spec:** `.ralph/specs/T1-leaf-profile-resolution.md`
Move `template_family` + `upstream_chat_kwargs` resolution from the engine (pre-routing) to
`upstream::finalize_request_for_backend`, mirroring `reasoning_effort_policies`. Introduce a
`BackendChatRequest` wrapper (remove `#[serde(skip)]` side-channel fields). Collapses the triply-
duplicated model-precedence truth. **Touches the effort leaf → live-verify `claude --effort
high/max/off` on :5022.**
**Files:** `src/config.rs`, `src/engine.rs`, `src/upstream.rs`, `src/models/chat.rs`, tests.
**Blocks:** 11.2, 11.9.

### Task 11.2 — Typed routing-candidate plan (delete G4 side-channel vision gating)
**Priority:** HIGH · **Spec:** `.ralph/specs/T2-routing-candidate-plan.md`
Delete `request_model_genuinely_resolves` + side-channel native-vision gating (`engine.rs:759`);
return a typed backend-candidate plan from the real routing/failover layer; reuse for G4 gating.
**Files:** `src/engine.rs`, `src/upstream.rs` (routing), tests.
**Depends on:** 11.1.

### Task 11.3 — Extract ToolDeltaGate from run_turn
**Priority:** HIGH · **Spec:** `.ralph/specs/T3-tooldeltagate-extraction.md`
Extract the `analyzeImage` delta-buffer state machine + duplicated monitor/SSE emission paths out
of `run_turn` (`engine.rs:1277`) into a `ToolDeltaGate` with unit tests.
**Files:** `src/engine.rs` (+ new module), tests.

### Task 11.4 — Split vision.rs + image-agent test suite
**Priority:** MEDIUM · **Spec:** `.ralph/specs/T4-vision-module-split.md`
Split `src/vision.rs` (1,364 lines) into `vision/{cache,strip,client}.rs` + `src/redaction.rs`;
move the image-agent suite + `MockVisionClient` to `tests/image_agent.rs`. Pure structural move.
**Files:** `src/vision.rs` (+ new files), `src/redaction.rs`, `tests/gateway.rs`, `tests/image_agent.rs`.

### Task 11.5 — Bytes-specialized SSE guard (cap before copy)
**Priority:** HIGH · **Spec:** `.ralph/specs/T5-sse-guard-bytes.md`
Specialize the bounded stream adapter to `Bytes`; scan borrowed bytes before yielding; retain only
the ≤3-byte carry. Removes the O(chunk) pre-rejection allocation (`upstream.rs:2474`, `2636`).
**Files:** `src/upstream.rs` (or the guard module from 11.6).
**Coordinates with:** 11.6 (place in the new module if both land).

### Task 11.6 — Extract SSE guard module + shrink port_streaming.rs
**Priority:** MEDIUM · **Spec:** `.ralph/specs/T6-sse-guard-extract.md`
Extract the SSE grammar state machine + `SseFrameGuard` to `src/sse_guard.rs`; make it `pub(crate)`
(white-box tests → module unit tests); shrink `tests/port_streaming.rs` (1,432 lines) to acceptance
cases; remove "Codex round" archaeology.
**Files:** `src/upstream.rs`, `src/sse_guard.rs` (new), `tests/port_streaming.rs`.

### Task 11.7 — Typed terminal reason in the canonical response
**Priority:** MEDIUM · **Spec:** `.ralph/specs/T7-typed-terminal-reason.md`
Carry a typed terminal reason (or map all non-stop → non-clean) so promotion gating uses an explicit
reason, not `event_type == "response.completed"` (`responses_to_anthropic.rs:468`).
**Files:** `src/models/responses.rs`, `src/engine.rs`, `src/adapters/responses_to_anthropic.rs`, tests.
**Blocks:** 11.8.

### Task 11.8 — Extract ReasoningEgressState from responses_to_anthropic
**Priority:** MEDIUM · **Spec:** `.ralph/specs/T8-reasoning-egress-state.md`
Extract `reasoning_buffer`/`reasoning_signature`/`content_started`/`has_tool_calls` + flush logic
into a `ReasoningEgressState` typed state machine; split the 2,020-line converter into focused
modules. Pure structural extraction.
**Files:** `src/adapters/responses_to_anthropic.rs` (+ new files), tests.
**Depends on:** 11.7.

### Task 11.9 — Move G3 budgeting behind route/provider resolution + single request builder
**Priority:** HIGH · **Spec:** `.ralph/specs/T9-budgeting-layer-move.md`
Move G3 budgeting to upstream dispatch (post route/provider resolution), or budget against a
conservative-min candidate set with unknown=no-op; one first-upstream-request builder for estimate +
dispatch; independent test oracle (not reusing the production estimator).
**Files:** `src/engine.rs`, `src/upstream.rs`, `tests/port_server.rs`.
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
