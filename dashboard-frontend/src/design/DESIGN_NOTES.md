# Argus dashboard — design direction

## Direction: "Night Watch"
Argus = the hundred-eyed sentinel of myth; the product is a real-time LLM-proxy
telemetry **instrument**. The visual identity leans into *watching*: a deep indigo
observatory ground, an iris-violet brand accent ("the Eye"), and amber as the twin
attention-signal alongside the semantic status traffic-lights. Deliberately NOT the
prior generic dark+blue SaaS look, and NOT the acid-green-on-black cliché.

## Tokens (single source: `palette.ts`)
- bg `#0b0d17` · panel `#141826` · panel-raised `#1c2133` · line `#2a324e`
- accent (the Eye) iris-violet `#8f8bff` · meta orchid `#e28ac4`
- status: healthy mint `#5de2a2` · cooling amber `#f4c152` · down `#ff6e6e`
- text `#e7e9f3` · muted `#8b93ad`

## Type
- Display/UI: **Space Grotesk** (geometric grotesk, technical character)
- Data: **IBM Plex Mono** (every id / model / token-count / latency reads as telemetry)

## Signature
The **Argus eye** masthead mark (`NavTabs.tsx`) + tracked `ARGUS` wordmark with
`llmconduit · watch` eyebrow; uppercase tracked instrument tabs. The eye keeps a slow
`argus-watch` pulse (the one ambient brand motion; cut by prefers-reduced-motion).

## Status
- Applied: palette + fonts + Argus eye masthead + instrument pass (segmented mono gauge
  stats strip, tracked labels, iris row-hover).
- **Fonts self-hosted** via `@fontsource/space-grotesk` + `@fontsource/ibm-plex-mono`
  (imported in `src/design/fonts.ts`); Vite emits woff2 as hashed assets served from
  'self' → CSP `font-src 'self'` safe. No CDN `<link>`.
- Playwright baselines regenerated against the new design; gates green; committed.

## Shipped beyond identity
- **Inspector JSON** (`viz/JsonPane` + `viz/jsonFold`): per-path collapsible tree (iris
  chevrons, per-pane collapse-all, `{ … } N` summaries) + a SHARED search across all three
  layers (A·B·C) — each pane filters to matches + ancestors, amber match marker + count chip.
  Reuses the existing path-keyed structural diff + highlight.js; DOM contract preserved.

## Future passes
- Substring (not line-level) match highlight in the inspector; LIVE indicator as a radar
  "ping"; filter chips as instrument toggles; Theater stream tiles as scope traces. Keep
  boldness on the Eye — add only what earns its place.

## Tried / avoided
- Avoided swapping the accent to acid-green (would land on the exact AI-design cliché).
- Kept status colors semantic (traffic-light) — brand accent stays out of green/amber/red.
- Boldness spent on the Eye + color; type stays characterful-but-legible (it's a data UI).
