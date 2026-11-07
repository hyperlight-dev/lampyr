// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Upgrades and the downgrade guard order adapters by `templateVersion`, so a
//! template whose adapter content changes must also get a new version.

use std::collections::BTreeMap;

use lampyr_cli::project::{self, AdapterMetadata};
use sha2::{Digest, Sha256};

const RUNTIMES: [&str; 5] = ["nodejs", "python", "go", "rust", "dotnet"];

fn rendered(runtime: &str) -> (String, String) {
    let root = tempfile::tempdir().unwrap();
    project::initialize(root.path(), Some("example"), runtime).unwrap();
    let metadata: AdapterMetadata = serde_json::from_slice(
        &std::fs::read(root.path().join(".lampyr-adapter/adapter.json")).unwrap(),
    )
    .unwrap();
    let content = serde_json::json!({
        "files": metadata.files,
        "entrypoint": metadata.entrypoint,
        "snapshotWarmup": metadata.snapshot_warmup,
    });
    let digest: String = Sha256::digest(serde_json::to_vec(&content).unwrap())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    (metadata.template_version, format!("sha256:{digest}"))
}

#[test]
fn adapter_content_changes_require_a_template_version_bump() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/adapter-template-versions.json"
    );
    let recorded: BTreeMap<String, BTreeMap<String, String>> =
        serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    for runtime in RUNTIMES {
        let (version, digest) = rendered(runtime);
        let versions = recorded.get(runtime).cloned().unwrap_or_default();
        match versions.get(&version) {
            Some(expected) => assert_eq!(
                *expected, digest,
                "{runtime} adapter content changed without a templateVersion bump; bump \
                 templates/services/{runtime}/manifest.json and record \"<new version>\": \"{digest}\" \
                 in tests/fixtures/adapter-template-versions.json"
            ),
            None => panic!(
                "{runtime} templateVersion {version} is not recorded; add \"{version}\": \"{digest}\" \
                 to tests/fixtures/adapter-template-versions.json (never change an existing entry)"
            ),
        }
    }
}
