# G5 — Debug-dump log rotation

**Priority:** LOW-MED · **Surface:** debug-logging · **GAPS.md:** G5

## Purpose
Add age-based cleanup of debug/request-log dump files so the log does not grow unbounded.
Today `upstream_request_log_path` is append-only JSONL (`src/upstream.rs`) with no rotation.

## Reference (study, adapt — do NOT transliterate)
- claude-relay behavior source: `/home/jon/git/claude-relay/claude_relay/server.py` /
  debug-file cleanup (`_cleanup_debug_files`); tests `tests/test_debug_rotation.py` (8 behaviors).
- llmconduit target: `src/request_log.rs` (or a new `src/log_rotation.rs`), wired from `src/config.rs`
  + startup in `src/lib.rs` / `src/main.rs`.

## Acceptance criteria (executable)
Create `tests/port_logging.rs` (uses `tempfile` or a temp dir) porting:
- [ ] Files older than `max_age_hours` are deleted.
- [ ] Files newer than `max_age_hours` are kept.
- [ ] Only `*.json` / `*.ndjson` are eligible; other extensions are skipped.
- [ ] Missing directory → returns 0, no error.
- [ ] Subdirectories are ignored (only files deleted).
- [ ] Mixed old/recent → only old deleted.
- [ ] A removal error (race) is tolerated; cleanup continues, count not double-incremented.

## Constraints (load-bearing — see AGENTS.md)
- **Do not introduce blocking IO on the tokio runtime** — use `spawn_blocking` (the upstream request log already does this for a reason).
- Add a config knob (e.g. `debug_log_max_age_hours`) via `PersistedConfig` + env override pattern in `config.rs`; default = disabled / generous so behavior is opt-in.
- Reuse existing `dirs`/path handling; do not invent a second log-path concept.

## Dependencies
None. Isolated module — safe early/parallel candidate.

## Definition of Done
- [ ] New tests green; cleanup runs off the async runtime.
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` applied.
- [ ] Codex (xhigh) review passed — see `.ralph/REVIEW_PROTOCOL.md`.

## Principles to invoke
`principle-make-operations-idempotent` (cleanup is idempotent), `principle-boundary-discipline`, `principle-prove-it-works`.
