use clap::Parser;
use resp2chat::build_app_with_gateway;
use resp2chat::cli::Cli;
use resp2chat::cli::Commands;
use resp2chat::cli::resolve_config_path;
use resp2chat::cli::run_configure_flow;
use resp2chat::config::Config;
use resp2chat::request_log::analyze_request_log;
use resp2chat::ui::UiHandle;
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
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
        Some(Commands::Start { config, ui }) => {
            let path = resolve_config_path(config)?;
            let config = Config::from_env_and_file(Some(&path))?;
            let bind_addr = config.bind_addr;
            let (app, gateway) = build_app_with_gateway(config);
            let listener = TcpListener::bind(bind_addr).await?;
            tracing::info!("resp2chat listening on {bind_addr}");
            tracing::info!("using config file {}", path.display());
            if ui {
                let ui = UiHandle::new(bind_addr.to_string(), gateway.subscribe_monitor());
                let mut server = tokio::spawn(async move { axum::serve(listener, app).await });
                tokio::select! {
                    server_result = &mut server => {
                        server_result
                            .map_err(|err| format!("server task failed: {err}"))?
                            .map_err(|err| format!("server failed: {err}"))?;
                    }
                    ui_result = ui.run() => {
                        ui_result?;
                        server.abort();
                    }
                }
            } else {
                axum::serve(listener, app).await?;
            }
            Ok(())
        }
        None => {
            let path = resolve_config_path(None)?;
            let config = Config::from_env_and_file(Some(&path))?;
            let bind_addr = config.bind_addr;
            let (app, _gateway) = build_app_with_gateway(config);
            let listener = TcpListener::bind(bind_addr).await?;
            tracing::info!("resp2chat listening on {bind_addr}");
            tracing::info!("using config file {}", path.display());
            axum::serve(listener, app).await?;
            Ok(())
        }
    }
}
