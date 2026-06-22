# 16 — Control-room analytics view 🔭⚙️⭐

> Ralph gap spec — implementation-free. FEATURES.md **item 10**. Frontend.
> **Depends on specs 02, 03, 04, 07, and 12** (12 is REQUIRED for the provider latency/error tiles). Lands LAST — only honest once the spine exists.

## Operator question
"How is the gateway doing, at a glance?"

## Current state (verified by code search)
- The hash router currently has four routes: `flows | topology | sankey | theater`.
- The spine fields (phase timestamps, `attempts[]`, `client_label`, cost/usage confidence) come from specs 02/03/04/07.
- Provider latency/error tiles need spec-12's **per-provider ring** — deriving them from visible flow summaries would hide failed primaries. (Codex review.)
- FEATURES.md gates this view on data quality — "without per-client, per-attempt, true timing, and price confidence it is a pretty summary of incomplete data."

## Scope — what to build
- A **5th hash route** `overview` (`flows | topology | sankey | theater | overview`).
- Tiles: top models/providers by volume · cost · latency · error-rate; cost-over-time; token-mix — segmented gauge clusters + uPlot trends.

## Data quality (bake into acceptance)
- Each tile **inherits the weakest data-quality tag of its inputs**: any `estimated`/`unavailable` input ⇒ the tile is tagged `estimated`/partial. Never a confident-looking total over incomplete data.

## Acceptance criteria
- [ ] New `overview` hash route alongside the existing four; the existing routes are undisturbed.
- [ ] Tiles render: top models/providers (volume/cost/latency/error), cost-over-time, token-mix.
- [ ] Provider latency/error tiles consume the **spec-12 per-provider DTO** (not derived from visible flow summaries, which hide failed primaries).
- [ ] Each tile surfaces the **weakest** tag of its inputs (`estimated`/`unavailable` shown, not hidden).
- [ ] **don't-lie-with-zeros**: `unavailable` inputs render `—`; no fabricated `0` in any tile.
- [ ] Empty-state (no data in window) renders `—`, not an all-`0` dashboard.

## Constraints / invariants (AGENTS.md)
- Per-domain `{domain, seq}` cursors for any new live data; consume existing DTOs — no new backend seam in this gap.

## Out of scope
- New backend aggregates beyond the spine; OTel/Prometheus egress + real alerting (later program, items 13–14).

## Validation gate
- **Frontend:** `npm run typecheck` (`tsc -b`) · `npm run lint` (`eslint . --max-warnings 0`) · `npm run test` (`vitest run` — tag propagation) · `npm run e2e` (Playwright): route loads + tag inheritance + empty-state `—`.
- Then **REVIEW_PROTOCOL.md**: Codex-xhigh review before the next gap.
