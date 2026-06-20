//! Build script: embed git provenance (short commit, working-tree dirty flag,
//! UTC build time) into the binary via `cargo:rustc-env`, so `--version` and the
//! startup log can report exactly which source a running process was built from.
//!
//! All three vars are emitted unconditionally (falling back to `unknown`) so the
//! `env!` reads in `src/lib.rs` always resolve, including builds from a tarball
//! with no `.git` or no `git`/`date` on PATH.

use std::process::Command;

fn main() {
    // Re-run when HEAD moves (new commit) or the index changes (staged edits),
    // keeping the embedded provenance in sync with the build.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");

    let git_hash =
        run(&["git", "rev-parse", "--short", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // `--untracked-files=no`: dirty means a TRACKED file differs from HEAD, so a
    // stray scratch/log file in the working tree does not flip the provenance flag.
    let dirty = match run(&["git", "status", "--porcelain", "--untracked-files=no"]) {
        Some(out) => !out.is_empty(),
        None => false,
    };
    let build_time =
        run(&["date", "-u", "+%Y-%m-%dT%H:%M:%SZ"]).unwrap_or_else(|| "unknown".into());

    println!("cargo:rustc-env=LLMCONDUIT_GIT_HASH={git_hash}");
    println!("cargo:rustc-env=LLMCONDUIT_GIT_DIRTY={dirty}");
    println!("cargo:rustc-env=LLMCONDUIT_BUILD_TIME={build_time}");
}

/// Run a command, returning its trimmed stdout on success, `None` otherwise.
fn run(args: &[&str]) -> Option<String> {
    let (cmd, rest) = args.split_first()?;
    let output = Command::new(cmd).args(rest).output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}
