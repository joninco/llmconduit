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
use llmconduit::log_rotation::cleanup_orphan_work_dirs;
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

/// Backdate a DIRECTORY's modification time. A directory fd can only be opened
/// read-only, but `futimens` still honors an explicit `set_modified` for the owner,
/// so a plain `File::open` suffices (unlike [`backdate`], which needs write access).
fn backdate_dir(path: &Path, by: Duration) {
    let when = SystemTime::now() - by;
    let times = fs::FileTimes::new().set_modified(when);
    let dir = fs::File::open(path).expect("open dir to set times");
    dir.set_times(times).expect("set dir times");
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

// ---------------------------------------------------------------------------
// F1f AC-17 — durable turn capture rotation: the SAME `debug_log_max_age_hours`
// window prunes aged `<id>.json` artifacts (via `cleanup_dump_files`) AND sweeps
// aged orphan `.work/<id>/` section dirs (via `cleanup_orphan_work_dirs`), while
// leaving a fresh artifact and an in-flight (freshly-written) `.work/<id>/`
// untouched.
// ---------------------------------------------------------------------------

/// AC-17: in one rotation pass over a `turn_capture_dir`, an AGED published
/// `<id>.json` is pruned AND an AGED orphan `.work/<id>/` (crash residue) is swept,
/// while a FRESH artifact and an in-flight `.work/<id>/` (fresh section file) are
/// BOTH kept. Uses real `now` + backdating so aged and fresh entries coexist under
/// one cutoff (the "mixed" pattern), exactly as production runs it.
#[test]
fn ac17_rotation_prunes_aged_artifact_and_sweeps_aged_orphan_work_dir() {
    let dir = TempDir::new();
    let work_root = dir.path().join(".work");

    // (1) Aged, published artifact -> pruned by the file cleanup.
    let aged_artifact = dir.write("api_old.json");
    backdate(&aged_artifact, Duration::from_secs(3 * 3600));
    // (2) Fresh, published artifact (just written) -> kept.
    let fresh_artifact = dir.write("api_new.json");

    // (3) Aged ORPHAN work dir (a crash killed the process before finalize deleted
    // it): backdate BOTH the section file and the dir itself so its most-recent
    // activity predates the cutoff.
    let aged_work = work_root.join("api_old");
    fs::create_dir_all(&aged_work).expect("create aged work dir");
    let aged_section = aged_work.join("inbound_request");
    fs::write(&aged_section, b"{\"model\":\"m\"}").expect("write aged section");
    backdate(&aged_section, Duration::from_secs(3 * 3600));
    backdate_dir(&aged_work, Duration::from_secs(3 * 3600));

    // (4) In-flight work dir: a turn actively streaming, with a FRESH section file
    // -> must be kept even though rotation is running.
    let live_work = work_root.join("api_live");
    fs::create_dir_all(&live_work).expect("create live work dir");
    fs::write(live_work.join("served_response"), b"event: partial\n\n")
        .expect("write live section");

    // One pass, real `now` (so fresh entries stay), 2h window.
    let now = SystemTime::now();
    let deleted = cleanup_dump_files(dir.path(), MAX_AGE, now);
    let swept = cleanup_orphan_work_dirs(dir.path(), MAX_AGE, now);

    // Artifact prune: only the aged `<id>.json` is removed.
    assert_eq!(deleted, 1, "only the aged artifact is pruned");
    assert!(!aged_artifact.exists(), "aged <id>.json pruned by rotation");
    assert!(fresh_artifact.exists(), "fresh <id>.json kept");

    // Orphan sweep: only the aged `.work/<id>/` is reclaimed; the in-flight one
    // (fresh section mtime) survives.
    assert_eq!(swept, 1, "only the aged orphan .work dir is swept");
    assert!(!aged_work.exists(), "aged orphan .work/<id>/ swept");
    assert!(
        live_work.exists(),
        "in-flight .work/<id>/ (fresh section) is NEVER swept"
    );
}

/// AC-17 (in-flight protection detail): an orphan `.work/<id>/` whose DIR mtime aged
/// out but that still holds a FRESHLY-written section file is NOT swept — the newest
/// activity among the dir's contents keeps it alive. This is the exact shape of a
/// long-running turn that outlived the window while still streaming.
#[test]
fn ac17_orphan_sweep_keeps_dir_with_a_fresh_section_file() {
    let dir = TempDir::new();
    let work = dir.path().join(".work").join("api_streaming");
    fs::create_dir_all(&work).expect("create work dir");
    // The dir itself is old...
    backdate_dir(&work, Duration::from_secs(5 * 3600));
    // ...but a section file was just written (the turn is still live).
    fs::write(work.join("served_response"), b"still streaming").expect("write section");

    let swept = cleanup_orphan_work_dirs(dir.path(), MAX_AGE, SystemTime::now());

    assert_eq!(swept, 0, "a dir with any fresh section file is not swept");
    assert!(work.exists(), "the in-flight work dir survives");
}

/// AC-17 (safety): the sweep is a no-op when there is no `.work/` subtree at all —
/// e.g. a `turn_capture_dir` before any turn has registered. Defense-in-depth: even
/// though `spawn_cleanup` now scopes the destructive sweep to `turn_capture_dir` ALONE
/// (F1f review r1 — never the request-log dirs), a capture dir with no crash residue
/// still cleanly reclaims nothing.
#[test]
fn ac17_orphan_sweep_noop_without_work_subtree() {
    let dir = TempDir::new();
    dir.write("api_done.json");
    let swept = cleanup_orphan_work_dirs(dir.path(), MAX_AGE, now_in_future());
    assert_eq!(swept, 0, "no `.work/` subtree -> nothing to sweep");
}
