// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;
use std::path::PathBuf;

use lampyr_cli::build::validate_output;
use lampyr_cli::image::{self, Metadata};
use lampyr_cli::project::{self, Project};

fn artifact() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("lampyr.workspace.yaml"),
        "apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\n",
    )
    .unwrap();
    let service = directory.path().join("services/artifact-test");
    project::initialize(&service, Some("artifact-test"), "nodejs").unwrap();
    let mut project = Project::load(&service).unwrap();
    project.command = "/entrypoint.js <&> \"quoted\" \u{2028}\u{2029}".to_owned();
    std::fs::write(directory.path().join("kernel"), "kernel").unwrap();
    std::fs::create_dir(directory.path().join("snapshot")).unwrap();
    std::fs::write(directory.path().join("snapshot/blob"), "snapshot").unwrap();
    let metadata = Metadata::create(
        project,
        format!("sha256:{}", "01".repeat(32)),
        BTreeMap::from([
            ("rootfs".to_owned(), format!("sha256:{}", "02".repeat(32))),
            (
                "kernel".to_owned(),
                image::file_digest(&directory.path().join("kernel")).unwrap(),
            ),
            (
                "snapshot".to_owned(),
                image::directory_digest(&directory.path().join("snapshot"), &[]).unwrap(),
            ),
        ]),
        "GenuineIntel".to_owned(),
    )
    .unwrap();
    std::fs::write(
        directory.path().join("metadata.json"),
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
    directory
}

#[test]
fn verification_rejects_retired_custom_source_artifacts() {
    let directory = artifact();
    let path = directory.path().join("metadata.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["runtime"]["localSourceTree"] = "previous-source-build".into();
    std::fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        image::verify(directory.path())
            .err()
            .unwrap()
            .to_string()
            .contains("rebuild with the released runtime")
    );
}

#[test]
fn verification_rejects_changed_snapshot_kernel_and_project_metadata() {
    let directory = artifact();
    image::verify(directory.path()).unwrap();
    for (path, original) in [("kernel", "kernel"), ("snapshot/blob", "snapshot")] {
        std::fs::write(directory.path().join(path), "changed").unwrap();
        assert!(
            image::verify(directory.path())
                .err()
                .unwrap()
                .to_string()
                .contains("digest does not match")
        );
        std::fs::write(directory.path().join(path), original).unwrap();
    }
    let path = directory.path().join("metadata.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["project"]["name"] = "changed".into();
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    assert!(
        image::verify(directory.path())
            .err()
            .unwrap()
            .to_string()
            .contains("revision ID does not match")
    );
}

#[test]
fn source_digest_ignores_generated_files_but_includes_executable_modes_and_link_targets() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(directory.path().join("handler"), "application").unwrap();
    let ignored = [PathBuf::from(".lampyr")];
    let before = image::directory_digest(directory.path(), &ignored).unwrap();
    std::fs::create_dir(directory.path().join(".lampyr")).unwrap();
    std::fs::write(directory.path().join(".lampyr/generated"), "generated").unwrap();
    assert_eq!(
        before,
        image::directory_digest(directory.path(), &ignored).unwrap()
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        std::fs::set_permissions(
            directory.path().join("handler"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_ne!(
            before,
            image::directory_digest(directory.path(), &ignored).unwrap()
        );
        let link = directory.path().join("link");
        symlink("handler", &link).unwrap();
        let original_link = image::directory_digest(directory.path(), &ignored).unwrap();
        std::fs::remove_file(&link).unwrap();
        symlink("other", &link).unwrap();
        assert_ne!(
            original_link,
            image::directory_digest(directory.path(), &ignored).unwrap()
        );
    }
}

#[test]
fn adapter_sources_metadata_and_baselines_change_source_revision_and_artifact_identity() {
    let directory = tempfile::tempdir().unwrap();
    project::initialize(directory.path(), Some("example"), "nodejs").unwrap();
    let ignored = [PathBuf::from(".git"), PathBuf::from(".lampyr")];
    let project = Project::load(directory.path()).unwrap();
    let identity = || {
        let source = image::directory_digest(directory.path(), &ignored).unwrap();
        let metadata = Metadata::create(
            project.clone(),
            source.clone(),
            BTreeMap::from([
                ("rootfs".to_owned(), format!("sha256:{}", "02".repeat(32))),
                ("kernel".to_owned(), format!("sha256:{}", "03".repeat(32))),
            ]),
            "GenuineIntel".to_owned(),
        )
        .unwrap();
        let artifact_metadata = directory.path().join(".lampyr/metadata.json");
        std::fs::create_dir_all(artifact_metadata.parent().unwrap()).unwrap();
        std::fs::write(&artifact_metadata, serde_json::to_vec(&metadata).unwrap()).unwrap();
        (
            source,
            metadata.revision_id.clone(),
            metadata
                .local_image(artifact_metadata.parent().unwrap())
                .unwrap(),
        )
    };
    let original = identity();
    for path in [
        ".lampyr-adapter/server.js",
        ".lampyr-adapter/Dockerfile.rootfs",
        ".lampyr-adapter/adapter.json",
        ".lampyr-adapter/baseline/server.js",
        ".lampyr-adapter/baseline/Dockerfile.rootfs",
    ] {
        let path = directory.path().join(path);
        let bytes = std::fs::read(&path).unwrap();
        let mut modified = bytes.clone();
        modified.extend_from_slice(if path.extension().is_some_and(|ext| ext == "json") {
            b"\n "
        } else {
            b"\ncustomized"
        });
        std::fs::write(&path, modified).unwrap();
        if path.components().any(|part| part.as_os_str() == "baseline") {
            let error = Project::load(directory.path()).err().unwrap();
            assert!(format!("{error:#}").contains("baseline was modified"));
        } else {
            Project::load(directory.path()).unwrap();
        }
        let changed = identity();
        assert_ne!(
            original.0,
            changed.0,
            "source must include {}",
            path.display()
        );
        assert_ne!(
            original.1,
            changed.1,
            "revision must include {}",
            path.display()
        );
        assert_ne!(
            original.2,
            changed.2,
            "artifact must include {}",
            path.display()
        );
        std::fs::write(path, bytes).unwrap();
        assert_eq!(original, identity());
    }
    std::fs::write(directory.path().join(".lampyr/transient"), "machine state").unwrap();
    assert_eq!(original, identity());
}

#[test]
fn build_output_cannot_replace_roots_customer_files_or_symlinked_directories() {
    let directory = tempfile::tempdir().unwrap();
    let project = directory.path().join("project");
    let root = directory.path().join("platform");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&root).unwrap();
    for unsafe_path in [directory.path(), &project, &root] {
        assert!(validate_output(unsafe_path, &project, &root).is_err());
    }
    let output = directory.path().join("artifact");
    std::fs::create_dir(&output).unwrap();
    std::fs::write(output.join("notes"), "customer data").unwrap();
    assert!(validate_output(&output, &project, &root).is_err());
    std::fs::write(
        output.join(".lampyr-artifact"),
        format!("{}\n", image::SCHEMA),
    )
    .unwrap();
    assert!(validate_output(&output, &project, &root).is_err());
    assert_eq!(
        std::fs::read_to_string(output.join("notes")).unwrap(),
        "customer data"
    );
    let owned = directory.path().join("owned");
    std::fs::create_dir(&owned).unwrap();
    std::fs::write(
        owned.join(".lampyr-artifact"),
        format!("{}\n", image::SCHEMA),
    )
    .unwrap();
    validate_output(&owned, &project, &root).unwrap();
    #[cfg(unix)]
    {
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&owned, &link).unwrap();
        assert!(validate_output(&link, &project, &root).is_err());
        let via_link = link.join("../project");
        assert!(validate_output(&via_link, &project, &root).is_err());
    }
}

#[test]
fn registry_destinations_require_real_tags_and_do_not_confuse_ports_with_tags() {
    for invalid in [
        "",
        "lampyr.local/app:dev",
        "LAMPYR.LOCAL/app:dev",
        "localhost:5002/app",
        "repo:",
        "repo:bad tag",
        "repo:-invalid",
        "--help",
        "repo@sha256:abc",
    ] {
        assert!(
            lampyr_cli::registry::validate_tag(invalid).is_err(),
            "{invalid}"
        );
    }
    for valid in [
        "localhost:5002/app:dev",
        "registry.example/team/app:v1",
        "app:latest",
        "[::1]:5002/app:dev",
    ] {
        lampyr_cli::registry::validate_tag(valid).unwrap();
    }
}
