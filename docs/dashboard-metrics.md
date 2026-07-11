# Dashboard metric semantics

This document defines the shipped schema-v3 statistical contract and its audit checklist.
The REST, WebSocket, generated TypeScript, standalone validators, and dashboard consumers
share this version; a schema mismatch continues to use the existing reload/upgrade error path.

Unless a row says
otherwise, a flow enters a time window at its terminal timestamp and windows are
sliding `m1` (60 s), `m5` (300 s), and `h1` (3600 s) rings with one-second slots.
Dashboard metrics are diagnostic telemetry, not billing records.

## Global metrics strip

The strip is gateway-global and is not affected by URL flow filters. The UI displays a
`Global` badge to make that scope explicit.

| UI label | Event source and inclusion | Formula / unit | Availability and quality |
|---|---|---|---|
| inbound/s | FlowStore open for accepted POSTs to `/v1/responses`, `/v1/messages`, or `/v1/chat/completions`; recorded before the request terminates | accepted starts / observed window seconds | measured; oversized and pre-flow rejections are excluded |
| done/s | terminal finalize, exactly once per flow | completed + failed + cancelled terminals / observed window seconds | measured |
| active now | open FlowStore records at the published cut | count, not a window aggregate | measured; always available |
| fail % | failed terminal flows | `100 * failures / terminal requests` | derived; unavailable with no terminals; cancellations are not failures |
| cancel % | cancelled terminal flows | `100 * cancellations / terminal requests` | derived; unavailable with no terminals |
| p50/p95/p99 e2e ms | monotonic terminal elapsed time | nearest-rank over a 128-bucket logarithmic histogram from 1 ms through 1 hour | p50 needs 2 samples, p95 20, p99 100; geometric midpoint estimate, at most 6.2% in-range relative value error; clamped to observed min/max; overflow or saturation is partial |
| reported tok/s | terminal usage reports | sum of normalized prompt + completion tokens / observed seconds | derived; unavailable with no usage samples; cached and reasoning are subsets and are not added again |
| $/min | terminal-time priced usage | sum of persisted terminal costs / observed minutes | derived when fully priced, estimated for corrected or partially priced traffic, unavailable when nothing is priced |

During process warm-up, rate denominators are `min(configured window, process-observed
seconds)`. A window becomes warm after its full duration has elapsed. Exact zero means a
measured zero; `—` means unavailable. Sparklines retain null gaps rather than coercing them
to zero.

## Usage and pricing

Raw provider usage remains visible on a flow. Calculation-only normalization clamps prompt
and completion to nonnegative values, cached to `[0,prompt]`, and reasoning to
`[0,completion]`; canonical total volume is prompt plus completion. The bounded anomaly
classes are negative counts, provider-total mismatch, and invalid subclasses. Corrected
samples remain usable but reduce aggregate quality.

Prices must be finite and nonnegative. Cost is computed once at terminal finalize using the
served model. Unpriced traffic is unavailable rather than `$0.00`; mixed priced and unpriced
traffic is partial/estimated. Cached-token price impact is signed relative to charging the
same tokens at the ordinary input rate.

## Flow surfaces

Flow elapsed time is the backend monotonic elapsed duration. Phase and attempt durations retain
their captured offsets when available; legacy wall-clock pairs are used only when ordered, and
clock disorder becomes unavailable/partial rather than a measured zero. Wall-clock epochs remain
display timestamps. Stream output rate is completion tokens /
measured generation seconds and is labelled derived. Context pressure is prompt tokens /
the terminal flow's conservative effective route limit.

## Overview

Overview uses the selected window and URL scope. Totals distinguish successes, failures,
cancellations, usage samples, priced samples, context samples, and unattributable overflow.
Failure groups contain failed terminals only; cancellations have a separate rollup. An
overflowed filtered result is a lower bound and partial, while an unfiltered result includes
`__other__`. `status=open` has no terminal analytics and must return a structured 422; active
now remains available only as the global strip value.

Cost-series bins are one second for `m1`, five seconds for `m5`, and sixty seconds for `h1`.
No-traffic bins are zero; traffic with no price is null.

## Topology, Sankey, and Theater

Topology edges distinguish attempt rate, terminal-flow rate, reported terminal-token rate,
and persisted terminal-cost rate. Provider latency includes every attempt, including a failed
primary before failover, and uses the same histogram contract as global latency.

Sankey lanes are server-authored terminal rollups keyed by served provider and model for the
selected window/historical cut. Volume means reported tokens on terminal flows in that
window, not instantaneous emission. Browser-side cumulative-flow differencing is not an
authoritative statistics source.

Theater's character meter is an estimate (`characters / 4 / elapsed seconds`) labelled
`≈ tok/s`. It is unavailable until at least two ordered text timestamps exist.
