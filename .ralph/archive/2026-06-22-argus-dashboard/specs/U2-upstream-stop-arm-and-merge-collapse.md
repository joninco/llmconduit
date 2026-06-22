# U2 — Upstream `stop` field-set arm + collapse duplicate chat-kwargs merge helpers

> **Source:** thermo-nuclear PROJECT review 2026-06-20 (Topic 12). See /tmp/thermo-project-review.md

**Priority:** HIGH · **Surface:** `src/upstream.rs`, `src/config.rs`, `tests/port_*.rs` · **Thermo finding:** missing `"stop"` field-set arm → duplicate wire `stop` key (Finding 2); near-identical merge fork hid it + leaks the max-token-alias collision on the fallback path (Finding 4)

## Purpose
`chat_request_field_is_set` (`src/upstream.rs:2268-2285`) has no `"stop"` match arm, so the typed `stop` field falls through to `_ => false`. The function is consulted by BOTH gap-fill helpers — leaf-finalize via `merge_upstream_chat_kwargs` (`upstream.rs:2055`, reached from `finalize_request_for_backend:2011`) and provider-fallback via `merge_fallback_chat_kwargs` (`upstream.rs:2256`, reached from `request_for_provider:810`). When a config `upstream_chat_kwargs.stop` default coexists with a typed client `stop` (`src/models/chat.rs:47`, threaded from `engine.rs:1408` as `normalized_stop`), the config value is treated as gap-fillable and inserted into the `#[serde(flatten)]` `extra_body` (`chat.rs:48`). Because the typed `stop` field serializes as the literal key `"stop"` (no `rename`), the real wire POST (`reqwest .json(request)` at `upstream.rs:573`) emits TWO `"stop"` keys — verified concretely: serde flatten produces `{"stop":["TYPED"],"stop":["CONFIG"]}`, and last-key-wins parsers keep the config value, silently dropping the client's `stop` — a direct violation of the request-wins hard rule (AGENTS.md: explicit request fields win over configured defaults). Separately, `merge_upstream_chat_kwargs` (`upstream.rs:2037-2065`) and `merge_fallback_chat_kwargs` (`upstream.rs:2251-2266`) are near-identical gap-fill helpers differing ONLY by the max-token-alias guard (`upstream.rs:2047-2054`) that the fallback variant LACKS; a provider `upstream_chat_kwargs.max_tokens` can then land alongside a surviving client max-token alias in `extra_body`, reproducing the alias collision the leaf path defends against — reachable via `/v1/responses` because `ResponsesRequest` types only `max_output_tokens`, so a client `max_completion_tokens` survives in the flattened `extra_body` and is threaded to the backend (`engine.rs:1409`). The fork is itself what let the missing `stop` arm hide; collapsing it closes the drift.

## Jobs to Be Done
- A typed client `stop` field is recognized as "already set" so a configured `upstream_chat_kwargs.stop` never gap-fills into `extra_body`, on BOTH the leaf-finalize and provider-fallback code paths.
- The real wire POST carries exactly ONE `"stop"` key — the client's typed value — with no duplicate key and no silent client-value drop.
- The two gap-fill helpers become ONE shared helper that ALWAYS applies the max-token-alias skip, so the alias-collision guard protects the provider-fallback path too (no-op when no alias is present).

## Acceptance criteria
- [ ] `chat_request_field_is_set` (`src/upstream.rs:2268-2285`) gains `"stop" => request.stop.is_some(),`, slotted alongside the other typed-field arms.
- [ ] `merge_upstream_chat_kwargs` (`upstream.rs:2037-2065`) and `merge_fallback_chat_kwargs` (`upstream.rs:2251-2266`) are collapsed into ONE helper (e.g. `merge_chat_kwargs_gap_fill(request, defaults)`) that ALWAYS applies the max-token-alias skip (`upstream.rs:2047-2054` logic), no-op when no alias is set; both call sites — leaf-finalize (`upstream.rs:2011`) and provider-merge (`upstream.rs:810`) — call the single helper, and the second helper definition is deleted.
- [ ] Request-wins is byte-identical on every other key: gap-fill semantics, deep-merge of nested objects (`merge_json_value_preserve_destination`), and the alias-skip behavior already exercised on the leaf path are unchanged.
- [ ] The config.rs strip list (`src/config.rs:349-352`) is NOT modified — no `stop` strip is added; the typed-field arm is the fix (the strip list is the separate appendix-LOW concern).
- [ ] New/rewritten test: a request with a config `upstream_chat_kwargs.stop` AND a typed client `stop` driven through the leaf-finalize path (`finalize_request_for_backend` / `merge_chat_kwargs_gap_fill`) asserts the client value survives, `extra_body` carries NO `"stop"` key, and `serde_json::to_value(&request)` (mirroring `reqwest .json`) yields exactly one `"stop"` equal to the client value.
- [ ] New/rewritten test: the same config-stop vs typed-stop assertion driven through the provider-fallback path (`request_for_provider` / `merge_fallback_chat_kwargs` call site `upstream.rs:810`) — client value survives, no dup key.
- [ ] New/rewritten test: a `/v1/responses`-style request carrying a client `max_completion_tokens` in `extra_body` plus a provider `upstream_chat_kwargs.max_tokens` driven through the provider-fallback path asserts the provider alias does NOT land (alias collision avoided), proving the collapsed helper now applies the alias skip on the fallback path.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `src/upstream.rs` (`chat_request_field_is_set`, the collapsed `merge_chat_kwargs_gap_fill`, call sites `:810`/`:2011`); in-module `#[cfg(test)]` tests near the existing `request_for_provider_*` cases (`upstream.rs:3378+`).
- **New APIs:** one private `merge_chat_kwargs_gap_fill(request: &mut ChatCompletionRequest, defaults: &JsonMap<String, Value>)` replacing the two forks (name is illustrative; keep it private to the module).
- **Depends on:** Sequence AFTER U1 / plan task 12.1 (the `anthropic_to_responses` `stop_sequences` typed-field HIGH) — both touch `stop` semantics; land 12.1 first so the typed `stop` field is populated consistently before this arm relies on it.

## Constraints
- Request-wins is load-bearing: an explicit request field (typed OR `extra_body`) must always beat a configured default — this fix RESTORES it for `stop`; do not weaken it for any other key.
- `OPENAI_MAX_STOP_SEQUENCES=4→400` stop normalization (`models/chat.rs:84`, called `engine.rs`) is unchanged — this task does not touch stop-count validation, only the gap-fill field-set check.
- Request types are NOT `#[serde(deny_unknown_fields)]` (the `extra_body` flatten flow must keep working) — the collapsed helper must still gap-fill unrecognized keys into `extra_body`.
- The max-token-alias skip must remain a no-op when no alias is requested, so the leaf-finalize path stays byte-identical for existing inputs.
- `MonitorHub::disabled()` zero-overhead no-op and redaction paths are untouched.

## Out of scope
- Adding a `stop` entry to the `config.rs:349-352` strip list (separate appendix-LOW; explicitly excluded).
- The `anthropic_to_responses` `stop_sequences` typed-field fix and the >4→400 hard-rule restoration (plan task 12.1 / U1).
- `TerminalReason` serialize-contract test and any other Topic-12 finding.
- Any change to stop-count normalization or the `OPENAI_MAX_STOP_SEQUENCES` ceiling.

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
