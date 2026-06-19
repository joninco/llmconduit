# G7 — Config: glob routes / CLI --model-route / TOML

**Priority:** LOW · **Surface:** config-loading / CLI · **GAPS.md:** G7
"Different-by-design" routing. Port the remaining claude-relay config behaviors to llmconduit's YAML-upstreams
model WITHOUT regressing the existing `canonical_model_key` + exposed-alias routing or the profile chain.

## Already done — do NOT redo
`template_family` config field was added during G2 (profile-chain override). Confirm it covers
claude-relay's `template_family` (default "auto") behavior; only extend if a gap remains.

## Remaining scope (3 sub-features)
1. **Glob route keys** (`claude-opus-*`): allow glob patterns where llmconduit matches a request model to a
   route/profile. claude-relay matched `model_routes` keys by glob. In llmconduit the analogue is profile
   matching (`config.rs` resolve_* against resolved-catalog / upstream-remap / request model) and/or upstream
   selection. Add glob matching at the correct seam. PRECEDENCE: exact id wins → glob match → default
   (mirror claude-relay exact-before-glob-before-default; matches AGENTS.md "Exact model id wins").
2. **`--model-route "name=url,upstream"` CLI flag**: a clap flag that injects an ad-hoc route/upstream at
   startup without editing YAML — parse `name=url,upstream`, merge into `Config` (a synthetic upstream/route),
   layered with the documented config-resolution order (env > … ). Validate the spec string; reject malformed.
3. **TOML config**: accept TOML in addition to YAML for the config file (serde already derives both). Detect
   format by file extension (`.toml` vs `.yaml`/`.yml`); keep YAML behavior byte-identical.

## Reference (study, adapt — do NOT transliterate)
- claude-relay: `/home/jon/git/claude-relay/claude_relay/` — `model_routes`, glob route keys,
  `--model-route` CLI, TOML loading. Tests: `test_config.py` (glob, cli_spec, template_family, toml) ~4 of 7.
- llmconduit: `src/config.rs` (`Config`/`PersistedConfig`, profile resolution, env overrides; resolution
  order = global → matched-profile templates (`extends:`) → matched profile → explicit request fields),
  `src/cli.rs` (clap + interactive configure), `src/upstream.rs` (RoutingUpstreamClient), AGENTS.md
  "Config resolution order".

## Constraints (load-bearing — AGENTS.md)
- Exact model id wins; normalized-alias routing via `canonical_model_key` only succeeds when it maps to one
  unique id. Globs slot BETWEEN exact and default — never override an exact match.
- Blank/missing/unavailable/ambiguous model → first model of first non-empty provider catalog (unchanged).
- Profiles considered against resolved catalog model, upstream-remap target, original request model,
  de-duped in that order; later matches override earlier (keep this for glob matches too).
- Don't add CI/CD or new top-level files without asking; don't break the YAML path.

## Acceptance criteria (executable — `tests/port_config.rs` or extend existing)
- Glob route key (`claude-opus-*`) matches a request model and routes to the configured upstream/profile;
  exact id still beats an overlapping glob; no glob match falls back to default.
- `--model-route "name=url,upstream"` parses, injects the route, and routes a matching request; malformed
  spec → clean startup error (not a panic).
- A `.toml` config loads with identical semantics to the equivalent YAML (round-trip a representative config).
- `template_family` (G2) still resolves through the profile chain (regression guard).

## Definition of Done
Tests green · `cargo test` whole suite green · `cargo clippy --all-targets` clean · `cargo fmt` ·
**Codex-xhigh review APPROVED** (`.ralph/REVIEW_PROTOCOL.md`) · commit. Obey AGENTS.md hard rules + config order.
