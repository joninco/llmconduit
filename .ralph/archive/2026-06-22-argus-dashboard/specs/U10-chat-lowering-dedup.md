# U10 — Chat-lowering dedup: single tool-name authority + data-driven web_search placeholder

> **Source:** thermo-nuclear PROJECT review 2026-06-20 (Topic 12). See /tmp/thermo-project-review.md

**Priority:** LOW · **Surface:** `src/adapters/responses_to_chat.rs`, `src/vision/strip.rs (doc comment only)`, tests · **Thermo finding:** Duplicate-tool-name rejection implemented twice with divergent keying; `web_search_placeholder_result` is a ~40-line nested match re-spelling one base sentence ~6×.

## Purpose
Two adjacent dedup/cleanup opportunities in the Responses→Chat lowering path, both LOW (revised from MEDIUM), both pure refactors:

**(A) Duplicate tool-name authority.** `lower_request_with_image_agent` calls `lower_tools(&request.tools)` (`src/adapters/responses_to_chat.rs:131`) immediately followed by `build_tool_registry(&request.tools, …)` (`:132`), passing the *same* `request.tools` slice. Both reject duplicate tool names: `lower_tools` keys a raw **case-SENSITIVE** `seen_names: HashMap` (declared `:461`, checked `:578–585`), while `build_tool_registry` keys a **case-INSENSITIVE** lowercased `by_name` (checked `:643–647`). Because the case-insensitive check is strictly stricter, any duplicate the case-sensitive check would catch is *also* caught by the registry — the case-sensitive HashMap is fully subsumed. Its only observable effect today is non-deterministic error-message casing depending on which check fires first (`{name}` at `:581` vs `{name_lc}` at `:645`). Removing it makes `build_tool_registry` the single authority with no loss of validation.

**(B) `web_search_placeholder_result` template.** `web_search_placeholder_result` (`src/adapters/responses_to_chat.rs:741–780`) is a nested `match` that literally re-types the same base sentence — `"Previous web_search{ action label} completed in an earlier turn, but the original tool result is unavailable because replay state was missing."` — across ~6 arms, differing only by an optional action label (`""`, `" open_page"`, `" find_in_page"`) and which optional fields get appended (`Query:`, `URL:`, `Pattern:`). The duplication is a maintenance hazard: a wording fix must be applied in 6 places. Collapsing it to one base template plus a data-driven list of present fragments preserves the exact wire text.

These are byte-identical-on-the-wire refactors: the duplicate-name error message is reachable by an end-to-end path (registry) that already produces the *same* string format, and the placeholder text content is unchanged.

## Jobs to Be Done
- `build_tool_registry` is the single source of truth for duplicate-tool-name rejection; `lower_tools` no longer carries a parallel (subsumed) dedup map.
- `web_search_placeholder_result` is expressed as one base template + a joined `Vec<String>` of present optional fragments — no duplicated base sentence.
- Tests prove the surviving (case-insensitive) rejection and the unchanged placeholder text, asserted via the public lowering entrypoint where the report directs.

## Acceptance criteria
- [ ] **(A)** Delete the `seen_names` HashMap declaration (`:461`) and its insert/check block inside the `for tool in lowered_tools` loop (`:578–585`); `lower_tools` keeps building/sorting the `Vec<ChatTool>` but no longer rejects duplicates. `build_tool_registry`'s case-insensitive check (`:643–647`) remains the **only** duplicate-name rejection and is unchanged (KEEP the stricter case-insensitive behavior).
- [ ] Duplicate-tool-name rejection still occurs end-to-end: a `ResponsesRequest` with two `ToolSpec::Function` of the same name (and a same-name-differing-only-by-case pair) returns `AppError::bad_request` whose message starts `"duplicate tool name is not supported: "`. Verified through `lower_request` (which calls `build_tool_registry`), not through `lower_tools` directly.
- [ ] Rewrite the `duplicate_tool_name_rejected` test (`:1100–1116`, currently asserting `lower_tools(&tools).is_err()`) to assert via `lower_request` / the registry path. Add (or extend) a case-insensitive case (`"echo"` + `"ECHO"`) demonstrating the case-folding rejection that `lower_tools` never provided.
- [ ] **Stale doc reference:** since `lower_tools` no longer rejects duplicate names, update the doc comment at `src/vision/strip.rs:154` so the duplicate-name authority reads `build_tool_registry` (case-insensitively) instead of `lower_tools` — e.g. "...collide with the appended canonical tool (`build_tool_registry` rejects duplicate names, case-insensitively)". Comment-only, no code change in `strip.rs`.
- [ ] **(B)** Refactor `web_search_placeholder_result` (`:741–780`) to: (1) compute one `base` sentence string from a single action label (`""` for `Search`/`Other`/`None`, `" open_page"` for `OpenPage`, `" find_in_page"` for `FindInPage`); (2) collect present optional fragments into a `Vec<String>` (`format!("Query: {query}")`, `format!("URL: {url}")`, `format!("Pattern: {pattern}")`) in field order; (3) append nothing if the vec is empty, else append `format!(" {}", fragments.join(". "))` to `base`. Do **NOT** add a trailing period after the appended fragments.
- [ ] Preserve the `Search` query selection exactly: `query.clone().or_else(|| queries.as_ref().and_then(|queries| queries.first().cloned()))` — first `query`, else the first element of `queries`.
- [ ] The produced placeholder strings are **byte-identical** to current output for every action shape: `Search` (with `query`, with only `queries`, with neither), `OpenPage` (with/without `url`), `FindInPage` (url+pattern, url-only, pattern-only, neither), `Other`, and `None`. Add a test asserting full-string equality (not just `.contains`) for at least the `FindInPage{url,pattern}` and bare-`Other` shapes; keep/extend `web_search_placeholder_result_all_actions` (`:1197–1223`).
- [ ] `cargo test` green · `cargo clippy --all-targets` clean · `cargo fmt` · Codex-xhigh APPROVED.

## Integration points
- **Extends:** `src/adapters/responses_to_chat.rs` (`lower_tools`, `build_tool_registry`, `web_search_placeholder_result`) and its `#[cfg(test)]` module (`duplicate_tool_name_rejected`, `web_search_placeholder_result_all_actions`); `src/vision/strip.rs (doc comment only)` — fix the stale `lower_tools` duplicate-name reference at `:154`.
- **New APIs:** none — `lower_tools`, `build_tool_registry`, and `web_search_placeholder_result` keep their current signatures.
- **Depends on:** nothing. Independent of other Topic-12 tasks; touches only the lowering adapter.

## Constraints
- The duplicate-name error **message format** must remain `"duplicate tool name is not supported: {name}"` (registry uses the lowercased `{name_lc}`), so no caller/test that string-matches the prefix breaks; the wire contract is unchanged.
- `build_tool_registry`'s case-insensitive keying and the G4 `ImageAnalysis` classification arm (`:599–604`) must remain untouched.
- `web_search_placeholder_result` output must be byte-identical for all action shapes — this is NOT a wire-contract fix; the placeholder text is a tool-result string already shipped to the model. No trailing period after appended fragments.
- `lower_tools` must still sort tools by name and emit the same `Vec<ChatTool>` it does today (it only loses the dedup map).

## Out of scope
- Changing the duplicate-name rejection semantics (e.g. allowing case-distinct duplicates) — the case-insensitive behavior is intentionally retained.
- Altering placeholder wording, the `web_search_arguments` helper, or any other lowering arm.
- Re-raising adjudicated items (G8 `emit_thinking`, G5 `.jsonl`) or touching Topic-11 refactors.

## Definition of done
- [ ] Acceptance criteria green; Codex-xhigh APPROVED.
