// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::process::Command;

use lampyr_cli::project::Project;

fn fixture() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../samples/local-worker-workspace")
}

#[test]
fn dev_help_offers_request_metrics() {
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["dev", "--help"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("--metrics"));
}

#[test]
fn workspace_exposure_is_validated_before_building() {
    for (ingress, services, expected) in [
        (
            None,
            vec!["processor", "reused"],
            "local supervisor is missing",
        ),
        (
            Some("services: [processor]"),
            vec!["processor", "reused"],
            "exposed service \"processor\" is not in this workspace",
        ),
        (
            Some("services: [worker-missing]"),
            vec!["processor", "reused"],
            "exposed service \"worker-missing\" is not in this workspace",
        ),
        (
            Some("services: [worker-reused, worker-processor]"),
            vec!["processor", "reused"],
            "local supervisor is missing",
        ),
        (
            Some("services: [worker-reused, worker-reused]"),
            vec!["processor", "reused"],
            "duplicate exposed service",
        ),
        (
            Some("services: []"),
            vec!["processor", "reused"],
            "local supervisor is missing",
        ),
        (
            Some("service: worker-reused"),
            vec!["processor", "reused"],
            "ingress.service was replaced by ingress.services",
        ),
        (None, vec!["processor"], "local supervisor is missing"),
    ] {
        let directory = tempfile::tempdir().unwrap();
        let mut manifest = "apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\n".to_owned();
        if let Some(ingress) = ingress {
            manifest.push_str(&format!("ingress:\n  {ingress}\n"));
        }
        std::fs::write(directory.path().join("lampyr.workspace.yaml"), manifest).unwrap();
        std::fs::write(directory.path().join(".lampyr-platform"), "test\n").unwrap();
        for service in services {
            let target = directory.path().join("services").join(service);
            std::fs::create_dir_all(&target).unwrap();
            std::fs::copy(
                fixture().join("services").join(service).join("lampyr.yaml"),
                target.join("lampyr.yaml"),
            )
            .unwrap();
        }
        let output = Command::new(env!("CARGO_BIN_EXE_ly"))
            .arg("dev")
            .arg(directory.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains(expected), "{ingress:?}: {error}");
        if ingress.is_none() || ingress == Some("services: []") {
            assert!(error.contains("no services are exposed"), "{error}");
        }
        assert!(!directory.path().join(".lampyr/dev").exists());
    }
}

#[test]
fn service_directory_cannot_bypass_workspace_exposure_validation() {
    let root = tempfile::tempdir().unwrap();
    let service = root.path().join("services/api");
    lampyr_cli::project::initialize_application(&service, Some("api"), "nodejs", true).unwrap();
    std::fs::write(
        root.path().join("lampyr.workspace.yaml"),
        "apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\ningress:\n  services: [missing]\n",
    )
    .unwrap();
    for command in ["dev", "deploy"] {
        let mut cli = Command::new(env!("CARGO_BIN_EXE_ly"));
        cli.arg(command).arg(&service);
        if command == "deploy" {
            cli.args(["--image", "example.invalid/api:test"]);
        }
        let output = cli.output().unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("exposed service \"missing\" is not in this workspace"),
            "{command}: {error}"
        );
        assert!(!service.join(".lampyr").exists());
    }
}

#[test]
fn bindings_are_validated_and_retained_in_artifact_identity() {
    let directory = fixture().join("services/api");
    let project = Project::load(&directory).unwrap();
    assert_eq!(project.bindings["processor"], "worker-processor");
    let encoded = serde_json::to_value(&project).unwrap();
    assert_eq!(encoded["bindings"]["reused"], "worker-reused");
    for name in ["UPPERCASE", "1invalid", "bad-name"] {
        let mut invalid = project.clone();
        invalid
            .bindings
            .insert(name.into(), "worker-processor".into());
        assert!(invalid.validate().is_err());
    }
}

#[test]
fn single_service_uses_snapshot_supervisor_for_every_application_model() {
    for (runtime, standalone) in [
        ("nodejs", false),
        ("python", false),
        ("go", false),
        ("rust", false),
        ("dotnet", false),
        ("nodejs", true),
    ] {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join(".lampyr-platform"), "test\n").unwrap();
        let service = directory.path().join("service");
        lampyr_cli::project::initialize_application(&service, Some("service"), runtime, standalone)
            .unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_ly"))
            .arg("dev")
            .arg(&service)
            .env("PATH", directory.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(
            error.contains("local supervisor is missing"),
            "{runtime}, standalone={standalone}: {error}"
        );
        assert!(!service.join(".lampyr/dev").exists());
    }
}

#[test]
fn missing_local_bindings_fail_without_deployed_fallback() {
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .arg("dev")
        .arg(fixture().join("services/api"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no deployed fallback"));
}

#[test]
fn released_runtime_allows_bindings_to_reach_artifact_validation() {
    let missing = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args([
            "deploy",
            "--image",
            "example.invalid/worker-api:test",
            "--artifact",
        ])
        .arg(missing.path().join("missing"))
        .arg(fixture().join("services/api"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("resolve local artifact directory"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
