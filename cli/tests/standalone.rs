// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::process::Command;

use lampyr_cli::project::{self, Application, Project, TemplateManifest};

const RUNTIMES: [&str; 5] = ["nodejs", "python", "go", "rust", "dotnet"];

#[test]
fn standalone_scaffolds_are_entirely_customer_owned_and_never_upgraded() {
    let root = tempfile::tempdir().unwrap();
    for runtime in RUNTIMES {
        let output = Command::new(env!("CARGO_BIN_EXE_ly"))
            .args([
                "scaffold",
                "service",
                runtime,
                "--runtime",
                runtime,
                "--standalone",
            ])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let service = root.path().join(runtime);
        let project = Project::load(&service).unwrap();
        assert!(matches!(
            project.application,
            Some(Application::Standalone { .. })
        ));
        assert!(
            project.protocols.http && project.protocols.streaming && project.protocols.websocket
        );
        assert!(!project.protocols.events && !project.protocols.grpc);
        assert!(!service.join(".lampyr-adapter").exists());
        assert_eq!(
            lampyr_cli::service::rootfs_dockerfile(&service).unwrap(),
            "Dockerfile.rootfs"
        );
        let canonical = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../templates/services")
            .join(runtime)
            .join("standalone");
        let manifest: TemplateManifest =
            serde_json::from_slice(&std::fs::read(canonical.join("manifest.json")).unwrap())
                .unwrap();
        assert!(manifest.adapter_owned.is_empty());
        assert_eq!(project.command, manifest.entrypoint);
        for path in &manifest.user_owned {
            let source = std::fs::read(canonical.join("project").join(path)).unwrap();
            let expected = String::from_utf8(source)
                .unwrap()
                .replace(&format!("hello-{runtime}"), runtime);
            assert_eq!(
                std::fs::read_to_string(service.join(path)).unwrap(),
                expected
            );
        }
        std::fs::write(service.join("Dockerfile.rootfs"), "customer rootfs").unwrap();
        let before = lampyr_cli::image::directory_digest(&service, &[]).unwrap();
        lampyr_cli::adapter::diagnose(&service, &project).unwrap();
        assert_eq!(project.application_model(), "standalone");
        let output = Command::new(env!("CARGO_BIN_EXE_ly"))
            .args(["scaffold", "service", runtime, "--upgrade"])
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("customer-owned"));
        assert_eq!(
            before,
            lampyr_cli::image::directory_digest(&service, &[]).unwrap()
        );
        assert!(project::initialize_application(&service, Some(runtime), runtime, true).is_err());
        assert_eq!(
            before,
            lampyr_cli::image::directory_digest(&service, &[]).unwrap()
        );
    }
}

#[test]
fn standalone_uses_workspace_name_resolution_and_rejects_wasm_and_upgrade_flags() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("lampyr.workspace.yaml"),
        "apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args([
            "scaffold",
            "service",
            "api",
            "--runtime",
            "go",
            "--standalone",
        ])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    Project::load(root.path().join("services/api")).unwrap();
    for flags in [
        vec!["--runtime", "wasm", "--standalone"],
        vec!["--standalone", "--upgrade"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ly"))
            .args(["scaffold", "service", "bad"])
            .args(&flags)
            .current_dir(root.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        if flags.contains(&"--upgrade") {
            assert!(String::from_utf8_lossy(&output.stderr).contains("cannot be used with"));
        }
        assert!(!root.path().join("services/bad").exists());
    }
}

#[test]
fn explicit_models_reject_ambiguous_commands_capabilities_and_adapters() {
    let root = tempfile::tempdir().unwrap();
    project::initialize_application(root.path(), Some("example"), "nodejs", true).unwrap();
    let path = root.path().join("lampyr.yaml");
    let source = std::fs::read_to_string(&path).unwrap();
    for invalid in [
        format!("{source}\ncommand: /legacy\n"),
        format!("{source}\nprotocols: {{http: true}}\n"),
        source.replace("/app/server.js", "''"),
        source.replace("/app/server.js", "'   '"),
        source.replace("/app/server.js", "' /app/server.js '"),
        source.replace("type: standalone", "type: unknown"),
        source.replace("type: standalone", "type: handler"),
        source.replace("runtime: nodejs", "runtime: wasm"),
        source.replace("    command: /app/server.js\n", ""),
        source.replace("type: standalone", "type: standalone\n    adapter: custom"),
        source.replace(
            "application:\n    type: standalone\n    command: /app/server.js",
            "application: null",
        ),
    ] {
        std::fs::write(&path, invalid).unwrap();
        assert!(Project::load(root.path()).is_err());
    }
    std::fs::write(&path, &source).unwrap();
    for adapter_is_directory in [false, true] {
        let adapter = root.path().join(".lampyr-adapter");
        if adapter_is_directory {
            std::fs::create_dir(&adapter).unwrap();
        } else {
            std::fs::write(&adapter, "not an adapter").unwrap();
        }
        assert!(
            Project::load(root.path())
                .err()
                .unwrap()
                .to_string()
                .contains("must not contain")
        );
        if adapter_is_directory {
            std::fs::remove_dir(adapter).unwrap();
        } else {
            std::fs::remove_file(adapter).unwrap();
        }
    }
    let handler = root.path().join("handler");
    project::initialize(&handler, Some("handler"), "nodejs").unwrap();
    let manifest = handler.join("lampyr.yaml");
    let source = std::fs::read_to_string(&manifest).unwrap();
    for extra in ["command: /customer", "protocols: {}", "command: ''"] {
        std::fs::write(&manifest, format!("{source}\n{extra}\n")).unwrap();
        assert!(Project::load(&handler).is_err());
    }
    std::fs::write(&manifest, source).unwrap();
    let source = std::fs::read_to_string(&manifest).unwrap();
    for extra in ["command: /customer", "unexpected: true"] {
        std::fs::write(
            &manifest,
            source.replace("type: handler", &format!("type: handler\n    {extra}")),
        )
        .unwrap();
        assert!(Project::load(&handler).is_err());
    }
    std::fs::write(&manifest, source).unwrap();
    std::fs::remove_file(handler.join(".lampyr-adapter/adapter.json")).unwrap();
    assert!(Project::load(&handler).is_err());
}

#[test]
fn standalone_event_policy_and_effective_command_survive_artifact_roundtrip() {
    let root = tempfile::tempdir().unwrap();
    project::initialize_application(root.path(), Some("example"), "python", true).unwrap();
    let path = root.path().join("lampyr.yaml");
    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, format!("{source}\nevents:\n  maxRetries: 3\n  initialBackoff: 100ms\n  attemptTimeout: 30s\n  deadLetter: .lampyr/dead-letter.jsonl\n")).unwrap();
    let project = Project::load(root.path()).unwrap();
    assert!(project.protocols.events);
    let encoded = serde_json::to_value(&project).unwrap();
    assert_eq!(encoded["application"]["type"], "standalone");
    assert_eq!(encoded["application"]["command"], project.command);
    let effective: Project = serde_json::from_value(encoded.clone()).unwrap();
    effective.validate().unwrap();
    assert!(effective.protocols.events);
    let mut invalid = encoded;
    invalid["command"] = "/wrong".into();
    assert!(
        serde_json::from_value::<Project>(invalid)
            .unwrap()
            .validate()
            .is_err()
    );
    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, source.replace("maxRetries: 3", "maxRetries: 11")).unwrap();
    assert!(Project::load(root.path()).is_err());
}

#[test]
fn doctor_identifies_the_application_without_starting_a_server() {
    let root = tempfile::tempdir().unwrap();
    project::initialize_application(root.path(), Some("example"), "nodejs", true).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .arg("doctor")
        .current_dir(root.path())
        .env("PATH", "")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("standalone (/app/server.js)"));
    std::fs::write(root.path().join("lampyr.yaml"), "application: null").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .arg("doctor")
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout)
            .contains("application.type must be handler or standalone")
    );
}

#[cfg(unix)]
#[test]
fn standalone_rejects_dangling_adapter_symlinks_without_writes() {
    let root = tempfile::tempdir().unwrap();
    let adapter = root.path().join(".lampyr-adapter");
    std::os::unix::fs::symlink("missing-adapter", &adapter).unwrap();
    assert!(project::initialize_application(root.path(), Some("example"), "nodejs", true).is_err());
    assert!(!root.path().join("lampyr.yaml").exists());
    std::fs::remove_file(&adapter).unwrap();
    project::initialize_application(root.path(), Some("example"), "nodejs", true).unwrap();
    std::os::unix::fs::symlink("missing-adapter", &adapter).unwrap();
    assert!(Project::load(root.path()).is_err());
}
