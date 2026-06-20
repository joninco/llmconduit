//! Ported from claude-relay `tests/test_debug_rotation.py` (8 behaviors of
//! `_cleanup_debug_files`), adapted to llmconduit's `cleanup_dump_files`.
//!
//! The core function takes an injected `now: SystemTime`, so most behaviors are
//! exercised by passing a cutoff in the future to age out freshly-created files
//! without backdating mtimes. The "mixed" and "race" cases need two files with
//! different ages in one call, so those backdate one file via `std::fs::FileTimes`
//! (no extra dev-dependency required).

use llmconduit::log_rotation::cleanup_dump_files;
use llmconduit::log_rotation::cleanup_dump_files_with_remover;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::SystemTime;

const MAX_AGE: Duration = Duration::from_secs(2 * 3600);

/// Unique temp directory under the OS temp dir; removed on drop. Uses the
/// existing `uuid` dependency rather than adding `tempfile`.
struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "llmconduit-port-logging-{}",
            uuid::Uuid::new_v4().simple()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn write(&self, name: &str) -> PathBuf {
        let path = self.path.join(name);
        fs::write(&path, "{}\n").expect("write file");
        path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// A `now` 10h in the future: a just-written file (current mtime) is older than
/// `MAX_AGE` without touching its mtime.
fn now_in_future() -> SystemTime {
    SystemTime::now() + Duration::from_secs(10 * 3600)
}

/// Backdate a file's modification time using std only.
fn backdate(path: &Path, by: Duration) {
    let when = SystemTime::now() - by;
    let times = fs::FileTimes::new().set_modified(when);
    let file = fs::File::options()
        .write(true)
        .open(path)
        .expect("open file to set times");
    file.set_times(times).expect("set file times");
}

#[test]
fn deletes_old_files() {
    let dir = TempDir::new();
    let old = dir.write("old.json");

    let deleted = cleanup_dump_files(dir.path(), MAX_AGE, now_in_future());

    assert_eq!(deleted, 1);
    assert!(!old.exists());
}

#[test]
fn keeps_recent_files() {
    let dir = TempDir::new();
    let recent = dir.write("recent.json");

    // Real now: a just-written file is well within MAX_AGE.
    let deleted = cleanup_dump_files(dir.path(), MAX_AGE, SystemTime::now());

    assert_eq!(deleted, 0);
    assert!(recent.exists());
}

#[test]
fn ignores_non_json_files() {
    let dir = TempDir::new();
    let other = dir.write("old.txt");

    let deleted = cleanup_dump_files(dir.path(), MAX_AGE, now_in_future());

    assert_eq!(deleted, 0);
    assert!(other.exists());
}

#[test]
fn deletes_old_ndjson_files() {
    let dir = TempDir::new();
    let old = dir.write("old.ndjson");

    let deleted = cleanup_dump_files(dir.path(), MAX_AGE, now_in_future());

    assert_eq!(deleted, 1);
    assert!(!old.exists());
}

#[test]
fn handles_missing_directory() {
    let missing = std::env::temp_dir().join(format!(
        "llmconduit-port-logging-missing-{}",
        uuid::Uuid::new_v4().simple()
    ));
    assert!(!missing.exists());

    let deleted = cleanup_dump_files(&missing, MAX_AGE, now_in_future());

    assert_eq!(deleted, 0);
}

#[test]
fn handles_subdirectories() {
    let dir = TempDir::new();
    // A subdirectory, even with an eligible-looking name, must be ignored.
    let subdir = dir.path().join("nested.json");
    fs::create_dir(&subdir).expect("create subdir");

    let deleted = cleanup_dump_files(dir.path(), MAX_AGE, now_in_future());

    assert_eq!(deleted, 0);
    assert!(subdir.is_dir());
}

#[test]
fn mixed_old_and_recent_files() {
    let dir = TempDir::new();
    let old = dir.write("old.json");
    let recent = dir.write("recent.json");

    // Backdate only `old` by 3h; with a 2h max_age and real `now`, only `old`
    // ages out while `recent` (just written) is kept.
    backdate(&old, Duration::from_secs(3 * 3600));

    let deleted = cleanup_dump_files(dir.path(), MAX_AGE, SystemTime::now());

    assert_eq!(deleted, 1);
    assert!(!old.exists());
    assert!(recent.exists());
}

#[test]
fn tolerates_removal_race_without_double_count() {
    let dir = TempDir::new();
    // Two eligible old files. A concurrent deletion race is modeled by an
    // injected remover that fails on its FIRST invocation (as if that file
    // vanished between the directory read and the `remove_file` call) and really
    // removes on every later invocation. This is ORDER-INDEPENDENT: `read_dir`
    // yields the two files in an unspecified order, but whichever comes first
    // gets the `Err`, so a successful removal PROVABLY follows a failed one —
    // exercising the loop's continue-after-Err path regardless of filesystem
    // ordering. (Pre-deleting a file, or failing a fixed filename, would not
    // prove continuation if the successful removal happened to run first.)
    let a = dir.write("a.json");
    let b = dir.write("b.json");
    backdate(&a, Duration::from_secs(3 * 3600));
    backdate(&b, Duration::from_secs(3 * 3600));

    let mut remove_attempts: Vec<PathBuf> = Vec::new();
    let deleted = cleanup_dump_files_with_remover(dir.path(), MAX_AGE, SystemTime::now(), |path| {
        remove_attempts.push(path.to_path_buf());
        if remove_attempts.len() == 1 {
            // The first eligible file raced away under us: removal fails.
            Err(io::Error::new(io::ErrorKind::NotFound, "raced away"))
        } else {
            fs::remove_file(path)
        }
    });

    // Both eligible files were attempted (the loop genuinely continued past the
    // first, failed removal), and only the second — which actually succeeded —
    // is counted: no double-increment for the file that raced away.
    assert_eq!(
        remove_attempts.len(),
        2,
        "cleanup must attempt BOTH eligible files (continue past the failed removal): {remove_attempts:?}"
    );
    assert_eq!(deleted, 1, "only the successful removal is counted");

    // The first-attempted file failed removal, so it still exists; the
    // second-attempted file was really removed. Asserting via attempt ORDER (not
    // a fixed name) keeps this independent of `read_dir` ordering.
    let failed = &remove_attempts[0];
    let removed = &remove_attempts[1];
    assert!(
        failed.exists(),
        "the file whose removal returned Err must remain: {}",
        failed.display()
    );
    assert!(
        !removed.exists(),
        "the file removed after the failed one must be gone: {}",
        removed.display()
    );
    // Sanity: exactly one of the two eligible files survives.
    assert_eq!(
        [a.exists(), b.exists()].iter().filter(|&&e| e).count(),
        1,
        "exactly one of the two eligible files must survive"
    );
}
