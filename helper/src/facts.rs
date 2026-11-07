// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! The four facts a snapshot made here depends on, one token each, in the
//! order fiberd reads them from `--version` and READY: this helper (its
//! park layout, with the hyperlight-unikraft commit that writes the
//! snapshots), the hyperlight_host crate, the hypervisor in use and the
//! CPU vendor. fiberd records them as the platform of every park, so a
//! park cannot be resumed by a helper that would fail to restore it.

use std::path::Path;

pub const HYPERLIGHT_UNIKRAFT_COMMIT: &str = env!("HYPERLIGHT_UNIKRAFT_COMMIT");

/// The capability line `--version` prints after the facts: this helper
/// restores the template fiberd unpacks, so fiberd passes it
/// `--template-dir`. fiberd passes that flag only to a helper that says so.
pub const TEMPLATE_DIR: &str = "template-dir";

/// What `--version` prints: the facts, then each capability on a line of
/// its own. fiberd reads the facts from the first line only.
pub fn version() -> String {
    format!("{}\n{TEMPLATE_DIR}", line())
}

pub fn line() -> String {
    format!(
        "lampyr-hluk-helper/{}+hluk-{} hyperlight_host/{} {} {}",
        env!("CARGO_PKG_VERSION"),
        &HYPERLIGHT_UNIKRAFT_COMMIT[..12.min(HYPERLIGHT_UNIKRAFT_COMMIT.len())],
        env!("HYPERLIGHT_HOST_VERSION"),
        hypervisor(),
        cpu_vendor()
    )
}

/// The hypervisor hyperlight-unikraft uses here. It is built for KVM alone
/// (its `mshv` feature is off), so that is the only one this helper can
/// run on.
fn hypervisor() -> &'static str {
    if Path::new("/dev/kvm").exists() {
        "kvm"
    } else {
        "none"
    }
}

/// The CPU vendor as `ly build` records it in a template's metadata:
/// CPUID leaf 0 ("GenuineIntel", "AuthenticAMD").
pub fn cpu_vendor() -> String {
    #[cfg(target_arch = "x86_64")]
    {
        #[allow(unused_unsafe)] // __cpuid requires unsafe on Rust 1.91.
        let r = unsafe { core::arch::x86_64::__cpuid(0) };
        let mut bytes = [0u8; 12];
        bytes[0..4].copy_from_slice(&r.ebx.to_le_bytes());
        bytes[4..8].copy_from_slice(&r.edx.to_le_bytes());
        bytes[8..12].copy_from_slice(&r.ecx.to_le_bytes());
        let s = String::from_utf8_lossy(&bytes).replace(char::is_whitespace, "_");
        if s.is_empty() {
            "unknown".to_string()
        } else {
            s
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        "unknown".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn facts_are_four_tokens_fiberd_can_parse() {
        let line = line();
        let f: Vec<&str> = line.split(' ').collect();
        assert_eq!(f.len(), 4, "{line:?}");
        assert!(f.iter().all(|t| !t.is_empty()), "{line:?}");
        assert!(f[0].starts_with("lampyr-hluk-helper/"), "{line:?}");
        assert_eq!(
            f[1],
            concat!("hyperlight_host/", env!("HYPERLIGHT_HOST_VERSION"))
        );
        assert!(matches!(f[2], "kvm" | "none"), "{line:?}");
    }

    /// `ly build` records the hyperlight-unikraft commit it snapshots with,
    /// and the helper refuses any other, so the two must move together.
    #[test]
    fn the_helper_restores_what_ly_build_snapshots() {
        let assets =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../cli/src/assets.rs"))
                .expect("read cli/src/assets.rs");
        let line = assets
            .lines()
            .find(|l| l.contains("const RELEASE_COMMIT"))
            .expect("RELEASE_COMMIT in cli/src/assets.rs");
        let commit = line.split('"').nth(1).expect("a quoted commit");
        assert_eq!(
            commit, HYPERLIGHT_UNIKRAFT_COMMIT,
            "bump helper/Cargo.toml's hyperlight-unikraft tag with cli/src/assets.rs"
        );
    }
}
