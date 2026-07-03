//! Build script: two jobs.
//!
//! 1. Embed git provenance (short commit, working-tree dirty flag, UTC build
//!    time) into the binary via `cargo:rustc-env`, so `--version` and the
//!    startup log can report exactly which source a running process was built
//!    from. All three vars are emitted unconditionally (falling back to
//!    `unknown`) so the `env!` reads in `src/lib.rs` always resolve, including
//!    builds from a tarball with no `.git` or no `git`/`date` on PATH.
//!
//! 2. Materialize `$OUT_DIR/dashboard_dist/` so `include_dir!("$OUT_DIR/
//!    dashboard_dist")` in `src/dashboard_ui.rs` ALWAYS compiles (D8). By
//!    default this is a one-file STUB (`index.html`) — a node-less Rust host
//!    builds with no frontend toolchain. When the operator opts in with
//!    `LLMCONDUIT_BUILD_DASHBOARD=1`, the directory is cleared and the real
//!    React+Vite SPA is built into it via `npm ci && npm run build`. Container
//!    builds can instead point `LLMCONDUIT_DASHBOARD_DIST` at a bundle produced
//!    by a separate Node stage. The runtime gate is the separate
//!    `--with-debug-ui` flag — both environment variables are build-time only.

use std::fs;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

/// Relative path (from the crate root) to the frontend project that produces
/// `dist/` when `LLMCONDUIT_BUILD_DASHBOARD=1`.
const FRONTEND_DIR: &str = "dashboard-frontend";

fn main() {
    emit_git_provenance();
    embed_dashboard();
}

/// Job 1: git short commit, dirty flag, and UTC build time → `cargo:rustc-env`.
fn emit_git_provenance() {
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

/// Job 2 (D8): guarantee `$OUT_DIR/dashboard_dist/` exists so `include_dir!`
/// compiles. With `LLMCONDUIT_BUILD_DASHBOARD=1` build the real SPA into it;
/// otherwise write a one-file stub. Either way the directory is REBUILT from
/// scratch so a prior enabled build's real assets never linger into a later
/// stub build (and vice-versa).
fn embed_dashboard() {
    // These are build-time switches; rebuild this script when either flips.
    println!("cargo:rerun-if-env-changed=LLMCONDUIT_BUILD_DASHBOARD");
    println!("cargo:rerun-if-env-changed=LLMCONDUIT_DASHBOARD_DIST");
    // Re-run when any build-affecting frontend input changes, so an enabled build
    // re-bundles. Covers sources, dependency manifests, and every build-time
    // config (Vite/Tailwind/PostCSS/TypeScript) — not the generated `dist/`,
    // `node_modules/`, or `*.tsbuildinfo` caches. (Harmless no-ops when disabled —
    // the stub does not read these.)
    for rel in [
        "src",
        "package.json",
        "package-lock.json",
        "vite.config.ts",
        "index.html",
        "tailwind.config.ts",
        "postcss.config.js",
        "tsconfig.json",
        "tsconfig.app.json",
        "tsconfig.node.json",
    ] {
        println!("cargo:rerun-if-changed={FRONTEND_DIR}/{rel}");
    }

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set by cargo"));
    let dist_dir = out_dir.join("dashboard_dist");

    // Always start from an empty directory: clearing first means neither a
    // stale stub nor stale real assets from a prior build survive a flag flip.
    reset_dir(&dist_dir);

    let build_enabled = std::env::var("LLMCONDUIT_BUILD_DASHBOARD")
        .map(|value| value == "1")
        .unwrap_or(false);

    let prebuilt_dist = std::env::var_os("LLMCONDUIT_DASHBOARD_DIST")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);

    assert!(
        !(build_enabled && prebuilt_dist.is_some()),
        "LLMCONDUIT_BUILD_DASHBOARD and LLMCONDUIT_DASHBOARD_DIST are mutually exclusive"
    );

    if let Some(prebuilt_dist) = prebuilt_dist {
        embed_prebuilt_dashboard(&prebuilt_dist, &dist_dir);
    } else if build_enabled {
        build_real_dashboard(&dist_dir);
    } else {
        write_stub_dashboard(&dist_dir);
    }
}

/// Copy a dashboard bundle produced by a separate build stage into `OUT_DIR`.
fn embed_prebuilt_dashboard(source: &Path, dist_dir: &Path) {
    assert!(
        source.is_dir(),
        "LLMCONDUIT_DASHBOARD_DIST directory {} does not exist",
        source.display()
    );
    assert!(
        source.join("index.html").is_file(),
        "LLMCONDUIT_DASHBOARD_DIST directory {} has no index.html",
        source.display()
    );

    println!("cargo:rerun-if-changed={}", source.display());
    copy_dir_contents(source, dist_dir);
}

fn copy_dir_contents(source: &Path, destination: &Path) {
    let entries = fs::read_dir(source)
        .unwrap_or_else(|err| panic!("failed to read {}: {err}", source.display()));

    for entry in entries {
        let entry = entry
            .unwrap_or_else(|err| panic!("failed to read entry in {}: {err}", source.display()));
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry
            .file_type()
            .unwrap_or_else(|err| panic!("failed to inspect {}: {err}", source_path.display()));

        if file_type.is_dir() {
            fs::create_dir_all(&destination_path).unwrap_or_else(|err| {
                panic!("failed to create {}: {err}", destination_path.display())
            });
            copy_dir_contents(&source_path, &destination_path);
        } else if file_type.is_file() {
            fs::copy(&source_path, &destination_path).unwrap_or_else(|err| {
                panic!(
                    "failed to copy {} to {}: {err}",
                    source_path.display(),
                    destination_path.display()
                )
            });
        } else {
            panic!(
                "dashboard bundle contains unsupported entry {}",
                source_path.display()
            );
        }
    }
}

/// Remove `dir` (if present) and recreate it empty.
fn reset_dir(dir: &Path) {
    if dir.exists() {
        fs::remove_dir_all(dir)
            .unwrap_or_else(|err| panic!("failed to clear {}: {err}", dir.display()));
    }
    fs::create_dir_all(dir)
        .unwrap_or_else(|err| panic!("failed to create {}: {err}", dir.display()));
}

/// Write the node-less stub: a single `index.html` explaining how to get the
/// real SPA, plus a tiny known asset so the asset route has a deterministic
/// positive case in tests. `include_dir!` embeds whatever is here.
fn write_stub_dashboard(dist_dir: &Path) {
    let index = "<!DOCTYPE html>\n\
        <html lang=\"en\">\n\
        <head><meta charset=\"utf-8\"><title>llmconduit dashboard (stub)</title></head>\n\
        <body><main>\n\
        <h1>llmconduit dashboard</h1>\n\
        <p>Dashboard not built; rebuild with <code>LLMCONDUIT_BUILD_DASHBOARD=1</code> \
        (requires Node/npm) to embed the React SPA.</p>\n\
        </main></body>\n\
        </html>\n";
    fs::write(dist_dir.join("index.html"), index)
        .unwrap_or_else(|err| panic!("failed to write stub index.html: {err}"));

    let assets_dir = dist_dir.join("assets");
    fs::create_dir_all(&assets_dir)
        .unwrap_or_else(|err| panic!("failed to create stub assets dir: {err}"));
    // A stable asset name (real Vite assets are content-hashed and unknowable
    // here) so a test can assert the `/dashboard/assets/{*path}` route serves a
    // present asset by a known path.
    fs::write(
        assets_dir.join("stub.txt"),
        "dashboard stub asset; rebuild with LLMCONDUIT_BUILD_DASHBOARD=1\n",
    )
    .unwrap_or_else(|err| panic!("failed to write stub asset: {err}"));
}

/// Build the real SPA into `dist_dir` via `npm ci && npm run build`, pointing
/// Vite's `outDir` at it (keeping the `base: '/dashboard/'` from
/// `vite.config.ts`). Any failure — missing npm, failed install, failed build —
/// aborts the Cargo build with a clear message.
fn build_real_dashboard(dist_dir: &Path) {
    let frontend = PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo"),
    )
    .join(FRONTEND_DIR);
    assert!(
        frontend.is_dir(),
        "LLMCONDUIT_BUILD_DASHBOARD=1 but the frontend dir {} is missing; \
         cannot build the dashboard SPA",
        frontend.display()
    );

    // `npm ci` for a clean, lockfile-pinned install (fails if package.json and
    // the lockfile are out of sync, which is what we want in a build).
    run_in(
        &frontend,
        &["npm", "ci"],
        "npm ci (frontend dependency install)",
    );

    // `npm run build` == `tsc -b && vite build`. Pass Vite the OUT_DIR target and
    // `--emptyOutDir` (it sits outside the project root) AFTER `--` so the flags
    // reach `vite build`, not npm. `base: '/dashboard/'` stays as configured.
    let out_dir_arg = dist_dir.display().to_string();
    run_in(
        &frontend,
        &[
            "npm",
            "run",
            "build",
            "--",
            "--outDir",
            &out_dir_arg,
            "--emptyOutDir",
        ],
        "npm run build (Vite SPA bundle)",
    );

    // Sanity: the build must have produced an index.html in the OUT_DIR target.
    let index = dist_dir.join("index.html");
    assert!(
        index.is_file(),
        "LLMCONDUIT_BUILD_DASHBOARD=1: npm run build completed but {} was not \
         produced; check the frontend build output",
        index.display()
    );
}

/// Run a command in `dir`, panicking (aborting the Cargo build) with a clear,
/// `what`-labelled message if the binary is missing or the command fails.
fn run_in(dir: &Path, args: &[&str], what: &str) {
    let (cmd, rest) = args.split_first().expect("non-empty command");
    let status = Command::new(cmd).args(rest).current_dir(dir).status();
    match status {
        Ok(status) if status.success() => {}
        Ok(status) => panic!(
            "LLMCONDUIT_BUILD_DASHBOARD=1: {what} failed with {status} \
             (command: `{}` in {})",
            args.join(" "),
            dir.display()
        ),
        Err(err) => panic!(
            "LLMCONDUIT_BUILD_DASHBOARD=1: could not run {what}: {err} \
             (is `{cmd}` installed and on PATH? command: `{}` in {})",
            args.join(" "),
            dir.display()
        ),
    }
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
