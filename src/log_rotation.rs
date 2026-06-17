//! Age-based cleanup of debug / request-log dump files.
//!
//! The upstream request log (`upstream_request_log_path`) is append-only JSONL
//! with no rotation, so the directory holding dump files can grow unbounded.
//! This module deletes eligible dump files older than a configurable max age.
//!
//! The core [`cleanup_dump_files`] function is synchronous and takes an injected
//! `now`, so tests can age files out by passing a cutoff rather than backdating
//! mtimes. Callers on the tokio runtime must invoke it via `spawn_blocking`
//! (see [`spawn_cleanup`]) because it performs blocking filesystem IO.

use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

/// File extensions eligible for cleanup. Other extensions (and extensionless
/// files) are skipped so unrelated files in the directory are never touched.
const ELIGIBLE_EXTENSIONS: [&str; 2] = ["json", "ndjson"];

/// Delete eligible dump files in `dir` whose modification time is older than
/// `max_age` relative to `now`. Returns the number of files actually deleted.
///
/// Behavior:
/// - Only top-level `*.json` / `*.ndjson` files are considered; other
///   extensions are skipped and subdirectories are ignored (never removed).
/// - A missing directory returns `0` with no error (cleanup is idempotent).
/// - A per-file metadata or removal error (e.g. a concurrent deletion race) is
///   tolerated: that file is skipped, the count is not incremented for it, and
///   cleanup continues with the remaining entries.
///
/// This is blocking IO; do not call it directly on the async runtime. Use
/// [`spawn_cleanup`] from async contexts.
pub fn cleanup_dump_files(dir: &Path, max_age: Duration, now: SystemTime) -> usize {
    // Missing directory (or a path that isn't a directory) -> nothing to do.
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };

    let cutoff = now.checked_sub(max_age);

    let mut deleted = 0usize;
    for entry in entries {
        // A failure to read a single dir entry is tolerated like a removal race.
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();

        if !is_eligible_extension(&path) {
            continue;
        }

        // Use the entry's own file-type when available to avoid an extra stat;
        // fall back to metadata. Either way, only regular files are eligible so
        // subdirectories (even with an eligible-looking name) are ignored.
        let metadata = match entry.metadata() {
            Ok(metadata) => metadata,
            Err(_) => continue,
        };
        if !metadata.is_file() {
            continue;
        }

        if is_older_than_cutoff(&metadata, cutoff) {
            // Tolerate races: only count a deletion that actually succeeded.
            match fs::remove_file(&path) {
                Ok(()) => deleted += 1,
                Err(error) => {
                    tracing::debug!(
                        path = %path.display(),
                        %error,
                        "skipping debug dump file that could not be removed"
                    );
                }
            }
        }
    }

    deleted
}

/// Run [`cleanup_dump_files`] on the blocking thread pool and log the outcome.
///
/// Returns immediately if `max_age_hours` is `None` (the feature is opt-in) or
/// if `dir` is `None`. The spawned task is detached: cleanup is best-effort and
/// must never block startup or fail the server.
pub fn spawn_cleanup(dir: Option<PathBuf>, max_age_hours: Option<u64>) {
    let (Some(dir), Some(max_age_hours)) = (dir, max_age_hours) else {
        return;
    };
    if max_age_hours == 0 {
        // Treat 0 as disabled rather than "delete everything immediately",
        // which would race with files being written this same second.
        return;
    }
    let max_age = Duration::from_secs(max_age_hours.saturating_mul(3600));
    tokio::task::spawn_blocking(move || {
        let deleted = cleanup_dump_files(&dir, max_age, SystemTime::now());
        if deleted > 0 {
            tracing::info!(
                deleted,
                dir = %dir.display(),
                "cleaned up old debug/request-log dump file(s)"
            );
        }
    });
}

fn is_eligible_extension(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .map(|extension| {
            ELIGIBLE_EXTENSIONS
                .iter()
                .any(|eligible| extension.eq_ignore_ascii_case(eligible))
        })
        .unwrap_or(false)
}

fn is_older_than_cutoff(metadata: &fs::Metadata, cutoff: Option<SystemTime>) -> bool {
    // If `now - max_age` underflowed (max_age beyond the epoch), nothing can be
    // older than the cutoff, so keep everything.
    let Some(cutoff) = cutoff else {
        return false;
    };
    match metadata.modified() {
        Ok(modified) => modified < cutoff,
        // No mtime available: be conservative and keep the file.
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::cleanup_dump_files;
    use std::fs;
    use std::time::Duration;
    use std::time::SystemTime;

    // Smoke test of the core path; exhaustive behavior coverage lives in
    // tests/port_logging.rs (the 8 ported claude-relay behaviors).
    #[test]
    fn deletes_eligible_file_older_than_cutoff() {
        let dir = std::env::temp_dir().join(format!(
            "llmconduit-log-rotation-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        let target = dir.join("dump.json");
        fs::write(&target, "{}\n").expect("write file");

        // Inject a `now` 10h in the future so the just-written file is older
        // than a 2h max_age without backdating its mtime.
        let now = SystemTime::now() + Duration::from_secs(10 * 3600);
        let deleted = cleanup_dump_files(&dir, Duration::from_secs(2 * 3600), now);

        assert_eq!(deleted, 1);
        assert!(!target.exists());

        let _ = fs::remove_dir_all(&dir);
    }
}
