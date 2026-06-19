# GAPS.md — claude-relay → llmconduit behavior port

Gap inventory from porting `~/git/claude-relay`'s pytest suite (Python OpenAI↔Responses proxy)
to `llmconduit` (Rust). Outcome is a **categorized gap inventory**, not test parity.

> **STATUS — ALL GAPS CLOSED ✅** (as of branch `ralph/implement-gaps`). Every catalogued gap
> (G1–G8) plus the P1 partial and the descoped G3 keep-alive peek is implemented, tested, and
> Codex-xhigh APPROVED. The per-gap analysis below is retained for archival value (it records *why*
> each was a gap and *how* it was resolved); each heading now carries a `✅ RESOLVED` tag with its
> commit. Implementation ledger + discoveries: `.ralph/IMPLEMENTATION_PLAN.md`.

## Method

- Read each pytest assertion as a behavior statement; re-express at the **contract level**, not literal output shape.
- claude-relay's canonical target was OpenAI **Chat**; llmconduit's is canonical **Responses**. Behaviors are mapped to llmconduit's equivalent contract (e.g. "merge system into a system string" → Anthropic: system → `instructions`; Chat: system → system-role item in `input`).
- Each behavior is classified: **GREEN** (ported, passes), **PARTIAL** (`#[ignore="GAP:…"]` near-miss stub), **GAP** (whole feature absent → catalogued here, no stub), **DROP** (Python-only), **COVERED** (already tested in `gateway.rs`, not re-ported).

## Source inventory

173 behaviors across 9 pytest modules.

|Surface|Src#|Verdict summary|
|-|-|-|
|request-translation|17|mostly GREEN; output_config-effort = PARTIAL|
|response-translation|23|block-balance GREEN; reasoning-promotion heuristics = GAP|
|streaming-sse|16|SSE parsing = dep (`eventsource-stream`); 1MB DoS cap = GAP|
|error-mapping|9|GAP (no context-window-limit retry)|
|tool-call-translation|2|GREEN / COVERED|
|backend-routing|22|client-wins/case-insensitive GREEN; Kimi/DeepSeek family = GAP/PARTIAL|
|config-loading|7|profile-resolution GREEN; glob/template_family/CLI-route = GAP|
|debug-logging|8|GAP (no log rotation)|
|image-agent|47|GAP (entire surface absent)|
|server-http|20|route existence GREEN/COVERED; ctx-token-cap/peek = GAP|
|python-only|2|DROP|

---

## GAP CATALOG (whole features llmconduit lacks)

Ranked by estimated value to llmconduit. Each row: feature → claude-relay source tests → why absent → priority.

### G1 — Context-window-limit retry  ·  surface: error-mapping  ·  PRIORITY: HIGH  ·  ✅ RESOLVED `ebe6b41`
claude-relay parsed upstream 4xx/5xx bodies ("cannot be greater than max_model_len", vLLM "maximum context length", OpenAI "X in messages, Y in completion", "requested token count exceeds") to **retry with a reduced `max_completion_tokens`**, honoring a `min_completion_tokens` floor and a larger margin for "at least" boundary errors. Unrelated errors → no retry.
- Source: `test_backend.py::test_non_200_retry_*` (9 tests).
- llmconduit: `AppError` surfaces upstream failures verbatim (`src/error.rs`, `upstream.rs`). No body parsing, no token-budget retry. Failover is provider-level (pre-first-chunk), not context-shrink.
- Why HIGH: local vLLM/llama.cpp backends routinely 400 on context overflow; transparent shrink-and-retry is real resilience value for the gateway's target deployment.

### G2 — Backend model-family reshaping (Kimi / DeepSeek)  ·  surface: backend-routing  ·  PRIORITY: MEDIUM  ·  ✅ RESOLVED `d1e626e`
claude-relay detected the resolved backend model family and injected family-specific `chat_template_kwargs`: Kimi → `{thinking:true, preserve_thinking:true}` always (even when client inactive, to stop reasoning leakage via the identity parser) and reshaped nested assistant `thinking{}` → flat `reasoning_content`; DeepSeek → `{reasoning_effort, enable_thinking:true}`; a `template_family` route override forced a path regardless of model name; resolved model beat stale config; sampling defaults (`model_sampling`) injected by model with client params winning.
- Source: `test_backend.py` (~12 tests: kimi_*, deepseek_*, template_family_*, resolved_*, sampling_*).
- llmconduit: PARTIAL. Has Kimi/vLLM **sentinel cleanup** and nested-`thinking` parsing in `chat_to_responses.rs`, and per-model `model_profiles`/`upstream_chat_kwargs` with "explicit request wins" semantics (≈ `model_sampling` + client-wins). MISSING: automatic family **detection** + family-specific kwargs injection, `template_family` override, the always-on Kimi `thinking=true` reshape.
- See PARTIAL P2/P3 for the pieces that ARE testable.

### G3 — Server-side context budgeting + reasoning-aware peek  ·  surface: server-http  ·  PRIORITY: MEDIUM  ·  ✅ RESOLVED `41d7428` (budgeting) + `50720eb` (peek: redundant w/ G8+axum, contracted via tests)
claude-relay computed a fixed **128-token completion margin**, capped `max_completion_tokens` to `(context_limit − input − 128)`, raised `ContextWindowError` when input ≥ context, and ran `_peek_with_keepalive` to buffer reasoning-only streams until first visible content / `[DONE]` (with `count_reasoning` toggling whether thinking counts as visible).
- Source: `test_server.py` (~12: completion_token_margin, cap_max_completion_tokens_*, peek_*, delta_has_visible_output_* partly).
- llmconduit: no pre-flight token capping (delegates to upstream); streams progressively without a reasoning-buffering peek. `delta_has_visible_output` logic conceptually exists inside `StreamState` but isn't an exposed/contracted behavior.
- Why MEDIUM: pairs with G1; keep-alive peek is a streaming-UX nicety, lower urgency.

### G4 — Image agent (vision offload)  ·  surface: image-agent  ·  PRIORITY: LOW (for current design)  ·  ✅ RESOLVED `0a5ba94`
claude-relay offloaded images to a vision model: a per-session LRU+TTL `ImageCache`, `has_images` detection (last user message + tool_result arrays), `strip_and_cache_images` replacing images with `[Image #N]` placeholders + an `analyzeImage` tool, system-prompt injection, dedup of the injected tool, multi-turn stateless replay, and gating (skip for native-vision Kimi, skip without `vision_url`, skip when disabled/no-images).
- Source: `test_image_agent.py` (38) + `test_server.py` image-gating (5) + multi-turn (1) = 47 behaviors — **largest single gap**.
- llmconduit: deliberately ABSENT. `ToolSpec::ImageGeneration` is accepted in wire types but **stripped before upstream** (`responses_to_chat.rs` `lower_tools`); comment: "Client-side MCP servers handle image generation via function tools." No image cache, no vision-offload agent.
- Why LOW: architectural choice — llmconduit pushes multimodal/vision to the client or a vision-capable upstream, not an in-proxy agent. Re-introducing this is a feature project, not a bug.

### G5 — Debug-dump file rotation  ·  surface: debug-logging  ·  PRIORITY: LOW-MEDIUM  ·  ✅ RESOLVED `b610a53`
claude-relay's `_cleanup_debug_files` deleted `*.json`/`*.ndjson` dumps older than `max_age_hours`, kept recent, skipped other extensions + subdirectories, tolerated missing dir (→0) and concurrent-deletion `OSError`.
- Source: `test_debug_rotation.py` (8 tests).
- llmconduit: `upstream_request_log_path` is **append-only JSONL** (`upstream.rs`); `redact_payload_secrets` exists (`http.rs`) but there is NO age-based cleanup/rotation → unbounded growth.
- Why LOW-MEDIUM: operational hygiene; mitigated by external logrotate, but in-proc rotation would match claude-relay.

### G6 — SSE per-frame buffer cap (DoS guard)  ·  surface: streaming-sse  ·  PRIORITY: MEDIUM  ·  ✅ RESOLVED `881cfe1`
claude-relay capped SSE frame assembly at a configurable `max_buffer_bytes` (default 1 MB), rejecting oversized/unterminated frames before memory exhaustion.
- Source: `test_sse.py` (buffer_overflow, exactly_at_limit, just_over_limit, oversized_unterminated, custom_max_buffer = ~5 of 16).
- llmconduit: uses `eventsource-stream` for upstream SSE parsing and a 256 MiB **request body** limit at the HTTP layer (`http.rs`). No per-SSE-frame ceiling appears on the upstream-read path. (ASSUMPTION — `eventsource-stream` internals were not source-audited; confirm before treating as a confirmed gap.)
- Why MEDIUM: a hostile/buggy upstream could stream an unterminated multi-hundred-MB frame; the body limit doesn't cover the upstream-response path.

### G7 — Config: glob routes / template_family / CLI --model-route  ·  surface: config-loading  ·  PRIORITY: LOW  ·  ✅ RESOLVED `5dceac6`
claude-relay routed by `model_routes` (name→url+upstream), supported glob route keys (`claude-opus-*`), a `template_family` config field (default "auto"), TOML config, and a `--model-route "name=url,upstream"` CLI flag.
- Source: `test_config.py` (glob, cli_spec, template_family, toml) (~4 of 7).
- llmconduit: routes by `canonical_model_key` normalization + exposed-alias over a YAML `upstreams` catalog; no globs, no `template_family`, no per-route CLI flag.
- Why LOW: different-by-design routing model. The remaining `test_config.py` behaviors (exact-before-default, fall-back-to-default) ARE ported GREEN via profile/upstream resolution.

### G8 — Reasoning promotion / suppression heuristics  ·  surface: response-translation  ·  PRIORITY: MEDIUM  ·  ✅ RESOLVED `8297ca6`
claude-relay's Chat→Anthropic stream had nuanced reasoning handling: reasoning-only stream **promoted to text** at `finish_reason:stop` but **kept as thinking** at `finish_reason:length`; `emit_thinking=False` suppressed thinking blocks (still promoting at stop); late reasoning after text was dropped; signature-bearing thinking never promoted.
- Source: `test_convert_stream.py` (~8 of 23: reasoning_only_promoted_*, _length_truncated_not_promoted, emit_thinking_false, late_reasoning_*, signature_*).
- llmconduit: `responses_to_anthropic.rs` maps reasoning → thinking and has `finalize()` for synthetic terminal events, but does NOT implement the stop-vs-length promotion split or an `emit_thinking` suppression toggle (it estimates progressive usage instead).
- See PARTIAL P4 for the block-balancing pieces that ARE ported GREEN.

---

## PARTIAL / NEAR-MISS  (`#[ignore="GAP:…"]` stubs — small deltas, actionable)

|ID|Stub test|File|Delta llmconduit would need|
|-|-|-|-|
|P1 ✅ `1faba60`|`anthropic_output_config_effort_maps_to_reasoning_effort`|port_translation.rs|route `output_config.effort` (+adaptive thinking) → `reasoning.effort`; today effort comes only from `thinking`|

(P2/P3/P4, referenced by G2/G8, were never enumerated as separate stubs — they folded into the full G2/G8 implementations.)

---

## GREEN (ported, passing)

### request-translation — `tests/port_translation.rs` (13 green, 1 ignored)
|Test|Behavior confirmed|
|-|-|
|anthropic_top_k_is_rejected|`top_k` → 400|
|anthropic_system_string_becomes_instructions|system string → `instructions`|
|anthropic_system_blocks_join_into_instructions|system blocks joined → `instructions`|
|anthropic_stop_sequences_move_to_extra_body|`stop_sequences` → `extra_body.stop`|
|anthropic_max_tokens_maps_to_max_output_tokens|`max_tokens` → `max_output_tokens`|
|anthropic_forces_parallel_tool_calls_false|`parallel_tool_calls` forced false|
|anthropic_output_config_json_schema_maps_to_text|json_schema format → `text` controls|
|anthropic_output_config_non_json_schema_rejected|non-json_schema format → 400|
|anthropic_output_config_without_format_is_noop|no format → no `text`|
|chat_instructions_always_empty_system_stays_in_input|Chat `instructions` empty; system → input item|
|chat_reasoning_effort_maps_to_reasoning|`reasoning_effort` → `reasoning.effort`|
|chat_tool_choice_defaults_to_auto|missing tool_choice → `"auto"`|
|chat_unknown_knob_round_trips_through_extra_body|unknown key → `extra_body`|

---

## COVERED (already tested in `gateway.rs` — not re-ported)
- Single + parallel tool-call streaming (`streams_function_call_turn` et al.) ← tool-call-translation src tests.
- Server routes `/v1/responses|messages|chat/completions|completions|models|health|/` existence + probes.
- Failover pre-first-chunk, routing by model id / exposed alias, replay prefix match.

## DROP (Python-only)
- `test_compat.py::test_old_package_imports_alias_new_modules`, `test_old_entrypoint_imports_new_main` — module-rename import aliases; no Rust analogue.

---

## Priority summary — ✅ ALL RESOLVED (was the "next project"; now done)
Implementation order ran HIGH→LOW; every item below shipped + Codex-xhigh APPROVED. Commits in each heading above and in `.ralph/IMPLEMENTATION_PLAN.md`.
1. ✅ **G1 context-window retry** (HIGH) — resilience on local backends.
2. ✅ **G2 model-family reshaping**, **G3 context budgeting**, **G6 SSE buffer cap**, **G8 reasoning promotion** (MEDIUM).
3. ✅ **G5 log rotation** (LOW-MEDIUM).
4. ✅ **G4 image agent**, **G7 glob/template/CLI config** (LOW — design divergence, not bugs).
