// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn runtime_helper_accepts_project_names_and_existing_rootfs_driver_names() {
    let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let docker = directory.path().join("docker");
    std::fs::write(
        &docker,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$DOCKER_LOG\"\nif [ \"$1\" = image ]; then printf 'linux/amd64\\n'; fi\n",
    )
    .unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();

    for (runtime, driver) in [
        ("node", "node"),
        ("nodejs", "node"),
        ("python", "python"),
        ("go", "go"),
        ("rust", "rust"),
        ("dotnet", "dotnet-aot"),
        ("dotnet-aot", "dotnet-aot"),
    ] {
        let log = directory.path().join(format!("{runtime}.log"));
        let output = Command::new(env!("CARGO_BIN_EXE_runtime-assets"))
            .arg("--root")
            .arg(directory.path())
            .args(["--rootfs", runtime])
            .env("PATH", directory.path())
            .env("DOCKER_LOG", &log)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let invocation = std::fs::read_to_string(log).unwrap();
        let image = invocation.split_whitespace().last().unwrap();
        assert!(image.contains(&format!("/{driver}@sha256:")));
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            format!("hluk-{driver}-rootfs:latest=docker-image://{image}")
        );
        assert!(!invocation.lines().any(|line| line.starts_with("tag ")));
        assert!(!invocation.lines().any(|line| line.starts_with("pull ")));
    }
}

#[test]
fn preparing_another_runtime_version_does_not_replace_an_in_flight_build_base() {
    let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let docker = directory.path().join("docker");
    let state = directory.path().join("rootfs-tag");
    let log = directory.path().join("docker.log");
    let earlier_image = format!(
        "ghcr.io/hyperlight-dev/hyperlight-unikraft/node@sha256:{}",
        "11".repeat(32)
    );
    // An earlier checkout has prepared its base, but has not resolved FROM yet.
    std::fs::write(&state, &earlier_image).unwrap();
    std::fs::write(&docker, include_str!("fixtures/rootfs-docker.sh")).unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();

    // Interleave this checkout's preparation before the earlier Docker build.
    let output = Command::new(env!("CARGO_BIN_EXE_runtime-assets"))
        .arg("--root")
        .arg(directory.path())
        .args(["--rootfs", "nodejs"])
        .env("PATH", directory.path())
        .env("DOCKER_STATE", &state)
        .env("DOCKER_LOG", &log)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let invocations = std::fs::read_to_string(&log).unwrap();
    assert!(invocations.contains("/node@sha256:"));
    assert!(!invocations.contains(&earlier_image));
    let prepared_context = String::from_utf8(output.stdout).unwrap();
    let prepared_context = prepared_context.trim();

    let dockerfile = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../templates/services/nodejs/adapter/Dockerfile.rootfs");
    assert_eq!(
        std::fs::read_to_string(&state).unwrap(),
        earlier_image,
        "preparing a different runtime version replaced an in-flight build's shared tag"
    );

    // The new build resolves its own pinned context, not the older shared tag.
    let output = Command::new(&docker)
        .args(["buildx", "build", "--file"])
        .arg(&dockerfile)
        .args(["--build-context", prepared_context])
        .env("DOCKER_STATE", &state)
        .env("DOCKER_LOG", &log)
        .output()
        .unwrap();
    assert!(output.status.success());
    let prepared_image = prepared_context
        .strip_prefix("hluk-node-rootfs:latest=docker-image://")
        .unwrap();
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        prepared_image
    );
    assert_ne!(prepared_image, earlier_image);
}

#[test]
fn rootfs_mock_resolves_immutable_from_without_shared_tag_state() {
    let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let docker = directory.path().join("docker");
    std::fs::write(&docker, include_str!("fixtures/rootfs-docker.sh")).unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();
    let image = format!(
        "ghcr.io/hyperlight-dev/hyperlight-unikraft/node@sha256:{}",
        "11".repeat(32)
    );
    let dockerfile = directory.path().join("Dockerfile.rootfs");
    std::fs::write(&dockerfile, format!("FROM {image} AS rootfs\n")).unwrap();
    let output = Command::new(&docker)
        .args(["buildx", "build", "--file"])
        .arg(&dockerfile)
        .env("DOCKER_LOG", directory.path().join("docker.log"))
        .env("DOCKER_STATE", directory.path().join("absent-tag-state"))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "an immutable FROM must resolve without shared tag state: status={:?}, stderr={}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8(output.stdout).unwrap().trim(), image);
}

#[test]
fn runtime_helper_preserves_platform_pull_and_inspect_error_handling() {
    let directory = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
    let docker = directory.path().join("docker");
    std::fs::write(
        &docker,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$DOCKER_LOG"
case "$1" in
    image)
        case "$INSPECT_RESULT" in
            missing) printf 'No such image\n' >&2; exit 1 ;;
            wrong-platform) printf 'linux/amd64\n' ;;
            denied) printf 'permission denied\n' >&2; exit 1 ;;
        esac
        ;;
    pull) ;;
    *) exit 2 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o700)).unwrap();
    for result in ["missing", "wrong-platform", "denied"] {
        let log = directory.path().join(format!("{result}.log"));
        let output = Command::new(env!("CARGO_BIN_EXE_runtime-assets"))
            .arg("--root")
            .arg(directory.path())
            .args(["--rootfs", "python", "--platform", "linux/arm64"])
            .env("PATH", directory.path())
            .env("DOCKER_LOG", &log)
            .env("INSPECT_RESULT", result)
            .output()
            .unwrap();
        let invocations = std::fs::read_to_string(log).unwrap();
        if result == "denied" {
            assert!(!output.status.success());
            assert!(String::from_utf8_lossy(&output.stderr).contains("permission denied"));
            assert!(output.stdout.is_empty());
            assert!(!invocations.lines().any(|line| line.starts_with("pull ")));
        } else {
            assert!(output.status.success());
            let image = invocations
                .lines()
                .next()
                .unwrap()
                .split_whitespace()
                .last()
                .unwrap();
            assert!(invocations.contains(&format!("pull --platform linux/arm64 {image}")));
            assert_eq!(
                String::from_utf8(output.stdout).unwrap().trim(),
                format!("hluk-python-rootfs:latest=docker-image://{image}")
            );
        }
    }
}
