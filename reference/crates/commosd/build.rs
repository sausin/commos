//! Embed a build-time version string so `commosd --version`, `/metrics`, and `/dashboard` report
//! the release the binary was actually cut from — not the static `0.1.0` workspace manifest
//! version. Resolution order:
//!
//!   1. `COMMOS_VERSION` env at build time — CI passes the release tag (see the release workflow);
//!      also lets a distro packager pin an exact string.
//!   2. `git describe --tags --always --dirty` — dev and from-source builds get the tag (plus any
//!      commits-since / dirty suffix), so a locally-built binary is traceable to a commit.
//!   3. The Cargo manifest version — the fallback when neither is available (e.g. a published
//!      crate tarball with no `.git`).
//!
//! The resolved value is exposed to the crate as `env!("COMMOS_VERSION")`.

use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Re-run when the override changes or the checked-out commit/tag moves, so the embedded
    // version stays in sync without a manual clean. Ask git for the real `.git` location rather
    // than guessing a relative depth (which breaks under worktrees / different layouts).
    println!("cargo:rerun-if-env-changed=COMMOS_VERSION");
    if let Some(git_dir) = git_common_dir() {
        for f in ["HEAD", "packed-refs"] {
            let p = git_dir.join(f);
            if p.exists() {
                println!("cargo:rerun-if-changed={}", p.display());
            }
        }
    }

    println!("cargo:rustc-env=COMMOS_VERSION={}", resolve_version());
}

fn resolve_version() -> String {
    // 1. Explicit override (CI / packagers).
    if let Ok(v) = std::env::var("COMMOS_VERSION") {
        let v = v.trim();
        if !v.is_empty() {
            return strip_v(v);
        }
    }
    // 2. Git description of the working tree.
    if let Some(v) = git_describe() {
        return strip_v(&v);
    }
    // 3. Manifest version (always set by Cargo for a build script).
    std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "unknown".to_string())
}

/// `git describe --tags --always --dirty` from the crate dir, or `None` when git is unavailable
/// or this is not a checkout (a source tarball).
fn git_describe() -> Option<String> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git")
        .args(["describe", "--tags", "--always", "--dirty=-dev"])
        .current_dir(manifest_dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// Absolute path to the repository's common `.git` directory, resolved via git so it is correct
/// regardless of workspace depth or git worktrees. `None` when this is not a checkout.
fn git_common_dir() -> Option<PathBuf> {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").ok()?;
    let out = Command::new("git")
        .args(["rev-parse", "--absolute-git-dir"])
        .current_dir(&manifest_dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| PathBuf::from(s))
}

/// Drop a leading `v` from a release tag (`v1.2.3` → `1.2.3`) so the reported version matches the
/// manifest style; leaves a commit-hash-only describe (`g1a2b3c`) untouched.
fn strip_v(v: &str) -> String {
    match v.strip_prefix('v') {
        Some(rest) if rest.starts_with(|c: char| c.is_ascii_digit()) => rest.to_string(),
        _ => v.to_string(),
    }
}
