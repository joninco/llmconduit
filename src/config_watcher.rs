use crate::config::Config;
use crate::engine::Gateway;
use crate::reload_gateway_config;
use notify::Event;
use notify::EventKind;
use notify::RecommendedWatcher;
use notify::RecursiveMode;
use notify::Watcher;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

const RELOAD_DEBOUNCE: Duration = Duration::from_millis(200);

/// Start monitoring the configuration file for changes.
///
/// The containing directory is watched so saves implemented as an atomic
/// rename are observed too. Invalid or half-written files never replace the
/// currently running configuration; a subsequent successful save retries the
/// reload automatically.
pub fn watch_config_file(
    path: PathBuf,
    gateway: Arc<Gateway>,
) -> Result<tokio::task::JoinHandle<()>, String> {
    let config_path = absolute_path(&path)?;
    let watch_dir = config_path
        .parent()
        .ok_or_else(|| format!("config path has no parent: {}", config_path.display()))?
        .to_path_buf();
    std::fs::create_dir_all(&watch_dir).map_err(|err| {
        format!(
            "failed to create {} for config monitoring: {err}",
            watch_dir.display()
        )
    })?;

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut watcher: RecommendedWatcher = notify::recommended_watcher(move |event| {
        let _ = tx.send(event);
    })
    .map_err(|err| format!("failed to initialize config file monitor: {err}"))?;
    watcher
        .watch(&watch_dir, RecursiveMode::NonRecursive)
        .map_err(|err| format!("failed to monitor {}: {err}", watch_dir.display()))?;

    tracing::info!(path = %config_path.display(), "monitoring config file for changes");
    Ok(tokio::spawn(async move {
        // Keep the watcher alive for as long as the task is alive.
        let _watcher = watcher;
        while let Some(event) = rx.recv().await {
            match event {
                Ok(event) if event_affects_config(&event, &config_path) => {
                    wait_for_config_write(&mut rx, &config_path).await;
                    reload_config(&config_path, &gateway).await;
                }
                Ok(_) => {}
                Err(err) => tracing::warn!(
                    path = %config_path.display(),
                    error = %err,
                    "config file monitor error"
                ),
            }
        }
    }))
}

async fn wait_for_config_write(
    rx: &mut mpsc::UnboundedReceiver<notify::Result<Event>>,
    config_path: &Path,
) {
    let delay = tokio::time::sleep(RELOAD_DEBOUNCE);
    tokio::pin!(delay);
    loop {
        tokio::select! {
            _ = &mut delay => break,
            event = rx.recv() => match event {
                Some(Ok(event)) if event_affects_config(&event, config_path) => {
                    delay.as_mut().reset(tokio::time::Instant::now() + RELOAD_DEBOUNCE);
                }
                Some(Ok(_)) => {}
                Some(Err(err)) => tracing::warn!(
                    path = %config_path.display(),
                    error = %err,
                    "config file monitor error"
                ),
                None => break,
            },
        }
    }
}

async fn reload_config(config_path: &Path, gateway: &Gateway) {
    let previous_bind_addr = gateway.config().bind_addr;
    let config = match Config::from_env_and_file(Some(config_path)) {
        Ok(config) => config,
        Err(err) => {
            tracing::warn!(
                path = %config_path.display(),
                error = %err,
                "config reload rejected; keeping the last known-good configuration"
            );
            return;
        }
    };
    let configured_bind_addr = config.bind_addr;
    if let Err(err) = reload_gateway_config(gateway, config).await {
        tracing::warn!(
            path = %config_path.display(),
            error = %err,
            "config reload failed; keeping the last known-good configuration"
        );
        return;
    }
    if configured_bind_addr != previous_bind_addr {
        tracing::warn!(
            current_bind_addr = %previous_bind_addr,
            configured_bind_addr = %configured_bind_addr,
            "config reloaded, but bind_addr changes require a restart"
        );
    }
    tracing::info!(path = %config_path.display(), "config reloaded");
}

fn event_affects_config(event: &Event, config_path: &Path) -> bool {
    event.need_rescan()
        || (matches!(
            event.kind,
            EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
        ) && event
            .paths
            .iter()
            .filter_map(|path| absolute_path(path).ok())
            .any(|path| path == config_path))
}

fn absolute_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path.to_path_buf())
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .map_err(|err| format!("failed to resolve {}: {err}", path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::event_affects_config;
    use notify::Event;
    use notify::EventKind;
    use notify::event::AccessKind;
    use notify::event::ModifyKind;
    use std::path::PathBuf;

    #[test]
    fn filters_events_to_relevant_config_file_changes() {
        let config_path = std::env::temp_dir().join("llmconduit-config-watcher-test.yaml");
        let changed = Event::new(EventKind::Modify(ModifyKind::Any)).add_path(config_path.clone());
        assert!(event_affects_config(&changed, &config_path));

        let other = Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path(PathBuf::from("/tmp/another-file.yaml"));
        assert!(!event_affects_config(&other, &config_path));

        let access = Event::new(EventKind::Access(AccessKind::Any)).add_path(config_path.clone());
        assert!(!event_affects_config(&access, &config_path));
    }
}
