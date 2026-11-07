// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::{BTreeMap, BTreeSet};

use lampyr_cli::artifact::{
    Artifact, Capability, ColdCompatibility, FileDigest, HostCompatibility, Manifest,
    OciDescriptor, Operation, SnapshotIndex, SnapshotManifest, SourceKind,
};
use lampyr_cli::provider::{Authorization, Provider};
use lampyr_cli::runtime::Cancellation;
use serde_json::json;
use sha2::{Digest, Sha256};

fn digest(path: &str, content: &[u8]) -> FileDigest {
    FileDigest {
        path: path.to_owned(),
        sha256: Sha256::digest(content)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    }
}

fn artifact() -> (tempfile::TempDir, Manifest) {
    let root = tempfile::tempdir().unwrap();
    let manifest = Manifest {
        protocol_version: 1,
        id: "test".to_owned(),
        version: "1".to_owned(),
        runtime_version: lampyr_cli::assets::VERSION.to_owned(),
        memory_mib: 256,
        rootfs: digest("rootfs.cpio", b"test rootfs"),
        kernel: None,
        cold_compatibility: None,
        program: digest("provider.py", b"# external provider\n"),
        snapshot_index: None,
        snapshot_repository: None,
        environment: BTreeMap::new(),
        network_hosts: Vec::new(),
        source_kinds: BTreeSet::new(),
        operations: BTreeMap::from([
            (Operation::Describe, BTreeSet::new()),
            (Operation::Apply, BTreeSet::from([Capability::State])),
        ]),
    };
    std::fs::write(root.path().join("rootfs.cpio"), b"test rootfs").unwrap();
    std::fs::write(root.path().join("provider.py"), b"# external provider\n").unwrap();
    save(&root, &manifest);
    (root, manifest)
}

fn save(root: &tempfile::TempDir, manifest: &Manifest) {
    std::fs::write(
        root.path().join("manifest.json"),
        serde_json::to_vec(manifest).unwrap(),
    )
    .unwrap();
}

fn host() -> HostCompatibility {
    HostCompatibility {
        architecture: "x86_64".into(),
        hypervisor: Some("kvm".into()),
        cpu_vendor: Some("GenuineIntel".into()),
    }
}

#[test]
fn python_assembled_snapshot_index_is_consumable_with_cold_boot_fallback() {
    let (root, mut manifest) = artifact();
    let native = variant(&root, &host());
    save_index(&root, &mut manifest, vec![native]);
    let destination = tempfile::tempdir().unwrap();
    let output = destination.path().join("assembled");
    let status = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../providers/snapshot_oci.py"
        ))
        .arg("assemble")
        .arg("--artifact")
        .arg(root.path())
        .arg("--variant")
        .arg(root.path())
        .arg("--output")
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());
    let loaded = Artifact::load_for_host_with_registry(&output, "test", &host(), None).unwrap();
    let snapshot = loaded.snapshot.as_ref().unwrap();
    assert!(snapshot.join("index.json").is_file());
    let mut incompatible = host();
    incompatible.cpu_vendor = Some("AuthenticAMD".into());
    let cold = Artifact::load_for_host_with_registry(&output, "test", &incompatible, None).unwrap();
    assert!(cold.snapshot.is_none());
    assert_eq!(
        std::fs::read(cold.verified_boot_files().unwrap().rootfs).unwrap(),
        b"test rootfs"
    );
}

fn blob(root: &tempfile::TempDir, media_type: &str, content: &[u8]) -> OciDescriptor {
    let hash = digest("", content).sha256;
    let directory = root.path().join("blobs/sha256");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join(&hash), content).unwrap();
    OciDescriptor {
        media_type: media_type.into(),
        digest: format!("sha256:{hash}"),
        size: content.len() as u64,
        annotations: BTreeMap::new(),
    }
}

fn variant(root: &tempfile::TempDir, compatibility: &HostCompatibility) -> OciDescriptor {
    let config = serde_json::to_vec(&json!({
        "arch": compatibility.architecture,
        "hypervisor": compatibility.hypervisor,
        "cpu_vendor": compatibility.cpu_vendor,
    }))
    .unwrap();
    let manifest = SnapshotManifest {
        schema_version: 2,
        media_type: "application/vnd.oci.image.manifest.v1+json".into(),
        artifact_type: "application/vnd.hyperlight.snapshot.config.v5+json".into(),
        config: blob(
            root,
            "application/vnd.hyperlight.snapshot.config.v5+json",
            &config,
        ),
        layers: vec![
            blob(
                root,
                "application/vnd.hyperlight.snapshot.memory.v2",
                compatibility.cpu_vendor.as_ref().unwrap().as_bytes(),
            ),
            // A second memory layer, as a snapshot taken on top of another has:
            // hyperlight-host 0.19 writes the delta as its own layer.
            blob(
                root,
                "application/vnd.hyperlight.snapshot.memory.v2",
                b"second memory layer",
            ),
            blob(
                root,
                "application/vnd.hyperlight.snapshot.transport.v1",
                b"transport",
            ),
        ],
    };
    let mut descriptor = blob(
        root,
        &manifest.media_type,
        &serde_json::to_vec(&manifest).unwrap(),
    );
    descriptor.annotations = BTreeMap::from([
        (
            "dev.hyperlight.snapshot.arch".into(),
            compatibility.architecture.clone(),
        ),
        (
            "org.opencontainers.image.ref.name".into(),
            format!(
                "{}-{}",
                lampyr_cli::assets::VERSION,
                hyperlight_unikraft::SNAPSHOT_KEY
            ),
        ),
        (
            "dev.hyperlight.snapshot.hypervisor".into(),
            compatibility.hypervisor.clone().unwrap(),
        ),
        (
            "dev.hyperlight.snapshot.cpu.vendor".into(),
            compatibility.cpu_vendor.clone().unwrap(),
        ),
    ]);
    descriptor
}

fn save_index(root: &tempfile::TempDir, manifest: &mut Manifest, variants: Vec<OciDescriptor>) {
    let index = SnapshotIndex {
        schema_version: 2,
        media_type: "application/vnd.oci.image.index.v1+json".into(),
        manifests: variants,
    };
    let bytes = serde_json::to_vec(&index).unwrap();
    std::fs::write(root.path().join("index.json"), &bytes).unwrap();
    std::fs::write(
        root.path().join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    manifest.snapshot_index = Some(digest("index.json", &bytes));
    save(root, manifest);
}

#[test]
fn indexed_snapshots_select_the_exact_compatible_variant_without_copying() {
    let (root, mut manifest) = artifact();
    let intel = variant(&root, &host());
    let mut amd_host = host();
    amd_host.cpu_vendor = Some("AuthenticAMD".into());
    let amd = variant(&root, &amd_host);
    save_index(&root, &mut manifest, vec![amd, intel]);
    for compatibility in [host(), amd_host] {
        let loaded = Artifact::load_for_host(root.path(), "test", &compatibility).unwrap();
        let snapshot = loaded.snapshot.as_ref().unwrap();
        assert_eq!(
            snapshot.canonicalize().unwrap(),
            root.path().canonicalize().unwrap()
        );
        let index: SnapshotIndex =
            serde_json::from_slice(&std::fs::read(snapshot.join("index.json")).unwrap()).unwrap();
        assert_eq!(index.manifests.len(), 2);
        let selected_digest = loaded.snapshot_digest.as_ref().unwrap();
        let selected: SnapshotManifest = serde_json::from_slice(
            &std::fs::read(snapshot.join("blobs/sha256").join(&selected_digest[7..])).unwrap(),
        )
        .unwrap();
        let memory = snapshot
            .join("blobs/sha256")
            .join(&selected.layers[0].digest[7..]);
        assert_eq!(
            std::fs::read(&memory).unwrap(),
            compatibility.cpu_vendor.unwrap().as_bytes()
        );
        assert!(snapshot.join("oci-layout").is_file());
        assert_eq!(
            std::fs::read(loaded.verified_boot_files().unwrap().rootfs).unwrap(),
            b"test rootfs"
        );
    }
}

#[test]
fn incompatible_or_unknown_hosts_cold_boot_without_loading_any_snapshot() {
    let (root, mut manifest) = artifact();
    let intel = variant(&root, &host());
    save_index(&root, &mut manifest, vec![intel.clone()]);
    // An unselected blob may not be used for fallback.
    std::fs::remove_file(root.path().join("blobs/sha256").join(&intel.digest[7..])).unwrap();
    for compatibility in [
        HostCompatibility {
            architecture: "aarch64".into(),
            ..host()
        },
        HostCompatibility {
            hypervisor: Some("mshv".into()),
            ..host()
        },
        HostCompatibility {
            cpu_vendor: Some("AuthenticAMD".into()),
            ..host()
        },
        HostCompatibility {
            hypervisor: None,
            ..host()
        },
        HostCompatibility {
            cpu_vendor: None,
            ..host()
        },
    ] {
        let loaded = Artifact::load_for_host(root.path(), "test", &compatibility).unwrap();
        assert!(loaded.snapshot.is_none());
        assert_eq!(
            std::fs::read(loaded.verified_boot_files().unwrap().rootfs).unwrap(),
            b"test rootfs"
        );
        assert_eq!(loaded.program, "# external provider\n");
    }
    save_index(&root, &mut manifest, vec![]);
    assert!(
        Artifact::load_for_host(root.path(), "test", &host())
            .unwrap()
            .snapshot
            .is_none()
    );
}

#[test]
fn variant_selection_is_order_independent_and_requires_all_compatibility_fields() {
    let (root, _) = artifact();
    let first = variant(&root, &host());
    let mut second = first.clone();
    second.digest = format!("sha256:{}", "0".repeat(64));
    let mut missing_vendor = second.clone();
    missing_vendor
        .annotations
        .remove("dev.hyperlight.snapshot.cpu.vendor");
    let mut missing_hypervisor = second.clone();
    missing_hypervisor
        .annotations
        .remove("dev.hyperlight.snapshot.hypervisor");
    let mut missing_arch = second.clone();
    missing_arch
        .annotations
        .remove("dev.hyperlight.snapshot.arch");
    let mut missing_key = second.clone();
    missing_key
        .annotations
        .remove("org.opencontainers.image.ref.name");
    let mut different_key = second.clone();
    different_key.annotations.insert(
        "org.opencontainers.image.ref.name".into(),
        format!("{}-k0000000000000000-c999", lampyr_cli::assets::VERSION),
    );
    let mut unsupported = second.clone();
    unsupported.media_type = "application/unknown".into();
    let mut index = SnapshotIndex {
        schema_version: 2,
        media_type: "application/vnd.oci.image.index.v1+json".into(),
        manifests: vec![
            missing_vendor,
            missing_hypervisor,
            missing_arch,
            missing_key,
            different_key,
            unsupported,
        ],
    };
    assert!(index.select(&host()).unwrap().is_none());
    index.manifests.extend([first, second.clone()]);
    assert_eq!(
        index.select(&host()).unwrap().unwrap().digest,
        second.digest
    );
    index.manifests.reverse();
    assert_eq!(
        index.select(&host()).unwrap().unwrap().digest,
        second.digest
    );
}

#[test]
fn selected_snapshot_corruption_is_never_replaced_by_cold_boot() {
    for corrupt in [
        "manifest",
        "config",
        "memory",
        "second memory layer",
        "transport",
    ] {
        let (root, mut manifest) = artifact();
        let selected = variant(&root, &host());
        let manifest_path = root.path().join("blobs/sha256").join(&selected.digest[7..]);
        let selected_manifest: SnapshotManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        let path = match corrupt {
            "manifest" => manifest_path,
            "config" => root
                .path()
                .join("blobs/sha256")
                .join(&selected_manifest.config.digest[7..]),
            "memory" => root
                .path()
                .join("blobs/sha256")
                .join(&selected_manifest.layers[0].digest[7..]),
            "second memory layer" => root
                .path()
                .join("blobs/sha256")
                .join(&selected_manifest.layers[1].digest[7..]),
            _ => root
                .path()
                .join("blobs/sha256")
                .join(&selected_manifest.layers[2].digest[7..]),
        };
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 1;
        std::fs::write(path, bytes).unwrap();
        save_index(&root, &mut manifest, vec![selected]);
        let error = Artifact::load_for_host(root.path(), "test", &host())
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("checksum mismatch"),
            "{corrupt}: {error}"
        );
    }
}

#[test]
fn memory_corrupted_after_a_verified_load_still_fails_the_next_load() {
    let (root, mut manifest) = artifact();
    let selected = variant(&root, &host());
    let selected_manifest: SnapshotManifest = serde_json::from_slice(
        &std::fs::read(root.path().join("blobs/sha256").join(&selected.digest[7..])).unwrap(),
    )
    .unwrap();
    save_index(&root, &mut manifest, vec![selected]);
    // The first load hashes the memory image and records it as verified for this process.
    Artifact::load_for_host(root.path(), "test", &host()).unwrap();
    let memory = root
        .path()
        .join("blobs/sha256")
        .join(&selected_manifest.layers[0].digest[7..]);
    let mut bytes = std::fs::read(&memory).unwrap();
    bytes[0] ^= 1;
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&memory, bytes).unwrap();
    let error = Artifact::load_for_host(root.path(), "test", &host())
        .err()
        .unwrap();
    assert!(error.to_string().contains("checksum mismatch"), "{error}");
}

#[test]
fn selected_descriptors_check_size_digest_and_native_media_types() {
    for invalid in [
        "short size",
        "long size",
        "unsafe digest",
        "unsupported media type",
        "missing blob",
    ] {
        let (root, mut manifest) = artifact();
        let mut selected = variant(&root, &host());
        let expected = match invalid {
            "short size" => {
                selected.size -= 1;
                "size limit"
            }
            "long size" => {
                selected.size += 1;
                "size mismatch"
            }
            "unsafe digest" => {
                selected.digest = "sha256:../../outside".into();
                "invalid provider snapshot OCI digest"
            }
            "missing blob" => {
                std::fs::remove_file(root.path().join("blobs/sha256").join(&selected.digest[7..]))
                    .unwrap();
                "open provider artifact file"
            }
            _ => {
                let path = root.path().join("blobs/sha256").join(&selected.digest[7..]);
                let mut value: SnapshotManifest =
                    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
                value.layers[0].media_type = "application/vnd.oci.image.layer.v1.tar+gzip".into();
                let descriptor = blob(
                    &root,
                    &value.media_type,
                    &serde_json::to_vec(&value).unwrap(),
                );
                selected.digest = descriptor.digest;
                selected.size = descriptor.size;
                "unsupported selected provider snapshot OCI manifest"
            }
        };
        save_index(&root, &mut manifest, vec![selected]);
        let error = Artifact::load_for_host(root.path(), "test", &host())
            .err()
            .unwrap();
        assert!(error.to_string().contains(expected), "{invalid}: {error}");
    }
}

#[test]
fn corrupt_or_unsupported_indexes_are_errors_even_on_incompatible_hosts() {
    let (root, mut manifest) = artifact();
    save_index(&root, &mut manifest, vec![]);
    std::fs::write(root.path().join("index.json"), b"corrupt").unwrap();
    assert!(
        Artifact::load_for_host(root.path(), "test", &host())
            .err()
            .unwrap()
            .to_string()
            .contains("checksum mismatch")
    );
    for bytes in [
        br#"{"schemaVersion":1,"mediaType":"application/vnd.oci.image.index.v1+json","manifests":[]}"#.as_slice(),
        b"not JSON",
    ] {
        std::fs::write(root.path().join("index.json"), bytes).unwrap();
        manifest.snapshot_index = Some(digest("index.json", bytes));
        save(&root, &manifest);
        assert!(Artifact::load_for_host(root.path(), "test", &host()).is_err());
    }
}

#[test]
fn cold_manifest_serialization_does_not_add_a_snapshot_index_field() {
    let (_, manifest) = artifact();
    assert!(
        serde_json::to_value(manifest)
            .unwrap()
            .get("snapshotIndex")
            .is_none()
    );
}

#[test]
fn native_layout_paths_are_relative_to_the_index_directory() {
    let (root, mut manifest) = artifact();
    let selected = variant(&root, &host());
    save_index(&root, &mut manifest, vec![selected]);
    let layout = root.path().join("snapshots");
    std::fs::create_dir(&layout).unwrap();
    for path in ["index.json", "oci-layout", "blobs"] {
        std::fs::rename(root.path().join(path), layout.join(path)).unwrap();
    }
    manifest.snapshot_index.as_mut().unwrap().path = "snapshots/index.json".into();
    save(&root, &manifest);
    let loaded = Artifact::load_for_host(root.path(), "test", &host()).unwrap();
    assert!(loaded.snapshot.unwrap().join("index.json").is_file());
}

#[cfg(unix)]
#[test]
fn sparse_native_memory_is_verified_without_allocating_a_copy() {
    use std::os::unix::fs::MetadataExt;

    let (root, mut manifest) = artifact();
    let mut selected = variant(&root, &host());
    let mut native: SnapshotManifest = serde_json::from_slice(
        &std::fs::read(root.path().join("blobs/sha256").join(&selected.digest[7..])).unwrap(),
    )
    .unwrap();
    let zeros = vec![0_u8; 16 << 20];
    let hash = digest("", &zeros).sha256;
    let memory = root.path().join("blobs/sha256").join(&hash);
    let file = std::fs::File::create(&memory).unwrap();
    file.set_len(zeros.len() as u64).unwrap();
    let before = file.metadata().unwrap().blocks();
    native.layers[0].digest = format!("sha256:{hash}");
    native.layers[0].size = zeros.len() as u64;
    let replacement = blob(
        &root,
        &native.media_type,
        &serde_json::to_vec(&native).unwrap(),
    );
    selected.digest = replacement.digest;
    selected.size = replacement.size;
    save_index(&root, &mut manifest, vec![selected]);
    let index = std::fs::read(root.path().join("index.json")).unwrap();

    let loaded = Artifact::load_for_host(root.path(), "test", &host()).unwrap();
    assert_eq!(
        loaded.snapshot.unwrap(),
        root.path().canonicalize().unwrap()
    );
    assert_eq!(
        std::fs::read(root.path().join("index.json")).unwrap(),
        index
    );
    assert_eq!(file.metadata().unwrap().blocks(), before);
    assert!(before * 512 < zeros.len() as u64);
}

#[test]
fn wrong_snapshot_keys_cold_boot_without_reading_the_variant() {
    let version = lampyr_cli::assets::VERSION;
    for tag in [
        None,
        Some(version.to_owned()),
        Some(format!("{version}-k0000000000000000-c999")),
    ] {
        let (root, mut manifest) = artifact();
        let mut selected = variant(&root, &host());
        match tag {
            Some(tag) => {
                selected
                    .annotations
                    .insert("org.opencontainers.image.ref.name".into(), tag);
            }
            None => {
                selected
                    .annotations
                    .remove("org.opencontainers.image.ref.name");
            }
        }
        std::fs::remove_file(root.path().join("blobs/sha256").join(&selected.digest[7..])).unwrap();
        save_index(&root, &mut manifest, vec![selected]);
        assert!(
            Artifact::load_for_host(root.path(), "test", &host())
                .unwrap()
                .snapshot
                .is_none()
        );
    }
}

#[test]
fn native_config_must_confirm_the_index_compatibility_annotations() {
    let (root, mut manifest) = artifact();
    let mut other_host = host();
    other_host.cpu_vendor = Some("AuthenticAMD".into());
    let mut selected = variant(&root, &other_host);
    selected.annotations.insert(
        "dev.hyperlight.snapshot.cpu.vendor".into(),
        "GenuineIntel".into(),
    );
    save_index(&root, &mut manifest, vec![selected]);
    let error = Artifact::load_for_host(root.path(), "test", &host())
        .err()
        .unwrap();
    assert!(
        error
            .to_string()
            .contains("config does not match host compatibility")
    );
}

#[test]
fn selected_native_layout_requires_a_valid_marker() {
    for marker in [
        None,
        Some("not JSON"),
        Some(r#"{"imageLayoutVersion":"2.0.0"}"#),
    ] {
        let (root, mut manifest) = artifact();
        let selected = variant(&root, &host());
        save_index(&root, &mut manifest, vec![selected]);
        match marker {
            Some(marker) => std::fs::write(root.path().join("oci-layout"), marker).unwrap(),
            None => std::fs::remove_file(root.path().join("oci-layout")).unwrap(),
        }
        assert!(Artifact::load_for_host(root.path(), "test", &host()).is_err());
    }
}

#[cfg(unix)]
#[test]
fn native_snapshot_blobs_cannot_escape_the_layout_directory() {
    let (root, mut manifest) = artifact();
    let selected = variant(&root, &host());
    save_index(&root, &mut manifest, vec![selected.clone()]);
    let blob = root.path().join("blobs/sha256").join(&selected.digest[7..]);
    let outside = tempfile::NamedTempFile::new().unwrap();
    std::fs::copy(&blob, outside.path()).unwrap();
    std::fs::remove_file(&blob).unwrap();
    std::os::unix::fs::symlink(outside.path(), &blob).unwrap();
    assert!(Artifact::load_for_host(root.path(), "test", &host()).is_err());
}

#[test]
fn hyperlight_native_writer_round_trips_through_provider_variant_loading() {
    let (root, mut manifest) = artifact();
    let rootfs = std::env::var_os("LAMPYR_TEST_ROOTFS")
        .expect("set LAMPYR_TEST_ROOTFS to a runnable runtime CPIO");
    let cpio = std::fs::read(rootfs).unwrap();
    std::fs::write(root.path().join("rootfs.cpio"), &cpio).unwrap();
    manifest.rootfs = digest("rootfs.cpio", &cpio);
    let layout = root.path().join("snapshot");
    let mut sandbox =
        hyperlight_unikraft::SandboxBuilder::from_initrd(root.path().join("rootfs.cpio"))
            .scratch_mb(128)
            .boot()
            .unwrap();
    sandbox.snapshot_to(&layout).unwrap_or_else(|error| {
        panic!(
            "save native snapshot: {error}; guest output: {}",
            sandbox.drain_output()
        )
    });
    drop(sandbox);
    let mut index: SnapshotIndex =
        serde_json::from_slice(&std::fs::read(layout.join("index.json")).unwrap()).unwrap();
    let mut incompatible = index.manifests[0].clone();
    incompatible.annotations.insert(
        "dev.hyperlight.snapshot.cpu.vendor".into(),
        "IncompatibleVendor".into(),
    );
    incompatible.digest = format!("sha256:{}", "0".repeat(64));
    incompatible.size = 0;
    index.manifests.insert(0, incompatible);
    let index = serde_json::to_vec(&index).unwrap();
    std::fs::write(layout.join("index.json"), &index).unwrap();
    manifest.snapshot_index = Some(digest("snapshot/index.json", &index));
    save(&root, &manifest);
    let loaded = Artifact::load(root.path(), "test").unwrap();
    let snapshot = loaded
        .snapshot
        .as_ref()
        .expect("native snapshot must be selected");
    assert_eq!(snapshot, &layout.canonicalize().unwrap());
    assert_eq!(std::fs::read(layout.join("index.json")).unwrap(), index);
    let selected: SnapshotManifest = serde_json::from_slice(
        &std::fs::read(
            layout
                .join("blobs/sha256")
                .join(&loaded.snapshot_digest.as_ref().unwrap()[7..]),
        )
        .unwrap(),
    )
    .unwrap();
    let transport = layout
        .join("blobs/sha256")
        .join(&selected.layers.last().unwrap().digest[7..]);
    let original = std::fs::read(&transport).unwrap();
    let mut corrupt = original.clone();
    corrupt[0] ^= 1;
    std::fs::write(&transport, corrupt).unwrap();
    assert!(loaded.load_snapshot().is_err());
    std::fs::write(&transport, original).unwrap();
    let mut restored =
        hyperlight_unikraft::SandboxBuilder::from_snapshot(loaded.load_snapshot().unwrap())
            .boot()
            .unwrap();
    restored.snapshot().unwrap();
}

#[test]
fn artifacts_check_identity_protocol_runtime_and_file_integrity() {
    let (root, mut manifest) = artifact();
    assert!(Artifact::load(root.path(), "different-provider").is_err());
    manifest.protocol_version = 2;
    save(&root, &manifest);
    assert!(Artifact::load(root.path(), "test").is_err());
    manifest.protocol_version = 1;
    manifest.runtime_version = "0.17.0".to_owned();
    save(&root, &manifest);
    assert!(Artifact::load(root.path(), "test").is_err());
    manifest.runtime_version = lampyr_cli::assets::VERSION.to_owned();
    save(&root, &manifest);
    std::fs::write(root.path().join("provider.py"), b"tampered").unwrap();
    let error = Artifact::load(root.path(), "test").err().unwrap();
    assert!(error.to_string().contains("checksum mismatch"));
}

#[test]
fn provider_artifact_manifests_without_snapshots_remain_loadable() {
    let (root, _) = artifact();
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.path().join("manifest.json")).unwrap()).unwrap();
    manifest.as_object_mut().unwrap().remove("snapshot");
    std::fs::write(
        root.path().join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let loaded = Artifact::load(root.path(), "test").unwrap();
    assert!(loaded.snapshot.is_none());
    assert!(
        serde_json::to_value(&loaded.manifest)
            .unwrap()
            .get("snapshot")
            .is_none()
    );
}

#[test]
fn provider_cold_boot_compatibility_is_checked_only_for_fallback() {
    let (root, mut manifest) = artifact();
    manifest.cold_compatibility = Some(ColdCompatibility {
        os: "other".into(),
        architecture: "other".into(),
    });
    save(&root, &manifest);
    let loaded = Artifact::load(root.path(), "test").unwrap();
    assert!(
        loaded
            .ensure_cold_compatible()
            .unwrap_err()
            .to_string()
            .contains("cold-boot artifact targets other/other")
    );

    manifest.cold_compatibility = Some(ColdCompatibility {
        os: std::env::consts::OS.into(),
        architecture: std::env::consts::ARCH.into(),
    });
    save(&root, &manifest);
    Artifact::load(root.path(), "test")
        .unwrap()
        .ensure_cold_compatible()
        .unwrap();
}

#[test]
fn provider_snapshot_repository_is_validated_and_unknown_hosts_still_cold_boot() {
    let (root, mut manifest) = artifact();
    manifest.snapshot_index = Some(digest("index.json", b"missing"));
    manifest.snapshot_repository =
        Some("ghcr.io/hyperlight-dev/lampyr/provider-test-snapshots".into());
    save(&root, &manifest);
    let loaded = Artifact::load_for_host(
        root.path(),
        "test",
        &HostCompatibility {
            architecture: "aarch64".into(),
            hypervisor: None,
            cpu_vendor: None,
        },
    )
    .unwrap();
    assert!(loaded.snapshot.is_none());

    manifest.snapshot_repository = Some("https://example.com/repository?tag=bad".into());
    save(&root, &manifest);
    assert!(
        Artifact::load_for_host(
            root.path(),
            "test",
            &HostCompatibility {
                architecture: "aarch64".into(),
                hypervisor: None,
                cpu_vendor: None,
            },
        )
        .err()
        .unwrap()
        .to_string()
        .contains("without a tag, digest or query")
    );
}

#[test]
fn installed_boot_files_are_reverified_without_copying() {
    let (root, mut manifest) = artifact();
    std::fs::write(root.path().join("kernel"), b"test kernel").unwrap();
    manifest.kernel = Some(digest("kernel", b"test kernel"));
    save(&root, &manifest);
    let artifact = Artifact::load(root.path(), "test").unwrap();
    let boot_files = artifact.verified_boot_files().unwrap();
    let canonical_root = root.path().canonicalize().unwrap();
    assert_eq!(boot_files.rootfs, canonical_root.join("rootfs.cpio"));
    assert_eq!(boot_files.kernel, Some(canonical_root.join("kernel")));
    std::fs::write(
        root.path().join("rootfs.cpio"),
        b"replaced after verification",
    )
    .unwrap();
    assert!(
        artifact
            .verified_boot_files()
            .err()
            .unwrap()
            .to_string()
            .contains("checksum mismatch")
    );
    assert_eq!(artifact.program, "# external provider\n");
    assert!(artifact.directory().join("manifest.json").is_file());
}

#[test]
fn snapshot_selection_skips_cold_boot_file_hashing() {
    let (root, mut manifest) = artifact();
    let selected = variant(&root, &host());
    save_index(&root, &mut manifest, vec![selected]);
    std::fs::write(root.path().join("rootfs.cpio"), b"not the approved rootfs").unwrap();
    let artifact = Artifact::load_for_host(root.path(), "test", &host()).unwrap();
    assert!(artifact.snapshot.is_some());
    assert!(artifact.verified_boot_files().is_err());
}

#[cfg(unix)]
#[test]
fn artifact_paths_must_be_owner_controlled() {
    use std::os::unix::fs::PermissionsExt;

    let (root, _) = artifact();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        Artifact::load(root.path(), "test")
            .err()
            .unwrap()
            .to_string()
            .contains("must not be writable by other users")
    );

    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::set_permissions(
        root.path().join("rootfs.cpio"),
        std::fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    assert!(
        Artifact::load(root.path(), "test")
            .err()
            .unwrap()
            .to_string()
            .contains("must not be writable by other users")
    );

    std::fs::set_permissions(
        root.path().join("rootfs.cpio"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let nested = root.path().join("snapshot");
    std::fs::create_dir(&nested).unwrap();
    std::fs::set_permissions(&nested, std::fs::Permissions::from_mode(0o777)).unwrap();
    assert!(
        Artifact::load(root.path(), "test")
            .err()
            .unwrap()
            .to_string()
            .contains("must not be writable by other users")
    );
}

/// A 0775 ancestor is only safe when its group is the user's private group.
/// Here its group is one of the user's other groups, which has other members
/// or at least isn't named after the user.
#[cfg(target_os = "linux")]
#[test]
fn a_shared_group_writable_ancestor_is_refused() {
    use std::os::unix::fs::PermissionsExt;

    // SAFETY: getegid has no preconditions.
    let egid = unsafe { libc::getegid() };
    let mut groups = vec![0; 256];
    // SAFETY: groups has room for as many entries as passed.
    let count = unsafe { libc::getgroups(groups.len() as i32, groups.as_mut_ptr()) };
    let Some(&shared) = groups[..count.max(0) as usize]
        .iter()
        .find(|&&gid| gid != egid)
    else {
        // The user belongs to no other group to make the ancestor shared.
        return;
    };
    let (root, _) = artifact();
    let outer = tempfile::tempdir().unwrap();
    let parent = outer.path().join("shared");
    std::fs::create_dir(&parent).unwrap();
    std::os::unix::fs::chown(&parent, None, Some(shared)).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o775)).unwrap();
    let moved = parent.join("artifact");
    std::fs::rename(root.keep(), &moved).unwrap();
    let error = Artifact::load(&moved, "test").err().unwrap().to_string();
    assert!(
        error.contains("must not be writable by other users")
            && error.contains(&parent.display().to_string()),
        "{error}"
    );
}

#[cfg(unix)]
#[test]
fn artifact_ancestors_must_prevent_directory_replacement() {
    use std::os::unix::fs::PermissionsExt;

    let (root, _) = artifact();
    let parent = tempfile::tempdir().unwrap();
    let shared = parent.path().join("shared");
    std::fs::create_dir(&shared).unwrap();
    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o777)).unwrap();
    let installed = shared.join("artifact");
    std::fs::rename(root.path(), &installed).unwrap();
    let error = Artifact::load(&installed, "test").err().unwrap();
    assert!(
        error.to_string().contains("without the sticky bit"),
        "{error}"
    );

    std::fs::set_permissions(&shared, std::fs::Permissions::from_mode(0o1777)).unwrap();
    Artifact::load(&installed, "test").unwrap();
}

#[test]
fn provider_artifacts_cannot_request_platform_lifecycle_or_unknown_capabilities() {
    for operation in [Operation::Install, Operation::Health, Operation::Uninstall] {
        let (root, mut manifest) = artifact();
        manifest.operations.insert(operation, BTreeSet::new());
        save(&root, &manifest);
        assert!(
            Artifact::load(root.path(), "test")
                .err()
                .unwrap()
                .to_string()
                .contains("only infrastructure and event source operations")
        );
    }

    for capability in ["tools", "platform"] {
        let (root, manifest) = artifact();
        let mut value = serde_json::to_value(manifest).unwrap();
        value["operations"]["apply"] = json!([capability]);
        std::fs::write(root.path().join("manifest.json"), value.to_string()).unwrap();
        assert!(Artifact::load(root.path(), "test").is_err());
    }
}

#[test]
fn provider_artifacts_may_own_event_source_operations_without_kubernetes_access() {
    let (root, mut manifest) = artifact();
    manifest.source_kinds.insert(SourceKind {
        api_version: "events.example.dev/v1".into(),
        kind: "ExampleSource".into(),
    });
    manifest.operations.insert(
        Operation::SourceCreate,
        BTreeSet::from([
            Capability::WorkspaceRead,
            Capability::State,
            Capability::Network,
            Capability::AuthCache,
        ]),
    );
    save(&root, &manifest);
    let loaded = Artifact::load(root.path(), "test").unwrap();
    assert!(
        loaded
            .manifest
            .operations
            .contains_key(&Operation::SourceCreate)
    );
}

#[test]
fn event_source_kinds_reject_kubernetes_workloads_and_builtin_sources() {
    for (api_version, kind) in [
        ("v1", "Pod"),
        ("rbac.authorization.k8s.io/v1", "RoleBinding"),
        ("sources.knative.dev/v1", "ContainerSource"),
    ] {
        let (root, mut manifest) = artifact();
        manifest.source_kinds.insert(SourceKind {
            api_version: api_version.into(),
            kind: kind.into(),
        });
        manifest
            .operations
            .insert(Operation::SourceCreate, BTreeSet::new());
        save(&root, &manifest);
        assert!(Artifact::load(root.path(), "test").is_err(), "{kind}");
    }

    let (root, mut manifest) = artifact();
    manifest.source_kinds.insert(SourceKind {
        api_version: "eventing.knative.dev/v1".into(),
        kind: "Trigger".into(),
    });
    manifest
        .operations
        .insert(Operation::SourceCreate, BTreeSet::new());
    save(&root, &manifest);
    assert!(Artifact::load(root.path(), "test").is_ok());
}

#[test]
fn provider_manifest_capability_is_limited_to_apply() {
    let (root, mut manifest) = artifact();
    manifest
        .operations
        .get_mut(&Operation::Apply)
        .unwrap()
        .insert(Capability::ProviderManifests);
    save(&root, &manifest);
    assert!(Artifact::load(root.path(), "test").is_ok());

    manifest.operations.insert(
        Operation::Init,
        BTreeSet::from([Capability::ProviderManifests]),
    );
    save(&root, &manifest);
    assert!(
        Artifact::load(root.path(), "test")
            .err()
            .unwrap()
            .to_string()
            .contains("valid only for apply")
    );
}

#[test]
fn requested_capabilities_do_not_authorize_themselves_or_require_a_hypervisor() {
    let (root, _) = artifact();
    let artifact = Artifact::load(root.path(), "test").unwrap();
    let error = Provider::boot(
        artifact,
        Operation::Apply,
        Authorization::default(),
        std::time::Duration::from_secs(1),
        Cancellation::default(),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("State is not authorized"));
}

#[cfg(unix)]
#[test]
fn artifact_symlinks_cannot_read_outside_the_artifact_directory() {
    let (root, manifest) = artifact();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("provider.py"), b"# external provider\n").unwrap();
    std::fs::remove_file(root.path().join("provider.py")).unwrap();
    std::os::unix::fs::symlink(
        outside.path().join("provider.py"),
        root.path().join("provider.py"),
    )
    .unwrap();
    save(&root, &manifest);
    assert!(Artifact::load(root.path(), "test").is_err());
}

#[cfg(unix)]
#[test]
fn artifact_root_cannot_be_a_symlink() {
    let (root, _) = artifact();
    let parent = tempfile::tempdir().unwrap();
    let link = parent.path().join("artifact");
    std::os::unix::fs::symlink(root.path(), &link).unwrap();
    let error = Artifact::load(&link, "test").err().unwrap();
    assert!(
        error.to_string().contains("must not be a symlink"),
        "{error}"
    );
}

#[test]
fn provider_packaging_and_manifests_use_the_cli_runtime_version() {
    let version = lampyr_cli::assets::VERSION;
    let packaging = include_str!("../../providers/snapshot_oci.py");
    assert!(packaging.contains(&format!("RUNTIME_VERSION = \"{version}\"")));
    for manifest in [
        include_str!("../../providers/kind/artifact.json"),
        include_str!("../../providers/azure/artifact.json"),
        include_str!("../../providers/k3d/artifact.json"),
    ] {
        let manifest: serde_json::Value = serde_json::from_str(manifest).unwrap();
        assert_eq!(manifest["runtimeVersion"], version);
    }
}
