# CLI

## Main entry point (`src/main.rs`)

`src/main.rs` lines 16-82. Parses CLI via clap, initialises tracing, then dispatches to the matching subcommand handler.

- No subcommand: reads config, calls same start logic as `start` — no flags, just default config.
- `start`: starts the gateway server. Accepts `--raw`, `--model-route`, `--config`, `--with-debug-ui`.
- `configure`: runs the interactive dialog-based config writer.
- `analyze-log`: diffs consecutive upstream request log entries and prints a report.

## cli.rs definitions (`src/cli.rs`)

The `Cli` struct (lines 21-27) defines a global `--with-debug-ui` flag applicable to all commands.

### Commands

| Command | Handler | Defined at | Description |
|-|-|-|-|
| `start` | inline in main.rs | `cli.rs:32` | Start the gateway server. |
| `configure` | `run_configure_flow()` (cli.rs:69) | `cli.rs:46` | Run the interactive config wizard and write a config file. |
| `analyze-log` | `analyze_request_log()` (request_log module) | `cli.rs:52` | Diff consecutive upstream request log entries and highlight unstable prefixes. |
| *(none)* | inline in main.rs:68-80 | — | Start the gateway server with defaults (no flags). |

### Global flags

Defined on `Cli` (cli.rs:21-27). Applicable to all commands.

| Flag | Type | Default | Description |
|-|-|-|-|
| `--with-debug-ui` | `bool` | `false` | Enable the embedded request debug UI at `/debug` and `/dashboard`. |

## start

Starts the gateway HTTP server. Handler lives inline in `src/main.rs` at lines 32-66.

| Flag | Type | Default | Description |
|-|-|-|-|
| `--config` | `Option<PathBuf>` | `~/.config/llmconduit/config.yaml` | Path to the config file. |
| `--raw` | `bool` | `false` | Dump raw model delta text to the terminal while the gateway is running. |
| `--model-route` | `Vec<String>` (repeatable) | `[]` | Ad-hoc model route `NAME=URL[,UPSTREAM_MODEL]`. Repeatable. Merged after config/env; CLI wins. |

The same start logic runs for the no-subcommand case (main.rs:68-80) with no flags set — default config path, no raw dump, no ad-hoc routes.

## configure

Runs an interactive wizard via `dialoguer`. Prompts for:

- Bind address
- Upstream chat-completions base URL
- Upstream API key (password input, blank = no auth)
- Upstream model override (blank = pass through)
- Upstream request JSONL log path (blank = disabled)
- Extra upstream chat kwargs (JSON object, blank = none)
- Brave Search base URL
- Brave Search API key (blank = disable provider-side web_search)
- Brave max results
- Request timeout (seconds)

Writes the resulting `PersistedConfig` to the config path on confirmation.

Handler: `run_configure_flow()` (cli.rs:46-72).

## analyze-log

Diffs consecutive entries in a JSONL request log and prints instability highlights.

Handler: `analyze_request_log()` from the `request_log` module (cli.rs:82-95).

| Flag | Type | Default | Description |
|-|-|-|-|
| `--config` | `Option<PathBuf>` | `~/.config/llmconduit/config.yaml` | Path to the config file. |
| `--path` | `Option<PathBuf>` | `upstream_request_log_path` from config | Path to the JSONL request log. |
| `--pairs` | `usize` | `10` | Maximum number of consecutive pairs to report. |
