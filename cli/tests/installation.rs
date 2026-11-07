// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use serde_json::json;

#[test]
fn configuration_is_opaque_and_accepts_third_party_provider_settings() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("provider.yaml");
    std::fs::write(&path, "region: test\nmachine:\n  flavor: arbitrary\n").unwrap();
    assert_eq!(
        lampyr_cli::cluster::read_config(Some(&path)).unwrap(),
        json!({"region":"test","machine":{"flavor":"arbitrary"}})
    );
    std::fs::write(&path, "- not\n- a mapping\n").unwrap();
    assert!(lampyr_cli::cluster::read_config(Some(&path)).is_err());
}
