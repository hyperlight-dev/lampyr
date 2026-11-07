// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::path::{Path, PathBuf};
use std::process::Command;

fn git(root: &Path, arguments: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(arguments)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn repository(root: &Path) -> Option<PathBuf> {
    let top = PathBuf::from(git(root, &["rev-parse", "--show-toplevel"])?);
    (top.canonicalize().ok()? == root.canonicalize().ok()?).then_some(top)
}

fn git_path(root: &Path, path: &str) -> Option<PathBuf> {
    let path = PathBuf::from(git(root, &["rev-parse", "--git-path", path])?);
    Some(if path.is_absolute() {
        path
    } else {
        root.join(path)
    })
}

fn watch_repository(root: &Path) {
    let out_dir = std::env::var_os("OUT_DIR").map(PathBuf::from);
    if let Some(paths) = git(root, &["ls-files"]) {
        for path in paths.lines().filter(|path| !path.is_empty()) {
            let path = root.join(path);
            if path.exists() {
                println!("cargo:rerun-if-changed={}", path.display());
            } else if let Some(parent) = path.ancestors().skip(1).find(|ancestor| ancestor.exists())
                && out_dir
                    .as_ref()
                    .is_none_or(|out_dir| !out_dir.starts_with(parent))
            {
                println!("cargo:rerun-if-changed={}", parent.display());
            }
            // A missing file already made the build dirty when its previous
            // file watch fired. Its parent catches restoration when that
            // directory does not also contain Cargo's build outputs.
        }
    }
    for path in ["HEAD", "index", "packed-refs"] {
        if let Some(path) = git_path(root, path) {
            watch_existing(path);
        }
    }
    if let Some(reference) = git(root, &["symbolic-ref", "-q", "HEAD"])
        && let Some(path) = git_path(root, &reference)
    {
        watch_existing(path);
    }
}

fn watch_existing(mut path: PathBuf) {
    while !path.exists() {
        if !path.pop() {
            return;
        }
    }
    println!("cargo:rerun-if-changed={}", path.display());
}

fn main() {
    let manifest = std::env::var_os("CARGO_MANIFEST_DIR").unwrap();
    let root = Path::new(&manifest).parent().unwrap();
    let (commit, state) = repository(root).map_or_else(
        || ("unknown".to_owned(), "unknown"),
        |root| {
            watch_repository(&root);
            let commit = git(&root, &["rev-parse", "--short=12", "HEAD"])
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "unknown".to_owned());
            let state = git(&root, &["status", "--porcelain", "--untracked-files=no"]).map_or(
                "unknown",
                |status| {
                    if status.is_empty() { "clean" } else { "dirty" }
                },
            );
            (commit, state)
        },
    );
    println!("cargo:rustc-env=LAMPYR_BUILD_COMMIT={commit}");
    println!("cargo:rustc-env=LAMPYR_BUILD_STATE={state}");
}
