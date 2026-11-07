// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Hands the helper the hyperlight-unikraft commit and the hyperlight-host
//! version it is built with, read from Cargo.lock, so the facts fiberd
//! records are the crates that wrote the snapshots, never strings kept by
//! hand.

use std::fs;
use std::path::Path;

fn main() {
    // Read at run time: env! would bake in the checkout that compiled this
    // script, which a shared CARGO_TARGET_DIR reuses for other checkouts.
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let lock = Path::new(&manifest_dir).join("Cargo.lock");
    // Absolute, so another checkout's build sees a changed instruction and
    // reruns this script. Cargo checks a relative path against the reusing
    // checkout's own Cargo.lock by mtime alone, and would keep this
    // checkout's facts when that lock is older.
    println!("cargo:rerun-if-changed={}", lock.display());
    let text = fs::read_to_string(&lock).expect("read Cargo.lock");
    let host = field(&text, "hyperlight-host", "version").expect("hyperlight-host in Cargo.lock");
    let source =
        field(&text, "hyperlight-unikraft", "source").expect("hyperlight-unikraft in Cargo.lock");
    // A git source ends in `#<commit>`.
    let commit = source
        .rsplit_once('#')
        .map(|(_, c)| c)
        .expect("hyperlight-unikraft from git");
    println!("cargo:rustc-env=HYPERLIGHT_HOST_VERSION={host}");
    println!("cargo:rustc-env=HYPERLIGHT_UNIKRAFT_COMMIT={commit}");
}

/// The `key` line of the `[[package]]` named `name`.
fn field(lock: &str, name: &str, key: &str) -> Option<String> {
    let mut in_pkg = false;
    for line in lock.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            in_pkg = false;
        } else if line == format!("name = \"{name}\"") {
            in_pkg = true;
        } else if in_pkg && let Some(v) = line.strip_prefix(&format!("{key} = ")) {
            return Some(v.trim_matches('"').to_string());
        }
    }
    None
}
