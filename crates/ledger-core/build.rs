//! Enables the repository's git hooks (`core.hooksPath = .githooks`) on the first local build,
//! so a fresh clone needs no manual setup. Skipped in CI and when a hooks path is already set.
//! This only touches the repository-local git config; the library itself has no I/O.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var_os("CI").is_some() {
        return;
    }
    let Some(manifest_dir) = std::env::var_os("CARGO_MANIFEST_DIR") else {
        return;
    };
    let root = Path::new(&manifest_dir).join("../..");
    if !root.join(".git").exists() || !root.join(".githooks").is_dir() {
        return;
    }
    let current = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["config", "--get", "core.hooksPath"])
        .output();
    let already_set = matches!(current, Ok(output) if !output.stdout.is_empty());
    if !already_set {
        // Best effort: a failure here must never break the build.
        let _ = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["config", "core.hooksPath", ".githooks"])
            .status();
    }
}
