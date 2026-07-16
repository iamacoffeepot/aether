//! Bake build provenance into the `bloomery` binary so its `--describe`
//! manifest (ADR-0115) can report the source revision, build profile, and
//! target triple without a runtime git / cargo probe. The bin reads these back
//! via `env!`. Mirrors `aether-substrate-bundle`'s provenance block.
//!
//! - `AETHER_GIT_SHA` — `git rev-parse --short HEAD`, or `"unknown"` when the
//!   binary is built outside a git checkout (a published crate, a tarball). The
//!   `rerun-if-changed` on `.git/HEAD` re-runs the script when the checkout
//!   moves to a new commit.
//! - `AETHER_BUILD_PROFILE` — cargo's `PROFILE` (`debug` / `release`).
//! - `AETHER_TARGET_TRIPLE` — cargo's `TARGET` (e.g. `aarch64-apple-darwin`).

// Build script: PROFILE / TARGET are cargo-provided build-time env vars, the
// only channel cargo uses to pass them — no config layer exists at build time.
#![allow(clippy::disallowed_methods)]

use std::env;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    let git_sha = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .map_or_else(|| "unknown".to_owned(), |s| s.trim().to_owned());
    let git_sha = if git_sha.is_empty() {
        "unknown".to_owned()
    } else {
        git_sha
    };
    println!("cargo:rustc-env=AETHER_GIT_SHA={git_sha}");
    let profile = env::var("PROFILE").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=AETHER_BUILD_PROFILE={profile}");
    let target = env::var("TARGET").unwrap_or_else(|_| "unknown".to_owned());
    println!("cargo:rustc-env=AETHER_TARGET_TRIPLE={target}");
}
