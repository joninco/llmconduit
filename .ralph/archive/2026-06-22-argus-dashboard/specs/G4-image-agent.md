# G4 — Image agent (vision offload)

> **Status: SHIPPED** — historical design input. Acceptance criteria below are satisfied by the implemented code/tests; see `.ralph/IMPLEMENTATION_PLAN.md` (Task for this gap) for the final design. Open questions/verify-first notes are resolved.

**Priority:** LOW (largest gap, 47 behaviors) · **Surface:** engine / new vision seam · **GAPS.md:** G4
Owner directed implementation despite the "deliberately absent" architectural note. Port claude-relay's
in-proxy vision offload to llmconduit's canonical-Responses pipeline WITHOUT violating the engine hard rules.

## Reference (study, adapt — do NOT transliterate)
- claude-relay: `/home/jon/git/claude-relay/claude_relay/` — `ImageCache` (LRU+TTL), `has_images`,
  `strip_and_cache_images` (`[Image #N]` placeholders + `analyzeImage` tool), system-prompt injection,
  tool dedup, multi-turn stateless replay, gating.
- Tests: `test_image_agent.py` (38) + `test_server.py` image-gating (5) + multi-turn (1) = 44.

## Architecture (from Codex-xhigh design plan, code-grounded)
Implement as a canonical **pre-lowering transform** + a **provider-tool executor**:
1. **Strip/cache seam = `Gateway::stream_responses`**, AFTER model resolution + `apply_system_prompt_prefix`,
   BEFORE replay lookup/lowering (`engine.rs:459-495`). Single mutation surface on `ResponsesRequest.input`;
   keeps replay hashes on placeholder text (not image bytes); `lower_request` sees only placeholders + the tool.
2. **New `Arc<dyn VisionClient>` seam** in `src/vision.rs`, mirroring `SearchClient` (`search.rs:25-28`):
   `VisionClient`, `VisionRequest`, `VisionOutcome`, `CachedImage`, `ImageCache`, detection/strip helpers,
   prompt/tool-schema constants, `ReqwestVisionClient`. Construct in `lib.rs` beside `BraveSearchClient`,
   pass into `Gateway::new`.
3. **Inject exactly one `analyzeImage` tool** as a new server-side `ToolKind::ImageAnalysis`
   (`responses_to_chat.rs:20-32` + registry/lowering `:445-615`), classified server-side ONLY on active
   image-agent turns. Dedup any caller-supplied `analyzeImage`.
4. **Generic server-tool dispatcher**: extend `handle_tool_calls` (`engine.rs:1235-1244`) to classify ALL
   server-runnable calls (`web_search` + `analyzeImage`) BEFORE the mixed client/server rejection decision
   (`engine.rs:1290-1357`). `parallel_tool_calls:false` stays (`engine.rs:929-936`); if multiple server calls
   arrive, execute SEQUENTIALLY with a per-tool round ceiling (do NOT touch `WEB_SEARCH_ROUNDS_HARD_CEILING`).
5. **`run_image_analysis`** mirrors `run_web_search` (`engine.rs:1620-1772`): parse `imageId`/`task`/`context`,
   resolve cached image, call `VisionClient` under `request_timeout` + cancellable via `tx.closed()`,
   inject success/error as a model-visible chat `tool` message; internal error only for impossible state.
   Do NOT add the internal call/result to public `response_output`.
6. **Cache = separate from `ReplayStore`** (replay = SHA256 over `(model,instructions,input)`, no TTL,
   `replay.rs:41-87`). `ImageCache` = LRU+TTL keyed by `(session_id, image_id)`, cleared/repopulated when
   strip runs so multi-turn placeholder numbering resets like claude-relay.
7. **Reuse wire types**: `ContentItem::InputImage` (`responses.rs:184-193`), Chat image normalization
   (`chat_completions.rs:186-210`), Anthropic (`anthropic_to_responses.rs:325-349,:671-711`). No new
   non-canonical adapters. Prefer hiding `analyzeImage` as an internal server tool (do not surface it).
8. **Hide `analyzeImage`** in Chat + Anthropic converters like `web_search_call` is hidden
   (`chat_completions.rs:734-752`, `responses_to_anthropic.rs:270-309`).

## Config (`config.rs` + profile chain + env overrides)
`image_agent_enabled`, `vision_url`, `vision_model`, `image_cache_max_size`, `image_cache_ttl_secs`,
profile-level `native_vision: Option<bool>`.

## Gating — use image agent ONLY when ALL true
enabled · `vision_url` set · latest canonical user message has ≥1 `InputImage` · resolved/profiled backend
is NOT native-vision. Skip (no strip, no tool reserve) when: disabled · no images in latest user turn ·
missing `vision_url` · native-vision (Kimi / profile `native_vision` override) · `tool_choice == "none"`.
Gate AFTER resolved-model/profile resolution, not on the raw request model.

## Acceptance criteria (executable)
- `src/vision.rs` unit tests: session isolation, LRU eviction, TTL expiry, last-user detection, old-image
  skip, placeholder ordering, tool dedup, system-prompt injection, malformed/empty content preserved,
  multi-turn numbering reset.
- `tests/gateway.rs` (+ `MockVisionClient`): success 2-upstream-call loop, multiple image ids, cache
  miss/error/timeout → injected tool text, cancellation drops vision work, NO raw image bytes in upstream
  text-backend request, NO `analyzeImage` leak in Chat/Anthropic output.
- Gating tests: Kimi/native skip, DeepSeek/text-only use, missing-url skip, disabled skip, no-image skip,
  profile `native_vision` override, resolved-alias gating.
- Hard-rule tests: `parallel_tool_calls` stays false; client+`analyzeImage` rejected; `web_search`+client
  still rejected; `analyzeImage`+`web_search` server-only batch handled sequentially; forced `tool_choice`
  relaxes to auto after a server tool; image-analysis round ceiling enforced.

## Top review risks to pre-empt
1. Mixed-tools regression — centralize tool classification; test every server/client combo.
2. Replay pollution with image bytes — preprocess before replay; store only placeholders in visible history.
3. Internal tool leakage — suppress `analyzeImage` deltas/items in Chat/Anthropic; keep out of `response_output`.
4. Infinite loops — add image-analysis round limit WITHOUT changing the web-search hard ceiling.
5. Wrong native-vision stripping — gate after resolved-model/profile resolution.

## Definition of Done
Tests green · `cargo test` whole suite green · `cargo clippy --all-targets` clean · `cargo fmt` ·
**Codex-xhigh review APPROVED** (`.ralph/REVIEW_PROTOCOL.md`) · commit. Obey AGENTS.md hard rules.
