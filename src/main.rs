use clap::Parser;
use llmconduit::AppOptions;
use llmconduit::build_app_with_gateway_and_options;
use llmconduit::cli::Cli;
use llmconduit::cli::Commands;
use llmconduit::cli::resolve_config_path;
use llmconduit::cli::run_configure_flow;
use llmconduit::config::Config;
use llmconduit::log_rotation::spawn_cleanup;
use llmconduit::raw::RawOutput;
use llmconduit::request_log::analyze_request_log;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = Cli::parse();
    init_tracing(command_uses_dedicated_terminal(&cli.command));
    let app_options = AppOptions {
        with_debug_ui: cli.with_debug_ui,
    };

    match cli.command {
        Some(Commands::Configure { config }) => {
            let path = resolve_config_path(config)?;
            let _ = run_configure_flow(path.clone())?;
            println!("Wrote configuration to {}", path.display());
            Ok(())
        }
        Some(Commands::AnalyzeLog {
            config,
            path,
            pairs,
        }) => {
            let config_path = resolve_config_path(config)?;
            let config = Config::from_env_and_file(Some(&config_path))?;
            let log_path = path.or(config.upstream_request_log_path).ok_or_else(|| {
                format!(
                    "no request log path configured; pass --path or set upstream_request_log_path in {}",
                    config_path.display()
                )
            })?;
            let report = analyze_request_log(&log_path, pairs)?;
            println!("{report}");
            Ok(())
        }
        Some(Commands::Start { config, raw }) => {
            let path = resolve_config_path(config)?;
            let config = Config::from_env_and_file(Some(&path))?;
            let bind_addr = config.bind_addr;
            run_debug_log_cleanup(&config);
            let (app, _gateway) = build_app_with_gateway_and_options(
                config,
                raw.then(RawOutput::stdout),
                app_options,
            );
            let listener = TcpListener::bind(bind_addr).await?;
            tracing::info!("llmconduit listening on {bind_addr}");
            if app_options.with_debug_ui {
                tracing::info!("debug UI available at http://{bind_addr}/debug");
            }
            tracing::info!("using config file {}", path.display());
            axum::serve(listener, app).await?;
            Ok(())
        }
        None => {
            let path = resolve_config_path(None)?;
            let config = Config::from_env_and_file(Some(&path))?;
            let bind_addr = config.bind_addr;
            run_debug_log_cleanup(&config);
            let (app, _gateway) = build_app_with_gateway_and_options(config, None, app_options);
            let listener = TcpListener::bind(bind_addr).await?;
            tracing::info!("llmconduit listening on {bind_addr}");
            if app_options.with_debug_ui {
                tracing::info!("debug UI available at http://{bind_addr}/debug");
            }
            tracing::info!("using config file {}", path.display());
            axum::serve(listener, app).await?;
            Ok(())
        }
    }
}

/// Spawn opt-in age-based cleanup of debug/request-log dump files for each
/// configured log directory. No-op unless `debug_log_max_age_hours` is set.
/// Cleanup runs on the blocking pool, never blocking serve startup.
fn run_debug_log_cleanup(config: &Config) {
    let max_age_hours = config.debug_log_max_age_hours;
    if max_age_hours.is_none() {
        return;
    }
    for dir in config.debug_log_dirs() {
        spawn_cleanup(Some(dir), max_age_hours);
    }
}

fn init_tracing(raw_active: bool) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    if raw_active {
        tracing_subscriber::fmt()
            .with_env_filter(env_filter)
            .with_writer(std::io::sink)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }
}

fn command_uses_dedicated_terminal(command: &Option<Commands>) -> bool {
    matches!(command, Some(Commands::Start { raw: true, .. }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn detects_raw_start_command() {
        assert!(command_uses_dedicated_terminal(&Some(Commands::Start {
            config: None,
            raw: true,
        })));
    }

    #[test]
    fn does_not_suppress_logs_for_non_raw_commands() {
        assert!(!command_uses_dedicated_terminal(&None));
        assert!(!command_uses_dedicated_terminal(&Some(Commands::Start {
            config: None,
            raw: false,
        })));
        assert!(!command_uses_dedicated_terminal(&Some(
            Commands::AnalyzeLog {
                config: None,
                path: Some(PathBuf::from("/tmp/requests.jsonl")),
                pairs: 1,
            }
        )));
    }

    #[test]
    fn parses_debug_ui_flag_for_start() {
        let cli = Cli::parse_from(["llmconduit", "start", "--with-debug-ui"]);

        assert!(cli.with_debug_ui);
        assert!(matches!(cli.command, Some(Commands::Start { .. })));
    }
}
