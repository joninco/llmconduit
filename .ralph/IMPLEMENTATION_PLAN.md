# IMPLEMENTATION_PLAN.md — Topic E / E2: graceful image degradation

> **Spec (oracle):** `.ralph/specs/E2-graceful-image-degradation.md` (xhigh review folded in 2026-07-01).
> **Conventions:** `AGENTS.md`. **Run:** `/ralph-orchestrate --no-review --agents 1` — Sonnet-5 subagents,
> SERIAL (shared files: `engine.rs`, `upstream.rs`, `vision/strip.rs`, `config.rs`, `error.rs`). Per-task
> Codex-xhigh review per `.ralph/REVIEW_PROTOCOL.md`.
> **To activate:** archive the current SSE plan, then
> `cp .ralph/IMPLEMENTATION_PLAN.graceful-image.md .ralph/IMPLEMENTATION_PLAN.md` and point `.ralph/GOAL.md`
> at Topic E / E2.

## Executive summary

**Status: 3/3 tasks complete — Topic E / E2 done.** E2a ✅, E2b ✅ (Codex-xhigh APPROVED), E2c ✅ — all
committed (full detail archived in `.ralph/COMPLETED_TASKS.md`).

Field incident: an image in a claude-cli tool chain hit the text-only upstream → **400 "not a multimodal
model"** → the 400 tripped a **30 s provider cooldown** → every request **502**'d. Two independent fixes,
each shippable with its own tests: (E2a) a *request-intrinsic* 4xx (400/413/415/422) must never cool down
or fail over a healthy provider; (E2b) a residual image must never reach a non-native-vision backend —
degrade it to an instructive text placeholder (or reject per policy) at the engine canonical layer. Builds
ON the existing G4 vision agent (unchanged); closes the residuals it misses.

## Tasks

| Task | Description | Status |
|-|-|-|
| E2a | request-intrinsic 4xx {400,413,415,422} → `Terminal` (no failover/cooldown); 401/403/404/408/429 unchanged; dashboard class stays `HttpStatus` + `failover_reason=TerminalNoFailover` (`upstream.rs`, `error.rs`); shipped AC-1,2,3 | ✅ |
| E2b | role-agnostic residual-image pass (`vision/strip.rs::degrade_residual_images`/`has_residual_images`) wired at `engine.rs` after `activate_image_agent`, gated on `!backend_is_native_vision`; `unsupported_image_policy` (`Placeholder`/`Reject`) on `Config`+`PersistedConfig`+env+`configure()`; `Reject`→400 (not 502) pre-dispatch; degraded turn forces `request.store=false` (bypasses replay lookup+store); shipped AC-4,5,7,8,9 (15 new integration tests + 14 unit tests) | ✅ |
| E2c | docs only: `FEATURES.md` "Graceful image degradation" entry; `AGENTS.md` invariants + limitation verified (already added by E2a/E2b, no dupe); README.md `unsupported_image_policy` example (no repo config template exists) | ✅ |

**Ordering: serial E2a → E2b → E2c** (shared files force serial; `--agents 1`). E2a and E2b are logically
independent, but E2a first establishes the 4xx-status handling that E2b's `Reject` path reuses, and it
stops the cascade class immediately. Each code task leaves `cargo test` GREEN within itself.

## Review corrections already baked into the spec (do not re-litigate)

1. NOT "all 4xx terminal" — only request-intrinsic {400,413,415,422}; 401/403/404/408/429 keep
   failover+cooldown (disposition matrix in spec).
2. Terminal 4xx still classifies as dashboard `HttpStatus`, not `AttemptErrorClass::Terminal`.
3. AC-3 scoped to STREAMING; non-streaming 502-masking is AC-3b SHOULD/follow-up (`http.rs:1439`,
   `error.rs:76`).
4. Choke point is a role-agnostic residual pass (active strip is `role=="user"`+`image_url` only,
   `strip.rs:113/210`) covering `file_id` images (`anthropic_to_responses.rs:712`).
5. `Reject` → 4xx via a bad-request `AppError`, NOT `AppError::upstream` (502).
6. Degraded turns bypass the replay cache (`replay.rs:77` collision).
7. Observability MVP = log + monitor phase; response header + dashboard flag are a follow-up (engine
   returns only `ReceiverStream`; needs a metadata wrapper).
8. Config lands in `Config` AND `PersistedConfig` + default + env override + `configure()` preserve.
9. Out of scope (documented): `ContentItem::Other`, non-image binaries, `/v1/completions` (raw proxy),
   `count_tokens` (no such route).

## Acceptance mapping

- E2a → AC-1 (400 no cooldown; 2nd request served), AC-2 (5xx/408/429/**401/403/404** unchanged — assert
  separately), AC-3 (streaming structured terminal), AC-3b (non-streaming 502 — deferred/flagged).
- E2b → AC-4 (four image forms replaced, served 200), AC-5 (determinism + replay bypass, no collision),
  AC-6 (log + monitor MVP; header/dashboard follow-up), AC-7 (active-agent + native-vision no-regression;
  residual still caught), AC-8 (`Reject`→4xx, provider not cooled), AC-9 (no image bytes anywhere).

## Live verification (orchestrator gate, after E2a+E2b green)

Prereq: `:5022` rebuilt + a text-only upstream. `/v1/messages` turn with an image block:
1. `Placeholder` → `200`, upstream JSONL shows placeholder text (no image); a follow-up text request is
   served (provider not cooling).
2. Synthetic upstream 400 on a non-image request → provider NOT cooled; unrelated request `200`.
3. `Reject` → HTTP 4xx with Anthropic `error` body; provider not cooled.
