# OpenAI Responses API and Codex Full-Compatibility Implementation Plan

## Summary

Bring llmconduit from “compatible with limitations” to standards-conformant OpenAI Responses behavior for the audited surface, while preserving Chat Completions, Anthropic Messages, server-side tools, routing, replay safety, and cancellation guarantees.

Audit baseline:

- Running llmconduit: `0.1.0`, commit `fe62a432a545cfa210d0b49e5a9c12dab3b9af4c`.
- Audited Codex: `0.144.4`, source commit `8c68d4c87dc54d38861f5114e920c3de2efa5876`.
- Active model: `GLM-5.2-NVFP4`.
- The existing 1,197-test baseline must remain green.
- Preserve all pre-existing uncommitted files, including `src/bin/backfill_prices.rs`.

This is an implementation and regression plan, not deployment authorization. It must not edit the live configuration, restart the service, install a binary, or deploy without separate approval.

## Locked decisions

- Implement every confirmed llmconduit gap from the audit; do not patch Codex or vLLM.
- Keep `/v1/models` OpenAI-shaped. Supply Codex’s private metadata through a checked `model_catalog_json` example.
- Accept official request forms plus existing legacy ingress aliases; emit only standard Responses output.
- Implement `previous_response_id` using a bounded memory store by default and optional SQLite persistence.
- Give `store` only Responses persistence semantics. Decouple private replay, disable it by default, and expose separate replay configuration.
- Reject known unsupported capabilities with parameter-specific OpenAI 400 errors; never silently ignore them.
- Retain non-colliding vendor extensions through `extra_body`, but prevent typed-field collisions and duplicate serialized keys.
- For failover, keep the selected primary and remove incapable fallback candidates for that request. Never reroute to a different routing provider merely to obtain a capability.
- Default request logging to metadata only.
- Permit unauthenticated loopback serving. Require an environment-only API token or explicit insecure override for non-loopback binds.
- Do not semantically deduplicate function calls. Repeated GLM/vLLM calls with distinct upstream identities remain distinct.

Hosted OpenAI services such as Files, Conversations, Code Interpreter, and hosted Computer Use are outside scope. Their request forms must receive explicit unsupported-parameter errors unless backed by an existing llmconduit implementation.

## Public contracts and configuration

### Responses request contract

Refactor `src/models/responses.rs` into a standards-first request model with custom normalization at ingress:

- Accept string `input`, typed messages, and easy messages with omitted `type` or string `content`.
- Preserve `developer`, `system`, `user`, and `assistant` roles. Accept legacy `tool` messages only as an alias that can be unambiguously normalized to function output.
- Support canonical message content containing `input_text`, `input_image`, and `input_file`; capability-gate the latter two.
- Support function calls, function outputs, reasoning items, stored item references, and continuations containing encrypted reasoning when configured.
- Support both official flat function selection, `{type:"function",name:"…"}`, and the existing nested Chat-style alias.
- Make function descriptions optional. Validate function names, tool uniqueness, JSON Schema syntax, and the supported strict-schema subset.
- Support `text.format` variants `text`, `json_object`, and `json_schema` with variant-specific required fields.
- Validate bounds and types for metadata, temperature, top-p, output limits, reasoning controls, truncation, service tier, and parallel calls before dispatch.
- Require a valid Responses model or configured exposed alias. Missing model returns 400; an explicit unknown model returns 404. Preserve existing default-model behavior on Chat and Anthropic ingress.
- Known unsupported standard fields or item/tool variants return `unsupported_parameter` errors naming the exact JSON path.
- Unknown non-colliding top-level vendor fields remain in `extra_body`. Reject collisions with typed fields and remove consumed llmconduit extensions before upstream serialization.

### Public Response resource and SSE contract

Introduce a single lifecycle/state-machine component used by streaming and non-streaming paths:

- Mint `response.id` and `created_at` once at turn creation.
- Own stable response, message, reasoning, function-item, and call IDs.
- Assign zero-based output/content indexes in first-appearance order.
- Add a zero-based, strictly increasing `sequence_number` to every public Responses SSE event.
- Produce complete `created`, `in_progress`, `completed`, `incomplete`, and `failed` Response snapshots with official nullability and field names.
- Build non-streaming output from the exact terminal lifecycle resource rather than a separate collector.
- Strip gateway-only fields and events—including `terminal_reason`, `stop_sequence`, synthetic signatures, and `response.web_search_results`—only at raw Responses egress. Keep them available to internal Chat and Anthropic converters.
- End a stream with exactly one terminal event followed by EOF.
- Preserve upstream token totals exactly. Populate cached- and reasoning-token details when reported; use schema-required zero values when unavailable without inferring counts, and retain an internal “unreported” quality marker.
- Map `length` and `content_filter` to `incomplete` with the correct reason. Tool-call handoff and normal stops remain completed.
- Persist an eligible stored response before emitting its terminal event so it is referenceable as soon as the client receives completion. A persistence failure makes the response fail instead of falsely advertising stored state.

### Stateful Responses and replay

Add `src/response_store.rs` with a `ResponseStore` trait, a memory implementation, and a SQLite implementation:

```yaml
response_store:
  backend: memory       # memory | sqlite
  path: null            # required for sqlite
  max_entries: 1000
  retention_hours: 720  # 30 days

replay:
  enabled: false
  max_entries: 100
```

State semantics:

- `store:true` stores the completed or incomplete canonical history; `store:false` never does.
- Failed and cancelled responses are not referenceable.
- Stored data includes canonical prior input/output items, stable item IDs, requested and served model, timestamps, and expiry—but never HTTP headers or credentials.
- `previous_response_id` prepends the stored canonical chain to the new input. Current instructions replace rather than inherit previous instructions; an explicit current model wins.
- Missing, expired, evicted, failed, or non-stored IDs return a sanitized 404 naming `previous_response_id`.
- Callers may continue resending full history without using response state. No semantic deduplication is applied if they combine both mechanisms.
- Memory uses TTL-aware LRU eviction. SQLite uses a versioned schema, transactional writes, startup/on-access expiry cleanup, restrictive directory/file permissions, and non-blocking Tokio integration.
- SQLite mode keeps a bounded memory front cache but treats SQLite as authoritative across restarts.
- Replay is independent of `store`. Add the nonstandard boolean `llmconduit_replay` request extension only as a per-request bypass/allow hint when server replay is enabled; remove it before upstream dispatch.
- Retain image-degradation replay bypass and the existing visible-history hash invariants.

This intentionally supersedes the former `AGENTS.md` rules that rejected `previous_response_id` and overloaded `store`; update those rules when implementation lands.

### Capability registry

Add conservative Responses capabilities resolved by `(upstream, served model)` rather than model name alone:

```yaml
responses_capabilities:
  parallel_tool_calls: false
  structured_outputs: [text, json_object, json_schema]
  reasoning_summary: upstream       # upstream | unsupported
  encrypted_reasoning: unsupported  # passthrough | unsupported
  input_image: placeholder          # native | agent | placeholder | reject
  input_file: unsupported           # native | unsupported
  truncation_auto: unsupported      # upstream | unsupported
  text_verbosity: unsupported       # upstream | unsupported
  service_tiers: []
  prompt_cache_key: gateway_hash    # gateway_hash | upstream | unsupported
  prompt_cache_retention: []
```

Rules:

- Omitted capability metadata means unsupported, except baseline text, ordinary functions, numeric sampling controls, and disabled truncation.
- Validate capabilities after primary routing. If the primary lacks a requested capability, return 400. If a fallback lacks it, prune that fallback without cooldown or health penalty.
- `prompt_cache_key: gateway_hash` validates and hashes the opaque key for local cache-affinity/replay metrics without logging it. `upstream` additionally forwards it.
- Prompt-cache retention, service tier, text verbosity, encrypted reasoning, and automatic truncation are accepted only when declared.
- Echo the actual service tier in the Response resource.
- Never derive a reasoning summary from hidden chain-of-thought. Emit only a provider’s explicit safe summary channel.
- Native images may pass only to native-vision providers. Agent and placeholder modes preserve existing behavior; reject mode fails before dispatch. Raw images must never reach a non-native backend.
- Unsupported `input_file` fails before dispatch instead of becoming an upstream 502.
- Raw Responses requests for unsupported hosted image-generation tools fail explicitly; retain downstream stripping as a defense-in-depth invariant.

### API security and logging

Add environment-only controls:

- `LLMCONDUIT_API_TOKEN`: accepted through Bearer authorization or `x-api-key`, using constant-time comparison.
- `LLMCONDUIT_ALLOW_UNAUTHENTICATED_API=1`: explicit non-loopback development override.

Protect all `/v1/*` routes while leaving `/` and `/health` public. Dashboard authentication remains separate. Refuse startup on wildcard or non-loopback binds without a token or the explicit override.

Add logging modes defaulting to metadata:

```yaml
api_log_body_mode: metadata                 # metadata | redacted_payload
upstream_request_log_body_mode: metadata    # metadata | redacted_payload
```

Metadata mode records IDs, status, sizes, hashes, timing, and bounded error classifications only. Payload mode still uses the shared secret and image-URI redactors. Malformed JSON is represented only by a hash/length marker. Existing turn capture remains separately opt-in.

For `/v1/completions` and all other proxy paths, replace header blocklists with a narrow forwarding allowlist. Never forward inbound `authorization`, `x-api-key`, `api-key`, cookies, proxy credentials, or dashboard/session headers.

## Implementation sequence

### 1. Pin the protocol and build the conformance harness

- Add a focused `tests/responses_conformance.rs` and fixtures derived from the official Responses documentation as of 2026-07-14.
- Record the documentation date and expected event/resource schemas in the fixture README.
- Convert the reusable `/tmp/llmconduit-live-audit` probes into deterministic tests where possible; keep live/Codex execution opt-in.
- Run and record the existing 1,197-test baseline before changing behavior.
- Replace tests that currently bless defective behavior instead of merely adding parallel expectations.

### 2. Normalize requests, errors, models, and logging

Primary files: `src/models/responses.rs`, `src/adapters/responses_to_chat.rs`, `src/http.rs`, `src/error.rs`, and `src/upstream.rs`.

- Implement the official/legacy request unions and typed validation.
- Centralize surface-specific OpenAI errors with `{message,type,param,code}` while retaining Anthropic error rendering.
- Convert JSON extraction, content-type, and body-limit rejections to JSON errors.
- Normalize intrinsic upstream 400/413/415/422 responses without exposing raw bodies.
- Normalize `/v1/models` entries to stable OpenAI model objects with `id`, `object`, `created`, and `owned_by`; retain useful additive context metadata and standard pagination. Do not emit Codex’s private `{models:[…]}` shape.
- Make explicit unknown models fail on Responses ingress.
- Prevent typed/flattened key duplication with explicit request fields winning.
- Apply metadata-only logging and shared redaction to malformed-body and upstream-request paths.

### 3. Introduce the canonical lifecycle and public projection

Primary files: `src/models/responses.rs`, `src/engine.rs`, and a focused public projection module under `src/adapters/`.

- Create the lifecycle object before upstream dispatch.
- Route all item creation, deltas, completion, usage, status, and terminal-resource assembly through it.
- Add event sequencing and full Response snapshots.
- Make streaming/non-streaming collectors share the terminal resource.
- Implement complete message/output-text and refusal lifecycles.
- Correct incomplete and failed semantics.
- Preserve internal-only events until the final raw Responses projection so Chat and Anthropic behavior remains intact.

### 4. Repair tools and reasoning

Primary files: `src/adapters/chat_to_responses.rs`, `src/adapters/responses_to_chat.rs`, and `src/engine.rs`.

Function lifecycle:

1. Allocate/register the function item at its first upstream appearance.
2. Emit `response.output_item.added`.
3. Emit argument deltas carrying stable `item_id` and `output_index`.
4. Validate accumulated JSON and strict schema.
5. Emit `response.function_call_arguments.done`.
6. Emit `response.output_item.done` with matching item identity and final arguments.

Use upstream index first, then stable upstream call ID. A fragment lacking both identity forms is acceptable only when one call is unambiguously open; ambiguous parallel fragments fail rather than merging.

Reasoning lifecycle:

1. Emit reasoning `output_item.added`.
2. Emit summary-part added.
3. Emit summary-text deltas.
4. Emit summary-text done.
5. Emit summary-part done.
6. Emit reasoning `output_item.done` with the same summary content.

Only expose opaque encrypted reasoning when requested through `include` and supported. Keep Anthropic signatures internal. Validate malformed tool arguments before item completion; stream failure rather than presenting an executable completed call. Preserve function error outputs as ordinary model-visible continuation data.

Do not suppress duplicate semantic calls. Add diagnostic counters comparing raw and served call identities without altering behavior.

### 5. Add state, structured output, caching, and multimodal policy

- Implement the response store, memory LRU, SQLite persistence, expiry, and atomic terminal storage.
- Lower stored canonical histories through the same adapter as explicit histories.
- Decouple and disable replay; migrate existing replay tests and configuration.
- Implement `text`, `json_object`, and `json_schema` lowering. Validate schemas at request time and final generated JSON/schema compliance at termination.
- Implement explicit `include` handling for gateway-supported encrypted reasoning and web-search source data; reject unsupported include values.
- Forward or locally apply service-tier, prompt-cache, verbosity, and truncation fields only according to capability declarations.
- Preserve exact context/output limits from the model catalog. Reject impossible limits before dispatch when determinable; translate provider validation errors otherwise.
- Gate image and file behavior before upstream selection. Preserve the no-raw-image and degraded-image replay protections.

### 6. Correct timeout, failover, cancellation, and status translation

- Wrap request send/response-header acquisition in the configured upstream timeout.
- Keep connect timeout and per-chunk idle timeout distinct; do not impose a total deadline on healthy long streams.
- Make intrinsic 400/413/415/422 terminal without cooldown or fallback.
- Preserve existing pre-first-chunk-only failover for retryable connect, timeout, 408, 429, and 5xx failures.
- Never retry malformed streams or failures after the first served chunk.
- Maintain bounded channels and `tx.closed()` selection around upstream reads, tool execution, persistence, and event sends.
- Map exhausted connect/5xx failures to sanitized 502, header/idle timeouts to 504, rate limits to 429, and backend credential/configuration failures to internal 502 rather than client-authentication errors.
- Ensure cancelled responses are neither stored nor replayed and leave no live worker task.

### 7. Complete Codex integration assets

Add `docs/examples/codex-model-catalog.glm-5.2-nvfp4.json` and documentation showing:

- `wire_api = "responses"`.
- The standard `http://127.0.0.1:5022/v1` base URL.
- A dedicated `env_key = "LLMCONDUIT_API_TOKEN"`.
- `model_catalog_json` pointing to the checked catalog.
- An isolated `CODEX_HOME` alternative for environments affected by global OpenAI credential forwarding.

The GLM catalog entry must be validated by the installed Codex schema and advertise only verified behavior:

- Context window `524288` with the Codex effective-input percentage explicitly set.
- Text input only for the active non-native-vision configuration.
- Reasoning summaries enabled only after the conformant lifecycle lands.
- Parallel tool calls disabled for this model because repeated calls originate in GLM/vLLM.
- Verbosity disabled unless mapped and tested.
- No service tiers unless an upstream tier is verified.

Do not change `/v1/models` to silence Codex’s private-schema warning. The catalog is the compatibility seam.

## Required regression tests

### Request and model behavior

- `responses_accepts_string_typed_and_easy_message_inputs`
- `responses_preserves_developer_user_assistant_and_tool_history`
- `responses_tool_choice_accepts_flat_and_legacy_forms`
- `responses_function_description_is_optional`
- `responses_text_format_union_round_trips`
- `responses_validates_metadata_sampling_and_output_limits`
- `responses_known_unsupported_fields_name_the_parameter`
- `responses_unknown_vendor_fields_round_trip_without_key_collisions`
- `responses_missing_and_unknown_models_return_openai_errors`
- `models_normalizes_minimal_vllm_catalog`
- `codex_catalog_example_decodes_with_current_schema`

### Resource and SSE conformance

- `responses_full_resource_stream_nonstream_equivalence`
- `responses_text_sse_lifecycle_is_ordered`
- `responses_events_have_strict_sequence_numbers`
- `responses_ids_indexes_and_created_at_are_stable`
- `responses_terminal_event_occurs_exactly_once`
- `responses_length_and_content_filter_are_incomplete`
- `responses_failed_event_contains_full_resource`
- `responses_internal_fields_never_reach_public_wire`
- `responses_usage_matches_upstream_totals_and_details`
- `responses_refusal_lifecycle_is_conformant`

### Function and reasoning lifecycle

- `responses_function_call_sse_lifecycle_single`
- `responses_function_call_sse_lifecycle_parallel`
- `responses_parallel_fragments_without_identity_fail`
- `responses_duplicate_calls_with_distinct_ids_remain_distinct`
- `responses_function_output_continuation_multiturn`
- `responses_strict_tool_arguments_validate_against_schema`
- `responses_malformed_arguments_fail_before_item_done`
- `responses_tool_error_output_continues_normally`
- `gateway_owned_tools_remain_sequential`
- `client_tools_honor_parallel_tool_calls`
- `responses_reasoning_summary_lifecycle_is_complete`
- `responses_encrypted_reasoning_requires_include_and_capability`
- `responses_reasoning_usage_is_not_inferred`

### Stateful and advanced behavior

- `responses_previous_response_id_continues_memory_history`
- `responses_previous_response_id_survives_sqlite_restart`
- `responses_store_false_is_not_referenceable`
- `responses_expired_evicted_and_unknown_ids_return_404`
- `responses_previous_instructions_are_not_inherited`
- `responses_concurrent_children_read_the_same_parent_safely`
- `replay_is_decoupled_from_store_and_disabled_by_default`
- `responses_structured_output_supports_all_three_formats`
- `responses_invalid_structured_output_fails`
- `responses_prompt_cache_key_is_hashed_and_never_logged`
- `responses_service_tier_is_gated_and_echoed`
- `responses_truncation_auto_is_capability_gated`
- `responses_input_file_rejects_before_upstream`
- `responses_non_native_images_never_reach_upstream`
- `responses_capability_pruning_removes_only_incapable_fallbacks`

### Errors, reliability, and security

- Add table-driven malformed JSON, wrong media type, wrong field type, 413, invalid sampling, unsupported field, unknown model, upstream 4xx/5xx, and internal-error tests asserting status, JSON content type, all four OpenAI error fields, and no raw body leakage.
- Add two-provider intrinsic 400/413/415/422 tests asserting zero fallback calls and no cooldown.
- Add separate 408/429/5xx tests proving retryable pre-first-chunk failover remains intact.
- Add a TCP server that accepts a request but never sends headers, tested against bare and failover clients.
- Add malformed upstream SSE tests before and after the first chunk, proving safe failover only before output.
- Add sentinel-secret tests across tracing, upstream JSONL, turn capture, malformed-body diagnostics, and client errors.
- Add header-forwarding tests covering authorization, `x-api-key`, `api-key`, cookies, proxy credentials, and benign allowlisted headers.
- Add startup/auth tests for loopback, wildcard bind refusal, valid Bearer, valid `x-api-key`, invalid token, and explicit insecure override.
- Add concurrent streaming, bounded slow-reader backpressure, client cancellation, request-size limit, and task/resource cleanup tests.
- Add retry tests proving no duplicated served items or function calls.

### Controlled Codex smoke

Run against an alternate llmconduit process and temporary read-only workspace:

- Exact plain-text response.
- One safe shell invocation and tool-output continuation.
- Two different sequential shell invocations.
- A multi-turn task using `previous_response_id`.
- Exact upstream → Responses → Codex usage equality.
- No Responses parsing warnings or missing-model-catalog warning.
- Expected nonfatal behavior for an explicitly requested unsupported service tier.
- No repeated calls introduced between raw upstream and served events.
- Dedicated provider authentication received by llmconduit but absent from upstream headers.
- Record prompt size and cache details without imposing a false low-token threshold.

## Completion gates and rollout

- `cargo fmt --check`
- `cargo clippy --all-targets`
- Full `cargo test`, including updated formerly nonconformant tests.
- Focused conformance tests pass against both in-process mocks and an alternate-port HTTP server.
- Controlled Codex smoke passes with sanitized artifacts under `/tmp`.
- Chat Completions and Anthropic conformance suites remain unchanged and green.
- No raw image reaches a non-native backend.
- No retry occurs after first output or for request-intrinsic 4xx.
- No credential or sentinel request secret appears in logs, captures, client errors, or upstream headers.
- Update `AGENTS.md` and user documentation for the new state, replay, capability, authentication, and logging contracts.
- Document the breaking deployment prerequisite: a production wildcard bind must receive `LLMCONDUIT_API_TOKEN` or the explicit insecure override before restart.
- Do not edit the live configuration, restart the service, install a binary, or deploy until separately authorized.

After presenting the audit or implementation result, stop and wait for approval before any deployment or live-system modification.
