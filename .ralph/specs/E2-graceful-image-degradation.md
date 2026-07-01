# Spec E2 — Graceful image degradation for non-multimodal upstreams

> **Topic E — engine robustness.** Sibling of E1. Added 2026-07-01 from a field incident + `/askcodex`
> xhigh design review (review incorporated 2026-07-01; MUST-CHANGE items folded in). BACKEND only
> (`src/*.rs`). Acceptance criteria below = the oracle.

## Field incident (root cause)

claude-cli → llmconduit `:5022` tool-call chain died with an "API error":

```
08:06:54 WARN llmconduit::upstream: upstream provider failed; entering cooldown provider="local-vllm"
  cooldown_secs=30 error=upstream chat failed with 400 Bad Request:
  {"error":{"message":"DeepSeek-V4-Flash-DSpark is not a multimodal model","type":"BadRequestError","code":400}}
08:06:54..08:07:12  POST /v1/messages  status=502  (×6, every request — provider cooling)
```

A tool result carried an **image**. The only upstream is text-only → vLLM returned **400 "is not a
multimodal model"**. Two independent defects compounded:

1. **The raw image reached a text-only backend.** The G4 vision agent (`activate_image_agent`) was INACTIVE
   (no `vision_url`), so images fell through the "disabled-agent / missing-url" path and were forwarded
   verbatim → 400. (Also leaks even when the agent is ACTIVE — see the E2b choke-point note.)
2. **A request-intrinsic 400 tripped a provider-wide cooldown.** The streaming-chat failover path cools
   down the provider on ANY non-`Terminal` error (`upstream.rs:1807`), unlike the proxy path which only
   fails over on 5xx/408/429 (`should_failover_proxy_status`, `upstream.rs:2818`). One 400 → 30 s cooldown
   → every unrelated request 502'd.

## Goal

- A **request-intrinsic** 4xx (the request itself is unacceptable to any equivalent backend) is surfaced
  terminally and never cools a healthy provider. A **provider/transport** failure (5xx/408/429/connect,
  and provider-config 401/403/404) keeps today's failover + cooldown.
- An image that cannot be handled (non-native-vision backend, no image agent to offload to) is replaced by
  an instructive text **placeholder** (default) so the model self-corrects, or the turn is **rejected**
  with a structured 4xx (opt-in) — NEVER forwarded raw. Invariant: **no raw image content reaches a
  non-native-vision backend.**

Non-goals: adding/altering the vision backend or the active-agent strip+offer-`analyze_image` flow;
native-vision passthrough; non-image binary blocks (PDF/audio/`ContentItem::Other`) — explicitly OUT OF
SCOPE (documented follow-up); `/v1/completions` (raw byte proxy, bypasses the engine — excluded from the
invariant); a `count_tokens` route (does not exist in this repo).

---

## Architecture constraints (from AGENTS.md — re-read before editing)

- **Canonical-Responses-only.** Both transforms operate on `ResponsesRequest` / `Vec<ResponseItem>` at the
  engine layer — NOT per-inbound-format, NOT at the leaf (already lowered to `ChatCompletionRequest`).
- **Failover is pre-first-chunk only.** E2a only reclassifies an already-failed attempt's disposition; it
  adds no retry and duplicates no streamed tokens.
- **Deterministic upstream bytes + replay-safe.** claude-cli replays full history (incl. the image) every
  turn; the placeholder must be byte-identical each turn (no `Uuid`/clock/`HashMap`-order). BUT byte
  identity is not sufficient for replay: a degraded turn MUST bypass the replay cache (see AC-5).
- **Observable, not silent.** MVP: a WARN log with the degraded count + a monitor phase via `emit_with`
  (no-op under `MonitorHub::disabled()`). Response header + dashboard flag are follow-ups (see AC-6).
- **Redaction preserved.** Placeholders carry NO base64/URI bytes; existing image-URI log redaction
  (`upstream.rs:814`, `redaction.rs`) still applies to any non-stripping path.

## Key anchors (verified against HEAD this session)

| What | Location |
|-|-|
| G4 gating; returns `None` when agent inactive | `engine.rs:1481` `activate_image_agent` |
| Call site (residual pass goes AFTER this) | `engine.rs:1174` |
| Native-vision passthrough decision | `engine.rs:1502` `backend_is_native_vision` |
| Active strip is `role=="user"` + `image_url:Some` ONLY (leaks non-user + `file_id`) | `vision/strip.rs:113`, `:210` |
| Canonical image block (both variants) | `models/responses.rs:184` `ContentItem::InputImage { image_url, file_id, .. }` |
| Anthropic file image → `InputImage{file_id:Some}` | `adapters/anthropic_to_responses.rs:712` |
| Anthropic tool_result image → `FunctionCallOutput` + separate user image msg | `adapters/anthropic_to_responses.rs:339` |
| Per-profile native-vision flag | `config.rs:456/532` `native_vision`, `:1128` `profile_native_vision` |
| Config load / persisted / env override / preserve | `config.rs:17` `Config`, `:70-80` vision fields, `:593` `PersistedConfig`, `:1604` env overrides |
| Leaf builds "upstream chat failed with {status}" (after overflow check) | `upstream.rs:1195-1267` |
| Streaming failover Err arm (cooldown trigger) | `upstream.rs:1807` `mark_failure` |
| Terminal-disposition skip (no cooldown/failover) | `upstream.rs:1789` |
| Proxy failover status classifier (reuse) | `upstream.rs:2818` `should_failover_proxy_status` |
| Disposition enum + promotion | `error.rs:24` `FailoverDisposition`, `:114` `upstream_with_disposition` |
| `AppError::upstream` → **502** (why Reject/4xx need another variant) | `error.rs:76` |
| Attempt error taxonomy (`Terminal` vs `HttpStatus`) + `failover_reason` | `upstream.rs:114/117/143` `classify_attempt_error` |
| Non-streaming `response.failed` → `AppError::upstream` (502 masking) | `http.rs:1439` |
| Replay hashes post-transform visible history (collision risk) | `replay.rs:77` `hash_visible_history` |
| Cooldown unit tests | `upstream.rs:7393-7446` |

---

## Task E2a — request-intrinsic 4xx never cools down (do FIRST; carries its own tests)

**Disposition matrix (the oracle — from the xhigh review, adopted):**

| Upstream outcome | Failover? | Cooldown? | Disposition |
|-|-|-|-|
| 5xx, connect error, timeout / no response | yes | yes | `Failover` (unchanged) |
| 408, 429 | yes | yes | `Failover` (unchanged) |
| 401, 403, 404 (provider/model/auth config) | yes | yes | `Failover` (unchanged) |
| context overflow 400, first attempt | leaf shrink+retry | no | (handled at `upstream.rs:1195`, unchanged) |
| context overflow persists after shrink | no | no | `Terminal` (unchanged) |
| **request-intrinsic: 400, 413, 415, 422** | **no** | **no** | **`Terminal` (NEW)** |

**Change.**
- Keep `should_failover_proxy_status` (`upstream.rs:2818`) as the failover-eligibility test
  (`is_server_error() || 408 || 429`); optionally rename to a shared `status_is_failover_eligible`.
- In the leaf chat error build (`upstream.rs:1254-1267`, i.e. AFTER the existing context-overflow branch
  at `:1195`), when the failed status is in {400, 413, 415, 422}, construct the error with
  `FailoverDisposition::Terminal` (`upstream_with_disposition`). Everything else stays default `Failover`.
  The `== Terminal` gate at `upstream.rs:1789` then skips `mark_failure` (no cooldown, no failover).
- **Dashboard taxonomy:** a terminal request-intrinsic 4xx must STILL classify as `HttpStatus` (the status
  taxonomy at `upstream.rs:143`), NOT `AttemptErrorClass::Terminal` (which is disposition-derived at
  `:117` and reserved for context-overflow). Add/set a `failover_reason` = `TerminalNoFailover` (or
  equivalent) so the trace distinguishes "terminal because request-intrinsic" from a served/overflow
  terminal. Do not collapse the two.

**Documented limitation.** `Terminal` suppresses failover, so a request-intrinsic 400 is NOT retried on a
differently-capable provider (e.g. a text-only provider 400s an image while a multimodal peer would
accept). This is acceptable because E2b removes images before dispatch; for non-image request-intrinsic
400s another provider would reject identically. State this in `AGENTS.md`.

**Acceptance criteria:**
- **AC-1.** A synthetic upstream returning **400** on a streaming `/v1/messages` turn does NOT cool the
  provider: a second, unrelated request to the same provider is served (`200`), not `502`. (Regression for
  the incident; pair with `upstream.rs:7393`.)
- **AC-2.** 5xx/502/503, **408, 429, and 401/403/404** STILL cool down + fail over (unchanged). Assert
  these explicitly — do NOT lump all 4xx together.
- **AC-3 (streaming).** On a streaming turn, a request-intrinsic 4xx is surfaced to the client as a
  structured terminal SSE error per inbound format (Anthropic `error` / Responses `response.failed` / Chat
  SSE error) — never a raw `?` abort, never a masked `502`.
- **AC-3b (non-streaming, SHOULD).** Non-streaming collectors currently turn `response.failed` into
  `AppError::upstream` → **502** (`http.rs:1439`, `error.rs:76`). Either scope status-preservation as a
  follow-up OR add a status-preserving upstream-error variant. If deferred, say so; do not claim AC-3 holds
  non-streaming.

## Task E2b — residual-image safety pass (core; carries its own tests)

**Change.** Add a role-agnostic **residual-image pass** at the engine canonical layer, run AFTER
`activate_image_agent` (call site `engine.rs:1174`) whenever the resolved backend is NOT native-vision —
regardless of whether the agent returned `Some` or `None`. It sweeps ALL `ResponseItem`s (not just `user`)
for `ContentItem::InputImage` (BOTH `image_url: Some(..)` AND `file_id: Some(..)` variants) and applies the
policy. This is the true choke point: the active agent only strips `role=="user"` + `image_url` images
(`strip.rs:113/210`), so non-user images, `file_id` images, `tool_choice=="none"` residuals, and
old-history images all leak past it today. New invariant: **after this pass, no `InputImage` remains when
the backend is non-native-vision.** Must NOT fire on native-vision passthrough, and must NOT re-transform
images the active agent already stripped/cached (they're already gone).

Precise "image" scope: `ContentItem::InputImage` only. `ContentItem::Other` unknown blocks and non-image
binaries (PDF/audio) are OUT OF SCOPE (documented) — they cannot be reliably classified here.

New fn in `src/vision/strip.rs` (reuse the `ResponseItem`/`ContentItem` traversal): substitute each image
part IN PLACE with a stable placeholder text (preserve position + count; keep sibling text/`FunctionCallOutput`
content). Wording (stable, no bytes):
- user image: `[image omitted — this model is text-only and cannot view images. {n} image(s) were attached here. Do NOT guess their contents; ask the user to describe them or provide text.]`
- tool-output image (`FunctionCallOutput`): `[the tool returned an image, which this text-only model cannot view. Do NOT call the same tool again for image-only output; request text output or ask the user what it shows. Do NOT fabricate its contents.]`

Placeholder is INLINE (preserves role/location) — not a system-only note. `{n}` counts images in that same
message, in wire order (no `HashMap`).

**Config.** Add `unsupported_image_policy: { Placeholder, Reject }` (`#[serde(default)]` → `Placeholder`)
to BOTH `Config` (`config.rs:70-80`) AND `PersistedConfig` (`config.rs:593`), with the default fn, an env
override near `config.rs:1604`, and `configure()` preservation. No `Drop` variant (silent drop is a defect).

**Reject path.** `Reject` fails the turn BEFORE dispatch with a **4xx** (not 502): a bad-request-class
`AppError` (status 400/422) — NOT `AppError::upstream` (which is 502, `error.rs:76`) — rendered per inbound
format by the existing error path (pre-spawn errors return `Err`, `tests/gateway.rs:5157`). The provider is
never contacted, so it is not cooled.

**Replay.** A degraded turn MUST bypass the replay cache: `hash_visible_history` (`replay.rs:77`) hashes the
post-transform items, so two different images collapsing to identical placeholder text at the same position
would collide and serve the wrong cached response. Skip replay store+lookup for any turn the residual pass
degraded. (Byte-identical upstream bytes are still required for upstream determinism.)

**Acceptance criteria:**
- **AC-4.** `policy=Placeholder`, non-native-vision backend, agent inactive → the lowered
  `ChatCompletionRequest` contains NO image part; each image is replaced by the placeholder at its
  position. Cover: (a) `image_url` user image, (b) `file_id` user image, (c) tool-output image
  (`FunctionCallOutput` from an Anthropic tool_result image, `anthropic_to_responses.rs:339`), (d) an image
  in a NON-user message. Request is served `200` (no upstream 400).
- **AC-5.** Determinism + replay: lowering the SAME inbound request twice yields byte-identical upstream
  JSON (no per-turn id/clock; multi-image order preserved). A degraded turn does NOT read or write the
  replay cache (`replay.rs`) — prove two distinct images at the same text position do not collide.
- **AC-6 (MVP).** Degradation emits a `WARN` log with the count and a monitor phase via `emit_with` (no-op
  under `MonitorHub::disabled()`). **SHOULD (follow-up):** response header `x-llmconduit-image-degraded:{n}`
  + a `FlowRecord`/snapshot/REST/WS dashboard flag — these need a metadata wrapper out of the engine's
  `ReceiverStream` (`http.rs:1134`, `dashboard_flow.rs:690`, `dashboard_api.rs:80`, `dashboard_ws.rs:764`);
  spec them as a separate follow-up task, not part of E2b's green bar.
- **AC-7.** No regression to the ACTIVE-agent path: with `image_agent_enabled=true` + `vision_url` set +
  non-native backend + a LATEST-user image, the agent still strips+caches + offers `analyze_image`; the
  residual pass finds nothing to do for that image (no double-transform) but DOES degrade any residual
  non-user/`file_id`/old-history image. Native-vision passthrough (`backend_is_native_vision→true`) forwards
  images untouched (residual pass is skipped).
- **AC-8.** `policy=Reject` → the turn fails pre-dispatch with an HTTP **4xx** (not 502) carrying a
  structured error body per inbound format ("upstream model is text-only; images are not supported"); the
  provider is NOT cooled (never contacted).
- **AC-9.** No base64/image-URI bytes in the placeholder, the upstream JSONL (`upstream.rs:814`), the
  dashboard/normalized capture, or any failed-error text for a degraded turn.

## Task E2c — docs only

`FEATURES.md` (graceful image degradation + `unsupported_image_policy`), `AGENTS.md` (the two new
invariants: "no raw image to a non-native-vision backend" + "request-intrinsic 4xx never cools/failover",
incl. the documented failover-suppression limitation), `config.yaml` example comment. No code, no deferred
tests (E2a/E2b each ship their own tests).

## Gate B (every code task) + STOP

- `cargo fmt --all` + `cargo test` + `cargo clippy --all-targets -- -D warnings` clean. No stubs — full
  behavior + the listed ACs as tests, within the same task. Commit: E2a `fix(upstream):`, E2b
  `feat(engine):`, E2c `docs:`.
- STOP if: canonical-Responses-only or pre-first-chunk-failover cannot be honored; the residual pass cannot
  be made a true choke point without touching a raw-proxy path; or Gate B cannot go green.
