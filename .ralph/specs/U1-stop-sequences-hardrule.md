# U1 — Anthropic stop_sequences honor OPENAI_MAX_STOP=4 hard-rule (400)

> **Source:** thermo-nuclear PROJECT review 2026-06-20 (Topic 12). See /tmp/thermo-project-review.md

**Priority:** HIGH · **Surface:** src/adapters/anthropic_to_responses.rs, src/models/chat.rs, tests/port_translation.rs · **Thermo finding:** Anthropic `stop_sequences` smuggled raw into `extra_body["stop"]` bypasses the OPENAI_MAX_STOP_SEQUENCES=4 → 400 hard rule

## Purpose
`convert_request` maps Anthropic `stop_sequences` RAW into `extra_body["stop"]`
(`src/adapters/anthropic_to_responses.rs:39-45`) while leaving the typed
`ResponsesRequest.stop` field `None` (`:79`). The OpenAI stop-sequence ceiling is enforced ONLY on the
typed field: `normalize_stop` (`src/models/chat.rs:84-101`, `OPENAI_MAX_STOP_SEQUENCES = 4` at `:81`)
is called from the engine on `request.stop` (`src/engine.rs:1383`). Because the Anthropic value never
populates `request.stop`, it skips `normalize_stop` entirely and rides through
`build_upstream_extra_body` (`src/engine.rs:426`, invoked `:1372`) into the upstream request untouched.
A caller sending >4 `stop_sequences` therefore SILENTLY bypasses the documented "it 400s, not
truncates" contract (`src/models/chat.rs:83`) — a wire-contract bug. Fix: route
`request.stop_sequences` through `normalize_stop` and assign the result to the TYPED
`ResponsesRequest.stop` field, eliminating the raw `extra_body["stop"]` smuggling so the same ceiling +
empty-drop normalization the Chat path enjoys also covers the Anthropic path.

## Jobs to Be Done
- Anthropic `stop_sequences` flow through the SAME `normalize_stop` gate as the canonical/Chat path.
- The typed `ResponsesRequest.stop` field carries the sequences; `extra_body["stop"]` is no longer
  written by the Anthropic adapter (stop the raw smuggling).
- >4 `stop_sequences` produce a 400 (`AppError::bad_request`) at convert time — never silently truncate
  and never reach the upstream.
- Empty-string sequences and an all-empty / empty list collapse to `None` (consistent with
  `normalize_stop`), not a stray `extra_body` entry.

## Acceptance criteria
- [ ] `convert_request` calls `crate::models::chat::normalize_stop(request.stop_sequences)` (clone/take
      as needed) and assigns the `Some`/`None` result to `ResponsesRequest.stop`; the `extra_body.insert("stop", …)`
      block at `src/adapters/anthropic_to_responses.rs:39-45` is removed so the adapter never writes
      `extra_body["stop"]`.
- [ ] `convert_request` propagates the `normalize_stop` `Err` (it already returns `AppResult`, signature
      `src/adapters/anthropic_to_responses.rs:24`); a request with >4 `stop_sequences` returns
      `AppError::bad_request` with status `BAD_REQUEST`.
- [ ] ≤4 non-empty `stop_sequences` (e.g. `["STOP","END"]`) land in `result.stop` as
      `Some(vec!["STOP","END"])` and `result.extra_body.get("stop")` is `None`.
- [ ] An all-empty or empty `stop_sequences` list yields `result.stop == None` and no `extra_body["stop"]`.
- [ ] `tests/port_translation.rs::anthropic_stop_sequences_move_to_extra_body` (`:82-92`) is rewritten
      (renamed to reflect the typed field, e.g. `anthropic_stop_sequences_map_to_typed_stop`) to assert
      `result.stop == Some(vec!["STOP".into(),"END".into()])` and `result.extra_body.get("stop").is_none()`;
      the stale doc comment at `:81` is updated.
- [ ] The IN-MODULE unit test `converts_stop_sequences_to_extra_body_stop`
      (`src/adapters/anthropic_to_responses.rs:1005-1030`) — which today asserts `result.stop == None` and
      `result.extra_body.get("stop") == Some(["</decision>"])`, BOTH of which INVERT under the fix — is renamed
      (e.g. `converts_stop_sequences_to_typed_stop`) with assertions updated to
      `result.stop == Some(vec!["</decision>".to_string()])` and `result.extra_body.get("stop").is_none()`.
- [ ] A NEW test asserts >4 `stop_sequences` (e.g. five entries) yields
      `convert_request(...).expect_err(...).status == BAD_REQUEST`.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `src/adapters/anthropic_to_responses.rs::convert_request` (stop handling at `:39-45`,
  `:79`); reuses `crate::models::chat::normalize_stop` + `OPENAI_MAX_STOP_SEQUENCES`
  (`src/models/chat.rs:81-101`).
- **New APIs:** none — `ResponsesRequest.stop` already exists (`:79`) and `normalize_stop` is already
  `pub(crate)`.
- **Stop tests:** TWO stop tests must be updated — the in-module unit test
  `converts_stop_sequences_to_extra_body_stop` (`src/adapters/anthropic_to_responses.rs:1005-1030`) AND
  the `tests/port_translation.rs` integration test (`:82-92`). Both currently assert the old
  `extra_body["stop"]` smuggling behavior and invert under the fix.
- **Depends on:** nothing; sequence FIRST among Topic 12 (HIGH, wire-contract fix).

## Constraints
- Wire behavior change is the FIX: ≤4 sequences must reach upstream identically to before in VALUE
  (now via the typed `stop` field rather than `extra_body["stop"]`). Confirm the upstream chat request
  still carries the same `stop` values on the wire (engine threads `normalized_stop` →
  `UpstreamRequestAdditives.stop`, `src/engine.rs:1383,1408`); the only observable change for valid
  inputs is the 400 on >4 and the disappearance of the raw `extra_body["stop"]` key.
- Preserve `OPENAI_MAX_STOP_SEQUENCES = 4` and the "400, not truncate" semantics — do not weaken or
  re-clamp.
- Do not introduce `#[serde(deny_unknown_fields)]` or otherwise touch the `extra_body` flatten flow.
- Explicit-request-fields-win precedence and `parallel_tool_calls=false` forcing must be unchanged.

## Out of scope
- The Chat-completions stop path (already typed + normalized via `request.stop`).
- Any change to `normalize_stop`'s ceiling value or empty-drop logic.
- Engine-side stop handling (`src/engine.rs:1383`) — it already normalizes the typed field; this fix
  only ensures the typed field is populated by the Anthropic adapter.

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
