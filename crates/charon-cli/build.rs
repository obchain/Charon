//! Build script — emits `CHARON_GIT_SHA` so the running binary can
//! report its commit on `charon_build_info{git_sha=…}` (closes #392).
//!
//! Strategy: shell out to `git rev-parse --short HEAD`. On any failure
//! (no `git` on PATH, no `.git` in the build context, detached / clean
//! tarball, network filesystem with stale index, …) emit the literal
//! string `"unknown"` so the caller's `option_env!("CHARON_GIT_SHA")`
//! still resolves to a usable value rather than panicking at compile
//! time.
//!
//! Re-runs on every change to `.git/HEAD` and `.git/refs/` so the
//! recorded SHA tracks the working tree, not the first build's HEAD.

use std::process::Command;

fn main() {
    let sha = git_short_sha().unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=CHARON_GIT_SHA={sha}");

    // Re-run when HEAD or any ref changes so a `git checkout` between
    // builds does not silently bake yesterday's SHA into a fresh
    // binary. The path is repository-root-relative because `cargo`
    // runs the build script from the crate dir; walking up to the
    // workspace root keeps multi-crate workspaces behaving the same
    // as a single-crate repo.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs");
    println!("cargo:rerun-if-env-changed=CHARON_GIT_SHA");
}

/// Run `git rev-parse --short HEAD` from the crate root and return
/// the trimmed stdout, or `None` if `git` is missing, errors, or
/// returns an empty string. Never panics; build.rs failures should
/// only ever surface as the `"unknown"` fallback.
fn git_short_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let trimmed = s.trim().to_string();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed)
}
