# Test Port: claude-relay → llmconduit

Context document for porting unit tests from `~/git/claude-relay` (Python proxy) into `~/git/local-inference-lab/llmconduit` (Rust proxy). Load this in a fresh Claude session started from the llmconduit directory.

## Goal

**Outcome = gap inventory**, not test parity. Each ported test either passes (feature exists), fails (translation bug, fix root cause), or is marked `#[ignore = "GAP: <name>"]` (feature missing in llmconduit). The deliverable is a categorized list of missing functionality, ranked by surface.

Do NOT implement features just to make tests pass. The gaps ARE the data.

## Source / target

| | Source | Target |
|-|-|-|
| Path | `/home/jon/git/claude-relay` | `/home/jon/git/local-inference-lab/llmconduit` (CWD) |
| Lang | Python (pytest) | Rust (cargo test) |
| Role | OpenAI ↔ Responses API proxy | Same, Rust rewrite |
| Tests | `tests/test_*.py` (8 modules) | `tests/gateway.rs` (1 integration) |

Source test modules:
- `test_backend.py`
- `test_compat.py`
- `test_config.py`
- `test_convert_request.py`
- `test_convert_stream.py`
- `test_debug_rotation.py`
- `test_image_agent.py`
- `test_server.py`

## Strategy

**Port model: contract-level**, not literal transpile.

Rejected:
- Literal 1:1 transpile (drags Python idioms, jest-style mocks, internal-impl tests — high noise)
- Golden-file replay (useful later; not the primary axis)

Chosen:
- Read each pytest assertion as a behavior statement
- Re-express in Rust using `tokio::test`, `wiremock`/`httpmock`, `serde_json::json!`
- Group tests by **proxy surface**, not by source filename

Surfaces (organize Rust test tree this way):
- Request translation (OpenAI → Responses API)
- Response translation (Responses API → OpenAI)
- Streaming SSE frames
- Error mapping
- Tool-call translation
- Config loading
- Backend selection / routing
- Debug logging / rotation
- Image agent (if applicable)

## Workflow (per suite)

1. Explore agent reads `/home/jon/git/claude-relay/tests/test_<surface>.py` and returns assertion inventory (one row per behavior). Don't read in main thread — burns context.
2. Cross-check llmconduit `src/` for existing capability (Explore agent, scoped).
3. For each assertion:
   - Capability exists → write Rust test
   - Capability missing → write Rust test stub with `#[ignore = "GAP: <surface>/<feature>"]`
   - Translation bug suspected → write test, run, fix root cause if found (`principle-fix-root-causes`)
4. `cargo test` after each suite. Green or red-with-reason. No silent skips.
5. Append findings to `GAPS.md` (one row per `#[ignore]`).

## Principles in play (loaded as Claude Code skills)

Already ported to `~/.claude/skills/principle-*` — invoke by name when relevant:

| Principle | Use during this task |
|-|-|
| `principle-outcome-oriented-execution` | Outcome = gap report, not test parity |
| `principle-laziness-protocol` | Missing feature? Mark `#[ignore]`, do NOT implement |
| `principle-foundational-thinking` | Understand assertion intent before translating |
| `principle-exhaust-the-design-space` | Considered 3 port models; chose contract-level |
| `principle-sequence-verifiable-units` | One surface = one test module = one runnable unit |
| `principle-prove-it-works` | Each test runs green or red-with-clear-reason |
| `principle-guard-the-context-window` | Delegate exploration to Explore agent |
| `principle-build-the-lever` | If >10 tests share fixtures, build fixture loader crate first |
| `principle-subtract-before-you-add` | Drop Python-only tests (DI quirks, pytest fixtures, internal-impl) |
| `principle-fix-root-causes` | Test fails for translation bug → fix the mapper, not the test |
| `principle-boundary-discipline` | Proxy = pure boundary; cluster tests at HTTP-in / HTTP-out / SSE / JSON |
| `principle-separate-before-serializing-shared-state` | State-transition tests separate from translation tests |
| `principle-encode-lessons-in-structure` | Gap inventory → `GAPS.md` with surface tags |
| `principle-redesign-from-first-principles` | Rust idioms ≠ Python; reshape mocks/fixtures |
| `principle-never-block-on-the-human` | Run autonomously, surface only ambiguous decisions |

Skip: `migrate-callers-then-delete-legacy-apis`, `make-operations-idempotent`, `experience-first` (N/A for this task).

## Post-port audit gate (thermos)

Two rubric skills ported to `~/.claude/skills/`:
- `thermo-nuclear-review` — security + correctness + breakage + feature-gate leaks
- `thermo-nuclear-code-quality-review` — strict maintainability rubric

Workflow at end of port:
1. `git diff main...HEAD` in llmconduit
2. Launch 2 parallel Agent calls, each loads one rubric, audits diff + changed files
3. Synthesize prioritized findings inline

Orchestrator skill `thermos` itself NOT ported — depended on Cursor-specific Task subagent format. The parallel pattern above replicates its effect via Claude Code's Agent tool.

## Deliverables

1. `tests/` — ported Rust test modules grouped by surface
2. `GAPS.md` — categorized inventory (surface → feature → claude-relay test ref → status → priority)
3. Any root-cause fixes to `src/` translation logic (only when test reveals a bug, not a gap)
4. Post-port thermos audit findings

## First moves (new session)

```
1. Verify CWD = /home/jon/git/local-inference-lab/llmconduit
2. cat AGENTS.md llmconduit-architecture.md  (load project conventions)
3. Spawn Explore agent: map /home/jon/git/claude-relay/tests/ assertion inventory by surface
4. Spawn Explore agent: map llmconduit src/ current capability by surface
5. Cross-table → preliminary gap map BEFORE porting any test
6. Pick smallest surface first (probably config or error-mapping) to validate workflow
```

## Non-goals

- 100% test parity
- Porting pytest fixtures verbatim
- Porting Python-specific helper modules
- Implementing missing features (that's the next project)
- Refactoring llmconduit src/ (only minimal fixes when tests reveal translation bugs)

## Open questions to resolve in new session

- Does llmconduit have a fixture / test-helper convention already? Check `tests/gateway.rs` and any `tests/common/` module.
- Is there a preferred HTTP mocking crate already in `Cargo.toml`? (`wiremock` vs `httpmock` vs `mockito`)
- Are there features behind cargo features flags that gate behavior? (Affects which tests can run unconditionally.)
- Streaming: does llmconduit emit SSE via `axum::response::sse`, `eventsource-stream`, or custom? Affects assertion shape.
