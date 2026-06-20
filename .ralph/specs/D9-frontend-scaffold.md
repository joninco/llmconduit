# D9 — Frontend scaffold (React+TS+Vite) + data plumbing + design system

> **Source:** DASHBOARD_PLAN.md rev 8 §3. Topic 13. Built against a mocked contract (parallel with the
> Rust seams) — the contract is anchored to D1-D7/D13's real shapes.

**Priority:** HIGH · **Surface:** new `dashboard-frontend/`

## Purpose
The SPA shell: React 18 + TypeScript + Vite, state (TanStack Query + zustand), the single
`DashboardSocket` (batched envelope, per-domain dedup, snapshot-then-live), REST client, an in-browser
mock backend, hash router, and the dark-ops design system. This unblocks all four view specs (D10-D12)
to build against a stable contract while Rust lands in parallel.

## Jobs to Be Done
- Vite + React 18 + TS; `build` → `../dashboard-frontend/dist` (consumed by D8 via `$OUT_DIR`).
  Stack: `@tanstack/react-query` (REST cache + WS-driven invalidation), `zustand` (live WS state),
  `@tanstack/react-virtual` (virtualized flow table), `highlight.js` (JSON), `uPlot` (sparklines),
  `d3-force` + `d3-sankey` (topology + Sankey), `tailwind` + `shadcn/ui` (components).
  **`framer-motion` DEFERRED** — add only when a view (theater) demonstrably needs it; do NOT use FLIP
  in virtualized rows.
- REST client (`api/client.ts`) for the D13 endpoints (`/flows`, `/flows/:id`, `/metrics`,
  `/topology`, `/catalog`, `/snapshot`, `/flows/:id/kill`), with the CSRF token header (D7) on kill.
- `DashboardSocket` (`api/ws.ts`): connects `/dashboard/ws`, handles snapshot-then-live, decodes the
  batched `DashboardFrame {domain,seq,batch}` (D7), dedups **per-domain whole-frame**
  (`seq <= last_seq[domain]` skips), feeds zustand stores. Drives the D11 time-travel `seek`
  (pause applying + shadow-buffer frames; LIVE toggles replay).
- In-browser mock (`api/mock.ts`) emitting realistic WS frames + REST responses so views ship before the
  rust contract is live.
- `useSyncExternalStore` bridges zustand → React 18 concurrent.
- **React 18 viz correctness (§3.3):** all imperative viz (d3-force, d3-sankey, uPlot) isolated behind
  `useLayoutEffect` with full cleanup (destroy sim / dispose uPlot / remove SVG); StrictMode-idempotent
  (re-run does not leak sims or duplicate nodes); d3 computes layout where possible, React renders.
- Hash router (`#/flows`, `#/topology`, `#/sankey`, `#/theater`), `App.tsx` layout shell (stats strip
  top, scrubber under it, view router).
- **Auth UX (D7 assigns to the frontend):** a login shell — when the SPA loads unauthenticated it
  renders a token-entry form that `POST`s to `/dashboard/login` (D7), stores the resulting session, and
  redirects into the dashboard; a logout control `POST`s `/dashboard/logout`. The app boots with the
  double-submit CSRF token (set by D7 as a non-HttpOnly cookie + embedded in the SPA bootstrap) and
  attaches it as `X-CSRF-Token` on the kill POST (D10/D6). A 401 on any fetch/WS → bounce to the login
  shell. (D9 owns the UX + the `X-CSRF-Token` header wiring; D7 owns the server-side issuance+verify.)
- **Design system tokens:** bg `#0d0f12`, panel `#16191e`/`#1e2329`, line `#2a313a`; status healthy
  `#58d68d`/cooling `#f6c453`/down `#ff6b6b`; accent `#6bb6ff`, meta `#c58bd1`; diff tints. System-sans
  UI; `ui-monospace,"JetBrains Mono",SF Mono` for payloads; `font-variant-numeric: tabular-nums`; 4 px
  spacing grid; `prefers-reduced-motion` cuts particles/animation.

## Acceptance criteria
- [ ] `dashboard-frontend/` builds via `npm run build` to `dist/` (D8 embeds it); `npm run dev` serves
      against the in-browser mock with all 4 routes reachable.
- [ ] `DashboardSocket` decodes the batched `DashboardFrame` + dedups per-domain; a test feeds a
      `Monitor` frame with sibling messages and asserts all apply.
- [ ] REST client typed against the D13 shapes; kill includes `X-CSRF-Token`.
- [ ] Imperative-viz `useLayoutEffect`+cleanup pattern enforced; a StrictMode double-invoke does not
      leak a d3 simulation or duplicate an SVG (a viz-wrapper test).
- [ ] Design tokens centralized; `tabular-nums` on all numeric chips; `prefers-reduced-motion` honored.
- [ ] Mock backend produces flows/usage/metrics/topology + a multi-message `DebugUpate`-equivalent
      `Monitor` frame for view development.
- [ ] Login shell renders for unauthed loads, `POST`s `/dashboard/login`, redirects on success;
      logout control `POST`s `/dashboard/logout`; a 401 on any fetch/WS bounces to the login shell;
      the CSRF token is read from the bootstrap/cookie and sent as `X-CSRF-Token` on kill (mock test).
- [ ] Type-check (`tsc --noEmit`) + `eslint` clean; Codex-xhigh APPROVED.

## Integration points
- **Depends on:** NOTHING for the mock scaffold (Phase A parallel-start — develops against the
  in-browser mock). Only the **embedded production build** (`npm run build` consumed by 13.8) depends
  on 13.8; the contract it targets = D1/D3/D4/D5/D7/D13 shapes (typed from the specs, mocked until
  those land).
- **Unblocks:** D10, D11, D12 (all views).
- **Parallelizable:** develops against the mock WHILE D1-D7 Rust seams land — no blocking on Rust.

## Constraints
- No FLIP animations in virtualized rows; no `framer-motion` until needed.
- All viz StrictMode-safe with cleanup.
- Single embedded artifact (no external CDNs — D8/CSP).
- TS discriminated unions for the WS `DashboardPayload` (exhaustive switches, no `any`).

## Out of scope
- The 4 views themselves (D10-D12); this is scaffold + plumbing + tokens only.
- Real-backend e2e (landed when D1-D7/D13 are live, verified in D13).

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
