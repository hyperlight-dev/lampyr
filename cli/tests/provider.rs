// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use lampyr_cli::artifact::{Artifact, Capability, FileDigest, Manifest, Operation};
use lampyr_cli::provider::{Authorization, Provider, Request};
use lampyr_cli::runtime::Cancellation;
use lampyr_cli::workspace::Directory;
use serde_json::json;
use sha2::{Digest, Sha256};

fn artifact(program: &str) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let rootfs_path = std::path::PathBuf::from(
        std::env::var_os("LAMPYR_TEST_ROOTFS")
            .expect("set LAMPYR_TEST_ROOTFS to the provider Python CPIO"),
    );
    let rootfs = std::fs::read(&rootfs_path).unwrap();
    let file = |path: &str, bytes: &[u8]| FileDigest {
        path: path.to_owned(),
        sha256: Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    };
    let manifest = Manifest {
        protocol_version: 1,
        id: "test".to_owned(),
        version: "1".to_owned(),
        runtime_version: lampyr_cli::assets::VERSION.to_owned(),
        memory_mib: 256,
        rootfs: file("rootfs.cpio", &rootfs),
        kernel: None,
        cold_compatibility: None,
        program: file("provider.py", program.as_bytes()),
        snapshot_index: None,
        snapshot_repository: None,
        environment: BTreeMap::new(),
        network_hosts: Vec::new(),
        source_kinds: BTreeSet::new(),
        operations: BTreeMap::from([
            (Operation::Describe, BTreeSet::new()),
            (
                Operation::Init,
                BTreeSet::from([Capability::WorkspaceScaffold, Capability::State]),
            ),
            (
                Operation::Apply,
                BTreeSet::from([Capability::WorkspaceRead]),
            ),
            (Operation::Inspect, BTreeSet::new()),
        ]),
    };
    std::fs::write(root.path().join("rootfs.cpio"), rootfs).unwrap();
    std::fs::write(root.path().join("provider.py"), program).unwrap();
    std::fs::write(
        root.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    root
}

#[test]
fn external_provider_ids_and_configuration_execute_without_cli_registration() {
    let artifact = artifact(
        r#"
import json
from pathlib import Path
import hyperlight
def invoke(request):
    if request["operation"] == "describe":
        result = json.loads(Path("/provider/manifest.json").read_text())
    else:
        denied = []
        for name in ("host.invoke", "tool.run"):
            try:
                hyperlight.call(name, {})
            except Exception:
                denied.append(name)
        result = {"vendor":request["config"].get("vendorSpecific"),"denied":denied}
    return {"protocolVersion":1,"status":"ok","result":result}
"#,
    );
    let workspace = tempfile::tempdir().unwrap();
    let mut provider = Provider::boot(
        Artifact::load(artifact.path(), "test").unwrap(),
        Operation::Apply,
        Authorization {
            workspace: Some(Arc::new(Directory::open(workspace.path()).unwrap())),
            ..Default::default()
        },
        Duration::from_secs(20),
        Cancellation::default(),
    )
    .unwrap();
    let reply = provider
        .invoke(
            &Request {
                protocol_version: 1,
                operation: Operation::Apply,
                installation_id: "external".into(),
                config: json!({"vendorSpecific":{"arbitrary":"provider-owned"}}),
            },
            Duration::from_secs(10),
        )
        .unwrap()
        .value;
    assert_eq!(reply["vendor"]["arbitrary"], "provider-owned");
    assert_eq!(reply["denied"], json!(["host.invoke", "tool.run"]));
}

#[test]
fn provider_scaffolds_once_and_uses_edited_source_without_overwriting_it() {
    exercise_provider_scaffolding(false);
}

#[test]
fn native_snapshot_restores_provider_mounts_handshake_and_authorized_callbacks() {
    exercise_provider_scaffolding(true);
}

fn exercise_provider_scaffolding(snapshot: bool) {
    let artifact = artifact(include_str!("fixtures/provider.py"));
    if snapshot {
        let layout = artifact.path().join("snapshot");
        let mut guest =
            hyperlight_unikraft::SandboxBuilder::from_initrd(artifact.path().join("rootfs.cpio"))
                .scratch_mb(256)
                .boot()
                .unwrap();
        guest.snapshot_to(&layout).unwrap();
        drop(guest);
        let mut manifest: Manifest =
            serde_json::from_slice(&std::fs::read(artifact.path().join("manifest.json")).unwrap())
                .unwrap();
        let bytes = std::fs::read(layout.join("index.json")).unwrap();
        manifest.snapshot_index = Some(FileDigest {
            path: "snapshot/index.json".into(),
            sha256: Sha256::digest(&bytes)
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect(),
        });
        std::fs::write(
            artifact.path().join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }
    let workspace = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let authorization = || Authorization {
        workspace: Some(Arc::new(Directory::open(workspace.path()).unwrap())),
        scaffold: true,
        state: Some(Arc::new(Directory::private(state.path()).unwrap())),
        ..Authorization::default()
    };
    let loaded = Artifact::load(artifact.path(), "test").unwrap();
    assert_eq!(loaded.snapshot_digest.is_some(), snapshot);
    let mut provider = Provider::boot(
        loaded,
        Operation::Init,
        authorization(),
        Duration::from_secs(20),
        Cancellation::default(),
    )
    .unwrap();
    let mut request = Request {
        protocol_version: 1,
        operation: Operation::Init,
        installation_id: "one".to_owned(),
        config: json!({}),
    };
    provider.invoke(&request, Duration::from_secs(10)).unwrap();
    assert!(provider.invoke(&request, Duration::from_secs(10)).is_err());
    let edited = "output customer string = 'my source'\n";
    std::fs::write(workspace.path().join("main.bicep"), edited).unwrap();
    drop(provider);
    let mut provider = Provider::boot(
        Artifact::load(artifact.path(), "test").unwrap(),
        Operation::Apply,
        authorization(),
        Duration::from_secs(20),
        Cancellation::default(),
    )
    .unwrap();
    assert!(provider.invoke(&request, Duration::from_secs(10)).is_err());
    request.operation = Operation::Apply;
    assert_eq!(
        provider
            .invoke(&request, Duration::from_secs(10))
            .unwrap()
            .value["source"],
        edited
    );
    request.config = json!({"fail": true});
    let error = provider
        .invoke(&request, Duration::from_secs(10))
        .err()
        .unwrap();
    assert!(error.to_string().contains("deployment-failed"));
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("main.bicep")).unwrap(),
        edited
    );
}

#[test]
fn operations_cannot_use_capabilities_granted_to_another_operation() {
    let artifact = artifact(include_str!("fixtures/provider.py"));
    let state = tempfile::tempdir().unwrap();
    let mut provider = Provider::boot(
        Artifact::load(artifact.path(), "test").unwrap(),
        Operation::Inspect,
        Authorization {
            state: Some(Arc::new(Directory::private(state.path()).unwrap())),
            ..Authorization::default()
        },
        Duration::from_secs(20),
        Cancellation::default(),
    )
    .unwrap();
    let error = provider
        .invoke(
            &Request {
                protocol_version: 1,
                operation: Operation::Inspect,
                installation_id: "one".to_owned(),
                config: json!({}),
            },
            Duration::from_secs(10),
        )
        .err()
        .unwrap();
    assert!(format!("{error:#}").contains("state.read"));
}
