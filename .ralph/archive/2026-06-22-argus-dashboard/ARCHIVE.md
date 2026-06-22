# Archived Ralph config — Argus dashboard + thermo + core gaps

Archived 2026-06-22 (branch `worktree-dashboard`). **All work here is COMPLETE and Codex-xhigh
APPROVED** — moved out of the active `.ralph/` so a fresh `/ralph-guide` can be created (next
target: `FEATURES.md`). Nothing is deleted; full per-task history is in the git log.

## What this was
The complete prior llmconduit Ralph program, four topics:

- **Core gaps** — `specs/G1..G8`, `specs/P1` — context-window retry, model-family reshaping,
  context budgeting, image agent, debug-log rotation, SSE buffer cap, config routes, reasoning
  stream handling, output-config effort.
- **Topic 11 — thermo follow-ups** — `specs/T1..T11` — leaf-profile resolution, routing candidate
  plan, ToolDeltaGate extraction, vision module split, SSE guard, typed terminal reason, reasoning
  egress state, budgeting-layer move, error policy, streaming test quality.
- **Topic 12 — thermo project review** — `specs/U1..U10` — stop-sequences hard rule, merge collapse,
  monitor zero-overhead emit, config precedence tests, web-search ceiling, header dedup, tool-delta
  running bytes, image-agent flaky sleep, terminal-reason wire contract, chat-lowering dedup.
- **Topic 13 — Argus realtime dashboard** — `specs/D1..D13` — FlowStore/middleware, request
  identity + leaf capture, telemetry guard + usage, provider health + topology, MetricsLayer +
  snapshots, AbortHub/kill, auth + CSP + WS, build embed, React scaffold, the four views, REST
  routes + price config.

## Files
- `IMPLEMENTATION_PLAN.md` — the final plan (all topics marked complete).
- `GOAL.md` — the original goal statement.
- `specs/` — all 43 frozen specs.
- `orchestrate.dashboard-topic13.state.md` — the orchestration run log.

## Kept active (reused by the next project)
- `../../REVIEW_PROTOCOL.md` — the Codex-xhigh per-task review gate.
- `AGENTS.md` (repo root) — build / test / lint commands.
