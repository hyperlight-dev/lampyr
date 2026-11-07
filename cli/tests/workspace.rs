// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;

use base64::{Engine, engine::general_purpose::STANDARD};
use lampyr_cli::workspace::Directory;
use serde_json::json;

#[test]
fn scaffolding_refuses_collisions_before_creating_any_files() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Directory::open(root.path()).unwrap();
    std::fs::write(root.path().join("parameters.json"), "customer edits\n").unwrap();
    let files = BTreeMap::from([
        (
            "main.bicep".to_owned(),
            "output answer int = 42\n".to_owned(),
        ),
        ("parameters.json".to_owned(), "replacement\n".to_owned()),
    ]);
    assert!(workspace.scaffold(&files).is_err());
    assert!(!root.path().join("main.bicep").exists());
    assert_eq!(
        std::fs::read_to_string(root.path().join("parameters.json")).unwrap(),
        "customer edits\n"
    );
    let files = BTreeMap::from([(
        "modules/main.bicep".to_owned(),
        "param name string\n".to_owned(),
    )]);
    workspace.scaffold(&files).unwrap();
    assert!(workspace.scaffold(&files).is_err());
}

#[test]
fn workspace_reads_are_bounded_and_preserve_unicode() {
    let root = tempfile::tempdir().unwrap();
    let workspace = Directory::open(root.path()).unwrap();
    let contents = "a".repeat(8191) + "💡";
    std::fs::write(root.path().join("main.bicep"), &contents).unwrap();
    let first = workspace.read("main.bicep", 0).unwrap();
    assert!(!first.eof);
    let second = workspace.read("main.bicep", first.next).unwrap();
    assert!(second.eof);
    let bytes = [
        STANDARD.decode(first.data).unwrap(),
        STANDARD.decode(second.data).unwrap(),
    ]
    .concat();
    assert_eq!(bytes, contents.as_bytes());
    for path in [
        "../outside",
        "/etc/passwd",
        "modules/../../outside",
        ".git/config",
        "modules\\outside",
    ] {
        assert!(workspace.read(path, 0).is_err(), "{path}");
    }
}

#[cfg(unix)]
#[test]
fn workspace_symlinks_cannot_expose_files_outside_the_grant() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("private"), "not granted").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).unwrap();
    let workspace = Directory::open(root.path()).unwrap();
    assert!(workspace.read("escape/private", 0).is_err());
    let files = BTreeMap::from([("escape/new".to_owned(), "denied".to_owned())]);
    assert!(workspace.scaffold(&files).is_err());
    assert!(!outside.path().join("new").exists());
}

#[test]
fn manifest_inventories_survive_reopening_and_reject_oversized_updates() {
    let root = tempfile::tempdir().unwrap();
    let state = Directory::private(root.path()).unwrap();
    let manifest = json!({"data":{"script":"x".repeat(32 * 1024)}});
    state.write_state("runtime", &manifest).unwrap();
    assert!(
        state
            .write_state("runtime", &json!({"data":"x".repeat(512 * 1024)}))
            .is_err()
    );
    let reopened = Directory::private(root.path()).unwrap();
    assert_eq!(reopened.read_state("runtime").unwrap(), Some(manifest));
}

#[test]
fn private_state_survives_reopening_without_touching_customer_source() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("state");
    let state = Directory::private(&root).unwrap();
    assert!(state.read_state("operation").unwrap().is_none());
    state
        .write_state("operation", &json!({"id": "/deployments/one"}))
        .unwrap();
    state
        .write_state("operation", &json!({"id": "/deployments/two"}))
        .unwrap();
    let state = Directory::private(&root).unwrap();
    assert_eq!(
        state.read_state("operation").unwrap(),
        Some(json!({"id": "/deployments/two"}))
    );
    assert!(state.write_state("../outside", &json!({})).is_err());
    std::fs::write(root.join("broken.json"), "{not json").unwrap();
    assert!(state.read_state("broken").is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let _reprotected = Directory::private(&root).unwrap();
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(root.join("operation.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
