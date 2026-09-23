# CLI

## Main entry point (`src/main.rs`)

`src/main.rs` lines 15-90. Parses CLI via clap, initialises tracing (RUST_LOG respected, default `info`), then dispatches to the matching subcommand handler.

- No subcommand: reads config from the default path, calls the same start logic as `start` — no flags, so no ad-hoc model routes, no raw dump.
- `start`: starts the gateway server. Accepts `--raw`, `--model-route`, `--config`, plus the global `--with-debug-ui`.
- `configure`: runs the interactive dialog-based config writer.
- `analyze-log`: diffs consecutive upstream request log entries and prints a report.

Startup banner logs version, commit hash, dirty flag, and build time (`log_listening()`, main.rs:94-102).

## cli.rs definitions (`src/cli.rs`)

The `Cli` struct (cli.rs:21-27) defines a global `--with-debug-ui` flag applicable to all commands.

### Commands

| Command | Handler | Defined at | Description |
|-|-|-|-|
| `start` | inline in main.rs:47-71 | `cli.rs:32` | Start the gateway server. |
| `configure` | `run_configure_flow()` (cli.rs:69) | `cli.rs:46` | Run the interactive config wizard and write a config file. |
| `analyze-log` | `analyze_request_log()` (request_log module; dispatch main.rs:30-46) | `cli.rs:52` | Diff consecutive upstream request log entries and highlight unstable prefixes. |
| *(none)* | inline in main.rs:72-88 | — | Start the gateway server with defaults (no flags). |

### Global flags

Defined on `Cli` (cli.rs:21-27). Applicable to all commands.

| Flag | Type | Default | Description |
|-|-|-|-|
| `--with-debug-ui` | `bool` | `false` | Register the embedded request debug UI at `/debug` and `/dashboard`. Registration can be refused at startup (non-loopback bind without a token + validated https origin); the outcome is logged (`log_debug_ui_status()`, main.rs:110-126). |

## start

Starts the gateway HTTP server. Handler lives inline in `src/main.rs` at lines 47-71: loads config (env + file + CLI routes), validates startup auth env, spawns debug-log cleanup, binds, and serves.

| Flag | Type | Default | Description |
|-|-|-|-|
| `--config` | `Option<PathBuf>` | `~/.config/llmconduit/config.yaml` | Path to the config file. |
| `--raw` | `bool` | `false` | Dump raw model delta text to the terminal while the gateway is running. Tracing log output is suppressed (sent to a sink) so it does not interleave with the raw stream. |
| `--model-route` | `Vec<String>` (repeatable) | `[]` | Ad-hoc model route `NAME=URL[,UPSTREAM_MODEL]`. NAME may be a glob (e.g. `local-*`). Merged after config and env (CLI wins); a malformed spec is a clean startup error. |

The same start logic runs for the no-subcommand case (main.rs:72-88) with no flags set — default config path, no raw dump, no ad-hoc routes (env + file config only).

## configure

Runs an interactive wizard via `dialoguer`. Prompts for:

- Bind address
- Upstream chat-completions base URL
- Upstream API key (password input, blank = no auth; existing key kept via confirm prompt)
- Upstream model override (blank = pass through)
- Upstream request JSONL log path (blank = disabled)
- Extra upstream chat kwargs (JSON object, blank = none)
- Brave Search base URL
- Brave Search API key (blank = disable provider-side web_search)
- Brave max results
- Request timeout (seconds)

All other `PersistedConfig` fields (upstream pools, retry/circuit-breaker/bulkhead settings, model routes, profiles, response store, limits, image/vision settings, price table, etc.) are carried through from the existing config file unchanged — the wizard never prompts for or resets them.

Writes the resulting `PersistedConfig` to the config path on confirmation; declining the write prompt cancels with an error.

Handler: `run_configure_flow()` (cli.rs:69-236).

## analyze-log

Diffs consecutive entries in a JSONL request log and prints instability highlights.

Handler: `analyze_request_log()` from the `request_log` module, dispatched at main.rs:30-46. Fails with a clear error if no log path is given and none is configured.

| Flag | Type | Default | Description |
|-|-|-|-|
| `--config` | `Option<PathBuf>` | `~/.config/llmconduit/config.yaml` | Path to the config file. |
| `--path` | `Option<PathBuf>` | `upstream_request_log_path` from config | Path to the JSONL request log. |
| `--pairs` | `usize` | `10` | Maximum number of consecutive pairs to report. |
