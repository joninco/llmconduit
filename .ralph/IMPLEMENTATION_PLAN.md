# IMPLEMENTATION_PLAN.md — llmconduit gap implementation

Implements 7 of the 9 gaps in `GAPS.md` (G4 image-agent + G7 route-config deferred — LOW priority and
architecturally divergent). Specs: `.ralph/specs/*`. Review gate: `.ralph/REVIEW_PROTOCOL.md`.

## Executive summary
**Status: 7/7 implemented (P1, G5, G1, G2, G8, G6, G3 ✅). Run COMPLETE.** Loop validated: build → cargo test/clippy/fmt → Codex-xhigh review → fix → re-review APPROVED.
Execution: **serial (`--agents 1`)** — G1/G2/G3 all edit `src/engine.rs`, so parallel runs would
conflict; serial also keeps the per-gap Codex-xhigh review gate clean. Run with
`/ralph-orchestrate --agents 1 --no-review` (built-in multi-model review disabled; this plan's
per-gap Codex review replaces it).

## Working agreement (every task)
1. Study the gap's `.ralph/specs/<ID>.md` AND the referenced claude-relay impl in
   `/home/jon/git/claude-relay/claude_relay/` — adapt to Rust + canonical-Responses, do NOT transliterate.
2. Confirm with code search before assuming something is missing (some gaps are PARTIAL).
3. Obey AGENTS.md "Hard rules in the engine" — they are load-bearing.
4. Definition of Done per task = executable test green · `cargo test` whole suite green · `cargo clippy
   --all-targets` clean · `cargo fmt` · **Codex-xhigh review APPROVED** (`.ralph/REVIEW_PROTOCOL.md`) · commit.
5. Record discoveries/decisions back here (use a subagent); keep AGENTS.md operational-only.

---

## Pending tasks (priority order)

### Task 1 — P1 ✅ COMPLETED — commit `1faba60`, Codex-xhigh APPROVED (round 2). See Completed tasks.

### Task 2 — G5 ✅ COMPLETED — commit `b610a53` (new `src/log_rotation.rs`), Codex-xhigh APPROVED (R2). See Completed tasks.

### Task 3 — G1 ✅ COMPLETED — commit `ebe6b41`, Codex-xhigh APPROVED (round 7). See Completed tasks.

### Task 4 — G3 ✅ COMPLETED (resolved via OPTION B — estimate the lowered upstream payload)
- First implementation went through **9 Codex-xhigh review rounds / 12 findings** (rounds 4-9 were the
  same floor-vs-synthetic class) and was reverted. Re-done as the minimal claude-relay port; the re-attempt
  then took **several more Codex-xhigh rounds, ALL the same estimator-vs-payload class** — the pre-flight
  estimate diverging from the true upstream bytes (raw `ToolSpec` vs `lower_tools`; raw `request.input` vs
  lowering-dropped `ImageGenerationCall`; then dropped `Message` subfields, `text.verbosity`,
  `reasoning.summary`; finally the leaf's `sanitize_chat_request` content-flatten below lowering). Patching
  each field individually kept resurfacing the class. **Resolved per user decision by OPTION B at the
  TERMINAL layer: estimate over the EXACT serialized bytes the leaf POSTs — the lowered payload AFTER
  `sanitize_chat_request`** (`engine::estimate_request_from_lowered`). No transform exists below
  `sanitize_chat_request`, so an over-count is structurally impossible and the class is permanently closed.
- **OPEN QUESTION resolution (context-length source):** llmconduit had NO per-model context length before
  this. Chosen option (b)+(a-lite): parse context length from the upstream `/v1/models` entries (reusing
  the SAME key set as the Anthropic `/v1/models` reshape in `http.rs`: `max_input_tokens`, `context_length`,
  `context_window`, `max_context_length`, `max_model_len`), cache it in `UpstreamModelCatalog`
  (`context_limit_by_id`), and **gate all budgeting on the limit being known — no-op when unknown**. Ids and
  context limits come from a SINGLE `/v1/models` snapshot via a combined
  `UpstreamClient::supported_model_catalog() -> Vec<UpstreamModelEntry>` (default impl parses `list_models()`
  once; routing client builds from its cached union), so they can never describe different provider states.
  No config field added.
- **Design (one pre-spawn seam in `stream_responses`):** `lower_request` (already pre-spawn; `?` surfaces
  validation/lowering errors exactly as before, so budgeting adds no new error path and runs only on success)
  produces `LoweredTurn`. `estimate_input_tokens(&lowered, flatten_content)` builds the chat request from the
  lowered fields and runs the SAME `sanitize_chat_request` the leaf runs (single source of truth via
  `pub estimate_request_from_lowered`), then `ceil(bytes(serde_json(sanitized))/4)` — the EXACT POST body.
  Then cap an EXPLICITLY-requested `max_output_tokens` to `min(requested, context − est − 128)`; fixed 128
  margin; `est+margin ≥ context` ⇒ HTTP 400 before any `tokio::spawn`/upstream POST (clean-400 mechanism
  unchanged). **No floor, no synthesized cap, never raises; mutates only the typed field.**
- **Why it stays safe (terminal layer):** counting the post-`sanitize_chat_request` body means nothing
  transforms it further (content-flatten, tool_choice-clear, arg-stringify all already applied), so an
  over-count is impossible. The only omissions are the ADDITIVE leaf merges — `extra_body`/
  `upstream_chat_kwargs` + G2 family `chat_template_kwargs` + `temperature`/`stop`/penalties — which only
  GROW the real payload, so the estimate stays a safe lower bound → never a false 400. Deliberately keeps G3
  OUT of the kwargs-merge seam (its entanglement caused the original thrash). Covers the FIRST upstream turn;
  later tool-loop turns rely on G1's reactive shrink-and-retry. Accepted cost: one extra pre-spawn
  `lower_request` + side-effect-free `find_replay_baseline` read (`run_turn` re-lowers per loop anyway).
- Tests in `tests/port_server.rs` (8: margin/keep/cap/reject + image_generation TOOL, image_generation
  CALL-in-INPUT, reasoning.summary, and multi-part-text-flattens-like-string leaf-layer class guards) — the
  oracle reads the RECORDED upstream `ChatCompletionRequest` and reuses `estimate_request_from_lowered`
  (same sanitize), so it tracks the real wire payload across future lowering/sanitize changes; plus
  `extract_supported_model_catalog`/`extract_model_context_limits` unit tests in
  `src/upstream.rs`.

### Task 5 — G2 ✅ COMPLETED — commit `d1e626e`, Codex-xhigh APPROVED (round 3). See Completed tasks.
- Family detection/injection at the provider LEAF (after model rewrite); `template_family` override via profile chain. Chat reasoning suppression broadened to family-independent (drop unrequested reasoning for ALL models — user decision) to avoid a cross-layer family-signal residual.

### Task 6 — G8 ✅ COMPLETED — commit `8297ca6`, Codex-xhigh APPROVED (round 3). See Completed tasks.
- Reasoning buffered (needed for promote-at-stop); promote only on clean `response.completed`; signature/length/incomplete → thinking; late reasoning dropped; web_search surfaced via additive event only (not a client tool_use).

### Task 7 — G6 ✅ COMPLETED — commit `881cfe1`, Codex-xhigh APPROVED. See Completed tasks.
- VERIFY FINDING: gap was REAL — `eventsource-stream` 0.2.3 accumulates upstream SSE bytes unbounded (no cap); a never-terminated/oversized frame OOMs. Added a per-frame byte cap (`SseFrameGuard`, EOL-grammar-correct, EOF-finalized, config `max_sse_frame_bytes` default 8 MiB) guarded by a reference-oracle differential test.

---

## Completed tasks
| Task | Gap | Commit | Review |
|-|-|-|-|
| 1 | P1 output_config.effort → reasoning.effort | `1faba60` | Codex-xhigh APPROVED (R2) |
| 2 | G5 debug-dump log rotation (mode-aware dirs) | `b610a53` | Codex-xhigh APPROVED (R2) |
| 3 | G1 context-window-limit retry (regex classifier) | `ebe6b41` | Codex-xhigh APPROVED (R7) |
| 5 | G2 Kimi/DeepSeek family kwargs (provider-leaf) | `d1e626e` | Codex-xhigh APPROVED (R3) |
| 6 | G8 reasoning promotion/suppression (Anthropic) | `8297ca6` | Codex-xhigh APPROVED (R3) |
| 7 | G6 SSE per-frame buffer cap (DoS guard) | `881cfe1` | Codex-xhigh APPROVED |

## Discoveries (encode lessons — read before related tasks)
- **Effort normalization is single-sourced** in `responses_to_chat::normalize_reasoning_effort`
  (responses_to_chat.rs:~337): trims, lowercases, `unknown → "high"`. Adapters must pass effort
  strings through RAW (non-empty, trimmed) and NOT re-validate/allow-list — doing so creates a
  divergent second normalizer (this was the P1 Codex finding). **Relevant to G2** (family kwargs)
  and any future effort handling.
- **Context-overflow classifier (G1) is regex-based** in `upstream.rs::classify_context_overflow`
  (`regex` crate + `LazyLock`), mirroring claude-relay `backend.py`/`server.py`. Each of the 4 shape
  regexes MUST carry its shape's DISTINCTIVE leading literal (`cannot be greater than`,
  `requested N output tokens`, `requested N tokens … in the completion`, `requested token count exceeds`)
  — matching on generic anchors alone (`maximum context length is/of`) overmatches unrelated 4xx bodies
  (took 7 review rounds to converge). G1 extracts limits REACTIVELY from error strings. **Relevant to G3**,
  which is the PROACTIVE complement (needs context length BEFORE the request — a different source) and
  reuses the `min_completion_tokens` floor config G1 added.
- **G3 was DEFERRED after thrashing.** Proactive budgeting via a byte/4 input estimate + a
  `min_completion_tokens` floor + "synthesize a cap when nothing was requested" proved highly edge-prone:
  9 review rounds, the floor kept fighting "only cap down / honor explicit + provider defaults" across
  layers (engine pre-spawn, per-loop, the failover/routing kwargs seam). **Lesson:** if revisited, do the
  SIMPLE thing the claude-relay reference does — cap only an EXPLICITLY-requested budget downward + reject
  clear overflow; do NOT synthesize a cap or impose a floor. The reactive net (G1) handles the rest.
- **G3 pre-flight estimate: count the bytes the LEAF POSTs (post-`sanitize_chat_request`), not any earlier
  representation (Option B, terminal layer).** The re-attempt thrashed for several rounds, ALL one class:
  `estimate_input_tokens` byte count diverging from the true wire bytes — first raw `ToolSpec` vs
  `lower_tools`, then raw `request.input` vs lowering-dropped `ImageGenerationCall`, then dropped `Message`
  subfields / `text.verbosity` / `reasoning.summary`, then the leaf's `sanitize_chat_request` content-flatten
  (multi-part text → bare string) BELOW lowering. **Lesson: patching each upstream transform one-by-one is
  whack-a-mole — every layer between your estimate and the socket can reopen the class. Estimate at the
  TERMINAL layer: the exact serialized request the leaf POSTs.** Build a `ChatCompletionRequest` from the
  lowered fields and run the SAME `sanitize_chat_request` (single source of truth —
  `engine::estimate_request_from_lowered`), then `ceil(bytes/4)`. Nothing transforms the body below sanitize,
  so an over-count is structurally impossible. OMIT the ADDITIVE leaf merges (`extra_body`/
  `upstream_chat_kwargs`, G2 family kwargs, sampling scalars) — they only grow the payload, so the estimate
  stays a safe lower bound (never a false 400) AND G3 stays out of the kwargs-merge seam that caused the
  original thrash. Reorder so `lower_request`'s `?` surfaces validation errors before budgeting (no new error
  path). Tests assert against the RECORDED upstream request + reuse the same sanitize, so they track the real
  wire payload across future lowering/sanitize changes (no hand-mirror).
