// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! The helper's `--version`, which needs no hypervisor and no template.

use std::process::Command;

/// The facts on the first line, then `template-dir` alone, for every
/// spelling of the flag.
#[test]
fn version_prints_four_facts_then_the_template_dir_capability() {
    for flag in ["--version", "-version"] {
        let out = Command::new(env!("CARGO_BIN_EXE_lampyr-hluk-helper"))
            .arg(flag)
            .output()
            .unwrap();
        assert!(out.status.success(), "{flag}");
        let version = String::from_utf8(out.stdout).unwrap();
        let lines: Vec<&str> = version.split_inclusive('\n').collect();
        assert_eq!(lines.len(), 2, "{flag}: {version:?}");
        let facts: Vec<&str> = lines[0].trim_end_matches('\n').split(' ').collect();
        assert_eq!(facts.len(), 4, "{flag}: {version:?}");
        assert!(facts.iter().all(|f| !f.is_empty()), "{flag}: {version:?}");
        assert!(
            facts[0].starts_with(concat!(
                "lampyr-hluk-helper/",
                env!("CARGO_PKG_VERSION"),
                "+hluk-"
            )),
            "{flag}: {version:?}"
        );
        assert_eq!(lines[1], "template-dir\n", "{flag}: {version:?}");
    }
}
