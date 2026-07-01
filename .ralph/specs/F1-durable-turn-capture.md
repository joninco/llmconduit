# Spec F1 — Durable per-turn capture (debug artifact for CC weirdness)

> **Topic F — observability / debug durability.** Added 2026-07-01 from an operator need + `/askcodex`
> xhigh design review (review folded in; all HIGH/MED findings addressed below). BACKEND only
> (`src/*.rs`). Acceptance criteria per task = the oracle.

## Motivation

`llmconduit` (`:5022`, systemd) fronts real Claude Code traffic, translating Anthropic `/v1/messages` →
OpenAI/vLLM. When a CC session shows weird output (stray `<think>` tags leaking into text, malformed /
dropped tool calls, truncated content), there is **no durable record of the response** to debug later or
from a fresh session:

| Surface | Requests | Responses | Durable |
|-|-|-|-|
| Journal (`journalctl`) | full `body_payload` | metadata only (`served_model`) — NO body | yes |
| JSONL (`upstream_request_log_path`) | full | none | yes |
| Dashboard flow ring | capped/redacted preview | error-body only, env-gated, evicts, wiped on restart | **no** |

A `<think>` leak is a **200 OK** — nothing flags or persists it. This spec adds an **opt-in** capture that
writes ONE self-contained artifact per turn to disk with the FULL, untruncated request+response chain, so
an operator (or a Claude debug session) can open exactly what the backend sent and what CC received.

## Goal

When `turn_capture_dir` is configured, every instrumented inference turn writes `<dir>/<api_call_id>.json`
containing, FULL and untruncated (redaction per constraints below):

- `inbound_request` — the Anthropic request from CC (raw inbound body).
- `upstream_request` — the translated OpenAI `ChatCompletionRequest` actually put on the wire (final attempt).
- `upstream_response` — the RAW vLLM response bytes (SSE text; the pre-parse ground truth).
- `served_response` — the exact bytes returned to CC (post-conversion: Anthropic SSE, or non-stream JSON, or an error body).
- Outcome — `api_call_id`, `model_requested`/`model_served`, `started_ms`/`finished_ms`, `status`
  (`completed`/`incomplete`/`failed`/`cancelled`), `terminal_reason`, and per-section `{bytes, partial, encoding}`.

**Works WITHOUT `--with-debug-ui`** (its own instrumentation gate). Opt-in: no `turn_capture_dir` ⇒ a
zero-op sink (no threads, no allocs, no extension). Age-rotated via the existing `debug_log_max_age_hours`.

## Non-goals

- Not a replacement for the dashboard/monitor (live view stays). Not wired to the dashboard UI.
- No per-attempt `attempts[]` array in v1 — capture the FINAL attempt's request+response (+ the final
  failed HTTP body). A structured `attempts[]` trace is a documented FOLLOW-UP.
- No response-side secret hunting beyond the shared redaction path. No streaming of `/v1/completions` (raw
  byte proxy, bypasses the engine — excluded). No new CLI merge/reader tool (the artifact is already a
  single JSON; a `analyze-turn` pretty-printer is a follow-up).
- Not sampling/error-only — a `<think>` leak is a 200, so capture is all-or-nothing per the opt-in gate.

---

## Architecture constraints (from AGENTS.md — re-read before editing)

- **New logged surface ⇒ redact.** The artifact is a NEW on-disk logged surface. Do NOT bypass secret
  redaction (AGENTS.md line 137). Route request bodies through BOTH `redaction::redact_image_uris_in_value`
  AND the same secret redaction the `http.rs` `body_payload` log uses (`redact_payload_secrets`). Responses
  get image-URI redaction (model output carries no header secrets; keep it cheap + consistent).
- **No `Bytes`-slice retention.** Do NOT retain a `Bytes` slice of the 256 MiB inbound middleware buffer
  (AGENTS.md line 144; REVIEW_PROTOCOL memory invariant). COPY bytes into the capture (to the section file),
  never hold a slice that pins the whole backing allocation.
- **Bounded memory (Codex HIGH #2).** Do NOT buffer full bodies in a `HashMap<_, PartialCapture>`. Stream
  each large section INCREMENTALLY to a per-turn temp section file; keep only small metadata in memory. The
  final single JSON is assembled by STREAMING the section files through a JSON-string escaper (bounded
  buffer) — never by loading all sections into RAM at once.
- **Diagnostic, operator-only (REVIEW_PROTOCOL 2026-06-22 ruling).** Showing full request/response detail to
  the operator behind FS perms is the INTENDED purpose — not a leak. Reviewers must NOT flag "the artifact
  exposes a body/credential to the operator." The redaction above is for secrets-at-rest hygiene, not
  because operator visibility is a leak.
- **Cancellation is first-class.** A client hang-up / dashboard kill must NOT hang finalization; it closes
  the served/response sections as `partial` and still writes the artifact (RAII, below).
- **Don't-lie-with-zeros.** A missing/unmeasured section is explicit (`partial: true` or an absent field),
  NEVER a fabricated empty value that reads as "measured empty."

## Design overview (the shape all tasks build toward)

1. **Own instrumentation gate (Codex HIGH #1).** Today the `ApiCallId` extension + inbound capture live
   INSIDE the flow-store gate (`http.rs:536`, `is_enabled() && is_flow_capture_request`), so with the
   dashboard OFF the engine gets `api_call_id = None`. Add a capture gate that fires when
   `turn_capture.is_enabled()` on the SAME whitelisted inference paths (`is_flow_capture_request`),
   independent of `flow_store().is_enabled()`: insert `ApiCallId` and `capture.start(api_call_id, …)`
   regardless of the debug UI.
2. **Temp sections + single-JSON assembly.** Per turn: `<dir>/.work/<api_call_id>/{inbound_request,
   upstream_request,upstream_response,served_response}` written incrementally. At terminal, stream-assemble
   `<dir>/<api_call_id>.json.tmp` → atomic rename to `<dir>/<api_call_id>.json`; delete the work dir.
3. **Carrier = `BackendChatRequest` (Codex MED #5), not `ServingToken`.** Add
   `capture: Option<Arc<TurnCaptureState>>` to `BackendChatRequest` (`upstream.rs:3288`; failover/routing
   rebuilds already CLONE its `Arc`s — `upstream.rs:1626/2115`). The upstream taps read it WITHOUT changing
   the `UpstreamClient` trait chain, and diagnostic file IO stays OFF the serving-metrics mutex.
4. **Hybrid finalize (Codex HIGH #3/#4).** `stream_anthropic_response` loop-end is the WRONG finalizer
   (misses disconnect `http.rs:1311`, non-stream `collect_anthropic_response` `http.rs:1475`, pre-spawn
   `engine.rs:1196`). Instead:
   - **Served bytes** are captured by wrapping the outbound response `Body` (a tee) — covers streaming,
     non-streaming, and handler error responses uniformly; its `Drop` marks `served_done(partial)`.
   - **Status/terminal_reason** come from the ENGINE terminal seam (the `TelemetryGuard`-style path, which
     already covers completed/failed/cancelled/pre-spawn/Drop — mirror `dashboard_flow.rs:1917/1991`) via
     `engine_done(status, reason)`.
   - The artifact is assembled ONLY after BOTH `engine_done` AND `served_done` fire (a flush/close barrier —
     Codex "Ordering"), idempotently; a `Drop` fallback on the capture guard finalizes an abandoned turn as
     `failed`/`cancelled` with whatever sections closed.
5. **Encoding contract (Codex LOW #7).** Sections store raw bytes. At assembly each section is embedded as:
   valid-UTF-8-and-parses-as-JSON ⇒ a JSON value; valid-UTF-8 ⇒ a JSON string; else ⇒ base64 string with a
   sibling `"<section>_encoding":"base64"`. (vLLM/Anthropic SSE is UTF-8, so the common case is text.)

## Key anchors (verified against HEAD this session — 2026-07-01)

| What | Location |
|-|-|
| `api_call_id` minted (unconditional) | `http.rs:400` |
| Flow-gated `ApiCallId` extension + inbound capture (the debug-ui-only gate to bypass) | `http.rs:536` |
| Path whitelist for instrumentation | `http.rs` `is_flow_capture_request` |
| Streaming Anthropic SSE builder (served, streaming) | `http.rs:1300` `stream_anthropic_response` |
| Early return on client disconnect (why loop-end finalize is wrong) | `http.rs:1311` |
| Non-streaming Anthropic collector (bypasses SSE builder) | `http.rs:1475` `collect_anthropic_response` |
| Chat / Responses served builders (other client formats) | `http.rs:1255` `stream_chat_completions_response` |
| Existing request JSONL writer (redaction + spawn_blocking + Mutex pattern to mirror) | `upstream.rs:814-856` `UpstreamRequestLogger` |
| Per-turn on-wire request dispatch (upstream_request tap; has owned `request` + `serving`) | `upstream.rs:1190` `dispatch_chat_stream` |
| Shrink-and-retry re-dispatch (last-writer-wins overwrite) | `upstream.rs:1214/1241/1248` |
| Raw upstream bytes read (upstream_response tap, pre-parse) | `upstream.rs:4011` `stream_success_response` |
| `BackendChatRequest` (carrier; rebuilds clone its Arcs) | `upstream.rs:3288`; rebuilds `:1626/:2115` |
| `ServingToken` staged failed-response body (reuse for final failed HTTP body) | `upstream.rs:3215` `set_pending_response_body`; gap-05 note AGENTS.md:166 |
| Engine mints `response_id`; `flow_store.link(response_id, api_call_id)` | `engine.rs:1167`, `engine.rs:1109` |
| Serving token alloc (api_call_id in scope) | `engine.rs:1122-1126` |
| Pre-spawn failure finalize (must also write a failed capture) | `engine.rs:1196` |
| Terminal finalize call site (status + reason + api_call_id in scope) | `engine.rs:1472` |
| RAII guard finalize + Drop fallback to mirror | `dashboard_flow.rs:1794` (Drop), `:1917` (finalize), `:1991` |
| Redaction helpers (reuse, do not bypass) | `redaction::redact_image_uris_in_value`; `http.rs` `redact_payload_secrets` |
| Age-rotation entry point + config | `log_rotation.rs`; `config.rs:878` `debug_log_dirs`, `:666` `debug_log_max_age_hours` |
| Config field pattern to mirror (`upstream_request_log_path`) | `config.rs:40/575/607/625/795`, env `:1675`, resolve `:1515/1540` |

---

## Task F1a — config field + module skeleton + disabled sink (do FIRST; carries its own tests)

**Change.**
- New module `src/turn_capture.rs`. Public `TurnCapture` handle (cheap `Clone`, `Arc`-backed) with a
  DISABLED constructor (no dir) whose every method is a zero-op (no thread, no alloc, no fs). Enabled
  constructor takes the resolved `turn_capture_dir`. Sketch the per-turn `TurnCaptureState` (metadata +
  section file handles) and the method surface the later tasks fill: `start(api_call_id, model_requested,
  started_ms) -> Option<Arc<TurnCaptureState>>`, `engine_done(id, status, reason)`, and section writers
  (stubs OK here, real in F1c–F1e) — but `start`/disabled-no-op must be REAL + tested now.
- `Config.turn_capture_dir: Option<PathBuf>` + `PersistedConfig` field + `#[serde(default,
  skip_serializing_if)]` + `configure()` preservation + env override `LLMCONDUIT_TURN_CAPTURE_DIR` (mirror
  `upstream_request_log_path`/`debug_log_max_age_hours` exactly — `config.rs:40/575/607/625/795/1675`).
- `debug_log_dirs()` (`config.rs:878`) INCLUDES `turn_capture_dir` so the existing age-rotation
  (`debug_log_max_age_hours`) prunes the artifacts.
- DI: construct `Option<TurnCapture>` in `lib.rs` from config; thread the handle to the HTTP router state
  and (F1d/F1e) to the upstream client. `main.rs`/`cli.rs` only if a field must surface there.

**Acceptance criteria.**
- **AC-1.** `turn_capture_dir` parses from YAML + TOML, trims blank→`None`, and
  `LLMCONDUIT_TURN_CAPTURE_DIR` overrides; `configure()` round-trips it. Unit test.
- **AC-2.** `debug_log_dirs()` contains the configured `turn_capture_dir` (so rotation covers it). Unit test.
- **AC-3.** A DISABLED `TurnCapture` (no dir) is a zero-op: `start(...)` returns `None`, no file/dir is
  created, no panic. Unit test.

## Task F1b — HTTP instrumentation gate + inbound + served-body wrapper

**Change.**
- **Own gate (Codex HIGH #1).** On the whitelisted inference paths (`is_flow_capture_request`), when
  `turn_capture.is_enabled()`, insert the `ApiCallId` extension and call `capture.start(...)` EVEN IF
  `flow_store().is_enabled()` is false — so `api_call_id` reaches the engine off the dashboard. Keep the
  existing flow-gated block; add the capture gate alongside (a shared "instrument this request?" predicate).
- **Inbound (Codex line 144).** COPY `body_bytes` (never a retained slice), redact (image URIs + the
  `http.rs` `redact_payload_secrets` used by `body_payload`), write to the `inbound_request` section.
- **Served body wrapper (Codex HIGH #3).** Wrap the outbound response `Body` for instrumented turns with a
  tee that appends served bytes to the `served_response` section incrementally, and whose `Drop` calls
  `served_done(partial = stream did not reach a clean end)`. This ONE wrapper covers streaming SSE
  (`stream_anthropic_response`/chat/responses), non-streaming (`collect_anthropic_response` `http.rs:1475`),
  and handler error responses — do NOT tap `engine::send_event` (it is canonical Responses, not the served
  Anthropic bytes — Codex "Engine vs HTTP").

**Acceptance criteria.**
- **AC-4.** With `--with-debug-ui` OFF and `turn_capture_dir` set, a `/v1/messages` turn still threads
  `api_call_id` to the engine and produces an artifact (integration test with a stub upstream).
- **AC-5.** `inbound_request` in the artifact is redacted: an image `data:`/URL URI and a secret-bearing
  field are both redacted; the artifact contains no raw image bytes. Assert against AGENTS.md line 137/144.
- **AC-6.** Served bytes are captured for (a) a streaming turn, (b) a non-streaming turn
  (`collect_anthropic_response`), and (c) a client disconnect mid-stream → `served_response` present with
  `partial: true`.

## Task F1c — engine terminal integration (status/reason) + RAII finalize

**Change.**
- Thread the `TurnCaptureState` handle to the engine keyed by `api_call_id` (registry lookup, or attach to
  the telemetry path). Call `engine_done(api_call_id, status, terminal_reason)` on EVERY terminal —
  completed/incomplete/failed/cancelled AND the pre-spawn failure (`engine.rs:1196`) AND the terminal
  finalize (`engine.rs:1472`).
- **RAII (Codex HIGH #4).** A capture guard whose `Drop` finalizes an abandoned turn (no explicit
  `engine_done`) as `failed` (or `cancelled` if the abort token fired) with whatever sections closed —
  mirror `TelemetryGuard`/`MiddlewareGuard` Drop (`dashboard_flow.rs:1794/1917/1991`). `engine_done` and
  `served_done` are IDEMPOTENT; artifact assembly fires exactly once, only after BOTH have reported (the
  flush/close barrier — Codex "Ordering"), then removes the registry entry + work dir.

**Acceptance criteria.**
- **AC-7.** A pre-spawn validation failure (e.g. `previous_response_id`, or a lowering error at
  `engine.rs:1196`) writes an artifact with `status:"failed"` + the terminal reason, `served_response`
  present (the error body), `upstream_request`/`upstream_response` absent-or-partial (never contacted).
- **AC-8.** A completed streaming turn writes `status:"completed"` with all four sections and
  `finished_ms > started_ms`.
- **AC-9.** A mid-stream client disconnect writes `status:"cancelled"` with `served_response.partial=true`
  and does NOT hang finalization (bounded-time test).

## Task F1d — upstream request capture (carrier on `BackendChatRequest`)

**Change.**
- Add `capture: Option<Arc<TurnCaptureState>>` to `BackendChatRequest` (`upstream.rs:3288`); populate it
  where the engine builds the backend request (api_call_id/state in scope); the failover/routing rebuilds
  CLONE it for free (`:1626/:2115`).
- In `dispatch_chat_stream` (`upstream.rs:1190`), capture the SANITIZED on-wire `ChatCompletionRequest` to
  the `upstream_request` section, redacted (image URIs + `redact_payload_secrets`). LAST-WRITER-WINS so the
  shrink-and-retry (`:1241/:1248`) and failover reflect the FINAL on-wire request (Codex MED #6). Reuse the
  `UpstreamRequestLogger` serialize+redact pattern (`upstream.rs:814-856`).

**Acceptance criteria.**
- **AC-10.** The artifact's `upstream_request` equals the final on-wire OpenAI request (post
  sanitize/profile lowering), redacted; a shrink-and-retry turn shows the SHRUNK request, not the first.
- **AC-11.** A failover turn (A rebuild → B) still captures (rebuild preserves the `Arc` handle); the
  `upstream_request` is the SERVING attempt's request.

## Task F1e — raw upstream response capture (+ final failed HTTP body)

**Change.**
- In `stream_success_response` (`upstream.rs:4011`), tap `response.bytes_stream()` via the
  `BackendChatRequest` capture handle and stream RAW bytes incrementally to the `upstream_response` section
  (pre-parse ground truth). A section write error / malformed / timeout / drop closes the section
  `partial:true` via its own drop guard and must NOT hang or fail the turn (Codex HIGH #4).
- **Final failed HTTP body (Codex MED #6).** When the final attempt ends in a non-2xx (not an SSE stream),
  capture that error body into `upstream_response` (reuse the gap-05 staged body on `ServingToken`,
  `set_pending_response_body` / AGENTS.md:166; last-writer-wins across attempts already matches gap-05
  semantics).
- **Encoding (Codex LOW #7).** Section bytes are raw; assembly emits UTF-8 as a JSON string, non-UTF-8 as
  base64 + `upstream_response_encoding:"base64"`.

**Acceptance criteria.**
- **AC-12.** A successful streaming turn's `upstream_response` is the EXACT concatenated raw vLLM SSE bytes
  (byte-for-byte over a stub upstream emitting a known frame sequence incl. a `<think>`-in-content case).
- **AC-13.** A final-attempt non-2xx (e.g. a 400 body) is captured as `upstream_response` with the error
  text; `status:"failed"`.
- **AC-14.** A malformed/oversized-frame upstream (the G6 cap trips) closes `upstream_response`
  `partial:true` and the artifact still finalizes in bounded time (no hang).

## Task F1f — final JSON assembly + atomicity + rotation + docs

**Change.**
- Stream-assemble `<dir>/<api_call_id>.json` from the section temp files through a bounded JSON-string
  escaper (Codex HIGH #2): requests embed as JSON VALUES when they parse, else strings; responses embed as
  strings (base64 fallback). Include the outcome metadata + per-section `{bytes, partial, encoding}`. Write
  `<api_call_id>.json.tmp`, `fsync`-then-rename for atomicity, delete the `.work/<api_call_id>/` dir.
- **Barrier.** Flush/close ALL section writers before assembly (queued disk writes can lag the stream —
  Codex "Ordering").
- **Rotation.** Confirm `debug_log_max_age_hours` prunes `<api_call_id>.json` by mtime; ALSO sweep orphaned
  `.work/*` dirs (crash residue) older than the same window.
- **Docs.** `FEATURES.md` (durable turn capture: gate, layout, redaction, rotation, works-without-debug-ui);
  `AGENTS.md` invariant ("turn capture is a redacted, bounded-memory, opt-in on-disk surface; sections
  stream to temp files; artifact is atomic single JSON"); `README.md`/config example for `turn_capture_dir`.
  Note the FOLLOW-UPS: `attempts[]` per-attempt trace; a `analyze-turn` pretty-printer CLI; chat/responses
  client-format coverage parity.

**Acceptance criteria.**
- **AC-15.** The final `<api_call_id>.json` is a single valid JSON with all four sections + outcome; a
  non-UTF-8 upstream section round-trips via base64 + encoding marker; no `.tmp`/work residue remains.
- **AC-16.** Redaction holds end-to-end: no image bytes and no secret-key values in ANY section of a
  captured artifact (image + secret in both inbound and upstream_request).
- **AC-17.** With `debug_log_max_age_hours` set, an artifact older than the window is pruned by the existing
  rotation; an orphaned `.work/<id>/` older than the window is swept.

## Gate B (every code task) + STOP

- `cargo fmt --all` + `cargo test` + `cargo clippy --all-targets -- -D warnings` clean. FULL behavior + the
  task's ACs as tests within the SAME task — no stubs carried across tasks (F1a's method stubs are the only
  allowed forward-decls, and they are no-op-tested). Commit: F1a `feat(config):`, F1b/F1c/F1e `feat(engine):`
  or `feat(http):`/`feat(upstream):` as appropriate, F1d `feat(upstream):`, F1f `feat(turn-capture):` + `docs:`.
- STOP if: bounded-memory (no full-body buffering) cannot be honored; the served-body wrapper cannot cover
  non-streaming + disconnect without touching a raw-proxy path; redaction (AGENTS line 137) cannot be
  applied to a section; or Gate B cannot go green.
