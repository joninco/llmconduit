---
session_id: dashboard-topic13
active: false
branch: worktree-dashboard
base_commit: a311063
max_agents: 4 (capped by file-conflict reality — see below)
review_gate: Codex-xhigh per task (.ralph/REVIEW_PROTOCOL.md), orchestrator-run via Bash
builtin_review: disabled (--no-review)
---

# Orchestration log: dashboard-topic13 (Argus dashboard, Topic 13)

Orchestrator = this session (lean context). All implementation/fix work in isolated
Opus Task subagents. Codex-xhigh review per task run by orchestrator via `git show <hash> | codex exec`.
Credit rotation: `/home/jon/scripts/codex-account next` on out-of-credits, retry same command.

## Conflict / parallelism map (single shared worktree)
- Shared-file backend tasks (http.rs/engine.rs/lib.rs/upstream.rs/monitor.rs) must SERIALIZE
  (concurrent commits race the git index; concurrent same-file edits clobber).
- Frontend track (dashboard-frontend/, D9/D10/D11/D12) is file-disjoint from Rust → parallelizes
  with the backend serial chain. Parallel pattern: frontend agent is git-free, orchestrator commits its dir.
- Critical path: 13.1 → 13.2 → {13.3 → 13.4} → 13.5 → 13.7b → 13.13 → views.

## Task status (canonical tracker = Task tool list #1–#13; review log = /tmp/dashboard-topic13-review.md)
- Iter 1 (Phase A): 13.1 D1 (self-commit) ‖ 13.9 D9 (git-free, orch commits).
- DONE: D1 `66d95ff` (R5 APPROVED), D9 `5d9573` (R5 APPROVED), D8 `1fd8938` (R1 fixing).
- ORCHESTRATION POLICY: spawn a FRESH Agent per task AND per fix round. SendMessage-resume of a
  fully-idle agent proved unreliable (build-D8/build-D10 ignored resume messages); fresh spawns always
  execute. Fresh fix agents read the committed code + findings (no retained context needed).

## D7 contract requirements (frozen by D9 — the D7 subagent MUST honor these)
- Dashboard WS payload serde: `#[serde(tag="type", rename_all="snake_case")]` on `DashboardPayload`,
  arms monitor/usage/metric_tick/flow_status/topology_update. The `Monitor` arm NESTS the real
  `DebugWsMessage` under a `message` field — i.e. `{"type":"monitor","message":{<DebugWsMessage, itself
  internally-tagged "type">}}` — NOT flattened (DebugWsMessage already owns a `type` tag → collision).
  (Corrected after Codex R2.) D9 golden fixtures `dashboard-frontend/src/api/ws.fixtures.ts` = exact bytes.
- Bootstrap: emit `window.__LLMCONDUIT_DASHBOARD__` = {authenticated:bool, csrf_token, mutations_enabled}
  (field name `authenticated`, frozen with D9 R2).
- CSRF double-submit cookie name = `llmconduit_csrf` (non-HttpOnly).
- Frame envelope: `DashboardFrame{domain,seq,batch}`, ONE per DebugUpdate (seq=DebugUpdate.sequence),
  per-domain whole-frame dedup. Domains: flow/metrics/topology/monitor.
- WS `flow_status`/`usage` payloads: D7 MUST emit `api_call_id` (REQUIRED, authoritative key matching
  D1 store + D13 `/flows/:id`) AND `response_id` (the resp_uuid) AND `model_served`. D7's spec SKETCH
  fields `response_id`/`served_model` are illustrative (`/* */` placeholders) and superseded by this
  api_call_id-keyed shape (orchestrator-reconciled across D1/D7/D13; specs frozen, NOT edited). D4 ProviderHealth
  DTO (frozen w/ D9): {id,name,route,base_url,status:healthy|cooling|down,cooling_until_ms,last_error,
  served_count,failover_count,consecutive_failures,catalog_fetched_ms,catalog_size}. Flow id key =
  `api_call_id` (D13 routes `:id`=api_call_id). FlowStatus = open|completed|failed|cancelled.
- D13/D1 TODO (surfaced by D10 R2): the flow summary should expose a `client` label (derived from the
  user-agent header D1 already captures) so the inspector's "client" column isn't the HTTP method.
  D10 renders honestly (unavailable) until then; D13 should add it to the /flows summary shape.
- D4 ProviderHealth serde: serialize ALL fields (do NOT use `skip_serializing_if` on the Option fields
  like cooling_until_ms/last_error) so the key is always PRESENT (null when None) — frontend models them
  as required-key `T|null`. `base_url` required non-null. (Surfaced by D9 R3.)
