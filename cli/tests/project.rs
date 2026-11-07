// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use lampyr_cli::project::{self, AdapterMetadata, Project, TemplateManifest};
use std::process::Command;

#[test]
fn scaffold_creates_a_workspace_and_services_under_its_services_directory() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("workspace");
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["scaffold", "workspace"])
        .arg(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.join("lampyr.workspace.yaml").is_file());
    assert!(root.join("services").is_dir());
    assert!(root.join("infra").is_dir());
    assert!(root.join("k8s/fiberd-tenant.yaml").is_file());
    assert!(!root.join("k8s/aks-kvm-setup.yaml").exists());
    assert!(root.join("services/app/lampyr.yaml").is_file());
    let first = Project::load(root.join("services/app")).unwrap();
    let manifest = std::fs::read(root.join("lampyr.workspace.yaml")).unwrap();
    let configuration: serde_yaml_ng::Value = serde_yaml_ng::from_slice(&manifest).unwrap();
    assert_eq!(
        configuration["ingress"]["services"][0].as_str(),
        Some(first.name.as_str())
    );

    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["scaffold", "service", "api", "--runtime", "python"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let project = Project::load(root.join("services/api")).unwrap();
    assert_eq!(project.name, "api");
    assert_eq!(project.runtime, "python");
    assert_eq!(
        std::fs::read(root.join("lampyr.workspace.yaml")).unwrap(),
        manifest
    );

    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["build", "--output"])
        .arg(root.join("combined"))
        .arg(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--output cannot be used when building an entire Workspace")
    );

    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["init", "legacy"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand"));
}

#[test]
fn scaffold_service_still_supports_standalone_projects() {
    let parent = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["scaffold", "service", "standalone"])
        .current_dir(parent.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(parent.path().join("standalone/lampyr.yaml").is_file());
}

#[test]
fn workspace_services_are_ordered_and_empty_workspaces_fail_build() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("lampyr.workspace.yaml"),
        "apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\n",
    )
    .unwrap();
    std::fs::create_dir(root.path().join("services")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .arg("build")
        .arg(root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("Workspace contains no Lampyr Services")
    );

    for service in ["zeta", "alpha"] {
        let directory = root.path().join("services").join(service);
        std::fs::create_dir(&directory).unwrap();
        std::fs::write(directory.join("lampyr.yaml"), "").unwrap();
    }
    let services = lampyr_cli::customer_workspace::services(root.path()).unwrap();
    assert_eq!(
        services
            .iter()
            .map(|path| path.file_name().unwrap().to_str().unwrap())
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
}

#[test]
fn initialization_keeps_existing_application_code_and_creates_no_partial_project() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("lampyr.workspace.yaml"),
        "apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\n",
    )
    .unwrap();
    let service = root.path().join("services/example");
    std::fs::create_dir_all(&service).unwrap();
    std::fs::write(service.join("handler.js"), "customer application").unwrap();
    let error = project::initialize(&service, Some("example"), "nodejs")
        .err()
        .unwrap();
    assert!(error.to_string().contains("handler.js already exists"));
    assert!(!service.join("lampyr.yaml").exists());
    assert_eq!(
        std::fs::read_to_string(service.join("handler.js")).unwrap(),
        "customer application"
    );
}

#[test]
fn wasm_scaffolds_and_manifests_are_rejected() {
    let root = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["scaffold", "service", "unsupported", "--runtime", "wasm"])
        .current_dir(root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not supported"));
    assert!(!root.path().join("unsupported").exists());
    std::fs::write(
        root.path().join("lampyr.yaml"),
        "apiVersion: lampyr.dev/v1alpha1\nkind: Project\nname: unsupported\nruntime: wasm\napplication:\n  type: handler\nresources:\n  memory: 512Mi\n  cpu: 250m\n",
    )
    .unwrap();
    assert_eq!(
        Project::load(root.path()).err().unwrap().to_string(),
        "runtime \"wasm\" is not supported"
    );
}

#[test]
fn initialization_uses_the_canonical_template_for_each_runtime() {
    let root = tempfile::tempdir().unwrap();
    for (runtime, source) in [
        ("nodejs", "handler.js"),
        ("python", "handler.py"),
        ("go", "handler.go"),
        ("rust", "handler.rs"),
        ("dotnet", "Handler.cs"),
    ] {
        let service = root.path().join(runtime);
        project::initialize(&service, None, runtime).unwrap();
        assert!(
            service.join(source).is_file(),
            "{runtime} template missing {source}"
        );
        assert!(
            service.join("lampyr.yaml").is_file(),
            "{runtime} template missing lampyr.yaml"
        );
    }
}

#[test]
fn ownership_manifests_cover_every_source_and_scaffold_pristine_adapter_baselines() {
    let root = tempfile::tempdir().unwrap();
    for runtime in ["nodejs", "python", "go", "rust", "dotnet"] {
        let canonical = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../templates/services")
            .join(runtime);
        let manifest: TemplateManifest =
            serde_json::from_slice(&std::fs::read(canonical.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(manifest.schema_version, project::TEMPLATE_SCHEMA);
        assert_eq!(manifest.runtime, runtime);
        assert_eq!(manifest.machine_state, [".lampyr/"]);
        for required in ["lampyr.yaml", "Dockerfile", ".gitignore"] {
            assert!(manifest.user_owned.iter().any(|name| name == required));
        }
        for (ownership, declared) in [
            ("project", &manifest.user_owned),
            ("adapter", &manifest.adapter_owned),
        ] {
            let directory = canonical.join(ownership);
            let mut actual = if directory.exists() {
                std::fs::read_dir(directory)
                    .unwrap()
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };
            let mut declared = declared.clone();
            actual.sort();
            declared.sort();
            assert_eq!(
                actual, declared,
                "{runtime}/{ownership} ownership inventory"
            );
        }

        let service = root.path().join(runtime);
        project::initialize(&service, Some("custom-name"), runtime).unwrap();
        let project = Project::load(&service).unwrap();
        for filename in &manifest.user_owned {
            assert!(service.join(filename).is_file(), "{runtime}/{filename}");
        }
        let adapter = service.join(".lampyr-adapter");
        assert!(!service.join(".lampyr").exists());
        assert_eq!(
            std::fs::read_to_string(service.join(".gitignore"))
                .unwrap()
                .trim(),
            ".lampyr/"
        );
        assert!(!service.join("Dockerfile.rootfs").exists());
        assert_eq!(
            lampyr_cli::service::rootfs_dockerfile(&service).unwrap(),
            ".lampyr-adapter/Dockerfile.rootfs"
        );
        let metadata: AdapterMetadata =
            serde_json::from_slice(&std::fs::read(adapter.join("adapter.json")).unwrap()).unwrap();
        assert_eq!(metadata.schema_version, project::ADAPTER_SCHEMA);
        assert_eq!(metadata.runtime, runtime);
        assert_eq!(metadata.template_version, manifest.template_version);
        assert_eq!(metadata.entrypoint, project.command);
        assert_eq!(metadata.entrypoint, manifest.entrypoint);
        assert_eq!(metadata.snapshot_warmup, manifest.snapshot_warmup);
        assert_eq!(metadata.files.len(), manifest.adapter_owned.len());
        for filename in &manifest.adapter_owned {
            let editable = std::fs::read(adapter.join(filename)).unwrap();
            let baseline = adapter.join("baseline").join(filename);
            assert_eq!(editable, std::fs::read(&baseline).unwrap());
            let source = std::fs::read(canonical.join("adapter").join(filename)).unwrap();
            let expected = match std::str::from_utf8(&source) {
                Ok(text) => text
                    .replace(&format!("hello-{runtime}"), "custom-name")
                    .into_bytes(),
                Err(_) => source,
            };
            assert_eq!(editable, expected);
            assert_eq!(
                metadata.files[filename],
                lampyr_cli::image::file_digest(&baseline).unwrap()
            );
            std::fs::write(adapter.join(filename), "customer customization").unwrap();
            assert_eq!(editable, std::fs::read(&baseline).unwrap());
        }
    }
}

#[test]
fn snapshot_warmup_is_optional_validated_and_never_written_to_the_service_manifest() {
    let root = tempfile::tempdir().unwrap();
    project::initialize(root.path(), Some("example"), "nodejs").unwrap();
    let path = root.path().join(".lampyr-adapter/adapter.json");
    let original = std::fs::read(&path).unwrap();
    let mut metadata: serde_json::Value = serde_json::from_slice(&original).unwrap();
    for warmup in [Some("require('fastify')"), None, Some("  ")] {
        if let Some(code) = warmup {
            metadata["snapshotWarmup"] = code.into();
        } else {
            metadata.as_object_mut().unwrap().remove("snapshotWarmup");
        }
        std::fs::write(&path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        let before = lampyr_cli::image::directory_digest(root.path(), &[]).unwrap();
        let loaded = Project::load(root.path());
        if warmup == Some("  ") {
            assert!(
                format!("{:#}", loaded.err().unwrap()).contains("snapshotWarmup must not be empty")
            );
        } else {
            let serialized = serde_json::to_value(loaded.unwrap()).unwrap();
            assert!(serialized.get("snapshotWarmup").is_none());
        }
        assert_eq!(
            before,
            lampyr_cli::image::directory_digest(root.path(), &[]).unwrap()
        );
    }
}

#[test]
fn scaffolding_refuses_adapter_collisions_without_partial_writes() {
    for collision in [
        ".lampyr-adapter",
        ".lampyr-adapter/adapter.json",
        ".lampyr-adapter/server.js",
        ".lampyr-adapter/baseline/server.js",
    ] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(collision);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "customer file").unwrap();
        assert!(project::initialize(root.path(), Some("example"), "nodejs").is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "customer file");
        assert!(!root.path().join("lampyr.yaml").exists());
        assert!(!root.path().join("handler.js").exists());
    }
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".lampyr-adapter")).unwrap();
    assert!(project::initialize(root.path(), Some("example"), "nodejs").is_err());
    assert!(!root.path().join("Dockerfile").exists());
}

#[test]
fn scaffolding_refuses_service_names_too_long_to_name_a_grant() {
    assert_eq!(project::MAX_SERVICE_NAME, 43);
    let longest = "a".repeat(project::MAX_SERVICE_NAME);
    project::validate_service_name(&longest).unwrap();
    let root = tempfile::tempdir().unwrap();
    project::initialize(root.path(), Some(&longest), "nodejs").unwrap();
    let too_long = "a".repeat(project::MAX_SERVICE_NAME + 1);
    let root = tempfile::tempdir().unwrap();
    let error = project::initialize(root.path(), Some(&too_long), "nodejs").unwrap_err();
    assert!(error.to_string().contains("at most 43"), "{error:#}");
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn scaffolding_refuses_symlinked_adapter_directories_and_dangling_files() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let adapter = root.path().join(".lampyr-adapter");
    symlink(outside.path(), &adapter).unwrap();
    assert!(project::initialize(root.path(), Some("example"), "nodejs").is_err());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    std::fs::remove_file(adapter).unwrap();
    symlink("missing-handler", root.path().join("handler.js")).unwrap();
    assert!(project::initialize(root.path(), Some("example"), "nodejs").is_err());
    assert!(!root.path().join("lampyr.yaml").exists());
}

#[test]
fn rootfs_paths_preserve_legacy_services_without_masking_broken_adapters() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("Dockerfile.rootfs"), "legacy").unwrap();
    assert_eq!(
        lampyr_cli::service::rootfs_dockerfile(root.path()).unwrap(),
        "Dockerfile.rootfs"
    );
    std::fs::create_dir(root.path().join(".lampyr-adapter")).unwrap();
    assert!(lampyr_cli::service::rootfs_dockerfile(root.path()).is_err());
    std::fs::write(
        root.path().join(".lampyr-adapter/Dockerfile.rootfs"),
        "adapter",
    )
    .unwrap();
    assert_eq!(
        lampyr_cli::service::rootfs_dockerfile(root.path()).unwrap(),
        ".lampyr-adapter/Dockerfile.rootfs"
    );
}

#[test]
fn rootfs_copy_sources_resolve_from_the_service_root_not_the_adapter_directory() {
    let root = tempfile::tempdir().unwrap();
    for runtime in ["nodejs", "python", "go", "rust", "dotnet"] {
        let service = root.path().join(runtime);
        project::initialize(&service, Some("example"), runtime).unwrap();
        let dockerfile = lampyr_cli::service::rootfs_dockerfile(&service).unwrap();
        let source = std::fs::read_to_string(service.join(dockerfile)).unwrap();
        for line in source.lines().filter(|line| line.starts_with("COPY ")) {
            let words = line.split_whitespace().collect::<Vec<_>>();
            if words[1].starts_with("--from=") {
                continue;
            }
            for path in &words[1..words.len() - 1] {
                assert!(
                    service.join(path).is_file(),
                    "{runtime}: missing COPY source {path}"
                );
                assert!(
                    !path.contains("baseline"),
                    "{runtime}: baseline is not executable source"
                );
            }
        }
    }
}

#[test]
fn execution_policy_is_validated_without_starting_a_virtual_machine() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("lampyr.workspace.yaml"),
        "apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\n",
    )
    .unwrap();
    let service = root.path().join("services/example");
    project::initialize(&service, Some("example"), "python").unwrap();
    let path = service.join("lampyr.yaml");
    let source = std::fs::read_to_string(&path).unwrap();
    for invalid in [
        "mode: reuse\n  idleTimeout: 0s",
        "mode: reuse\n  idleTimeout: 1day",
        "mode: reuse\n  idleAction: park",
        "mode: fresh-per-request\n  idleTimeout: 30s",
    ] {
        std::fs::write(&path, format!("{source}\nexecution:\n  {invalid}\n")).unwrap();
        assert!(Project::load(&service).is_err(), "{invalid}");
    }
    std::fs::write(&path, format!("{source}\nexecution:\n  mode: reuse\n")).unwrap();
    let policy = Project::load(&service)
        .unwrap()
        .effective_execution()
        .unwrap();
    assert_eq!(policy.idle_timeout, "30s");
    assert_eq!(policy.idle_action, "release");
}

#[test]
fn managed_manifests_resolve_metadata_and_policy_without_rewriting_source() {
    let root = tempfile::tempdir().unwrap();
    for runtime in ["nodejs", "python", "go", "rust", "dotnet"] {
        let service = root.path().join(runtime);
        project::initialize(&service, Some("example"), runtime).unwrap();
        let path = service.join("lampyr.yaml");
        let source = std::fs::read_to_string(&path).unwrap();
        assert!(!source.contains("command:") && !source.contains("protocols:"));
        let adapter_path = service.join(".lampyr-adapter/adapter.json");
        let metadata = std::fs::read(&adapter_path).unwrap();
        let mut adapter: AdapterMetadata = serde_json::from_slice(&metadata).unwrap();
        adapter.entrypoint = "/custom-entrypoint".into();
        std::fs::write(&adapter_path, serde_json::to_vec(&adapter).unwrap()).unwrap();
        std::fs::write(service.join(".lampyr-adapter/server.custom"), "custom").unwrap();
        let before = lampyr_cli::image::directory_digest(&service, &[]).unwrap();
        let project = Project::load(&service).unwrap();
        assert_eq!(project.command, "/custom-entrypoint");
        assert!(project.protocols.events);
        assert!(
            project.protocols.http && project.protocols.streaming && project.protocols.websocket
        );
        assert_eq!(
            before,
            lampyr_cli::image::directory_digest(&service, &[]).unwrap()
        );
        let start = source.find("events:").unwrap();
        let end = source.find("resources:").unwrap();
        let without_policy = format!("{}{}", &source[..start], &source[end..]);
        std::fs::write(&path, &without_policy).unwrap();
        let project = Project::load(&service).unwrap();
        assert!(!project.protocols.events);
        let effective = serde_json::to_vec(&project).unwrap();
        let artifact_project: Project = serde_json::from_slice(&effective).unwrap();
        artifact_project.validate().unwrap();
        assert_eq!(artifact_project.command, "/custom-entrypoint");
        assert!(!artifact_project.protocols.events);
        std::fs::write(
            &path,
            format!("{without_policy}\nprotocols:\n  events: true\n"),
        )
        .unwrap();
        assert!(
            Project::load(&service)
                .err()
                .unwrap()
                .to_string()
                .contains("must omit top-level command and protocols")
        );
        std::fs::write(&path, without_policy).unwrap();
        adapter.runtime = "wasm".into();
        std::fs::write(&adapter_path, serde_json::to_vec(&adapter).unwrap()).unwrap();
        assert!(Project::load(&service).is_err());
        std::fs::remove_file(adapter_path).unwrap();
        assert!(Project::load(&service).is_err());
    }
}

#[test]
fn native_applications_require_an_explicit_application_model() {
    let root = tempfile::tempdir().unwrap();
    for runtime in ["nodejs", "python", "go", "rust", "dotnet"] {
        let service = root.path().join(runtime);
        project::initialize(&service, Some("example"), runtime).unwrap();
        let path = service.join("lampyr.yaml");
        let source = std::fs::read_to_string(&path).unwrap();
        let legacy = source.replace("application:\n    type: handler\n", "");
        std::fs::write(&path, &legacy).unwrap();
        assert!(
            Project::load(&service)
                .err()
                .unwrap()
                .to_string()
                .contains("application.type must be handler or standalone"),
            "{runtime}"
        );
        let explicit_legacy_command =
            format!("{}\ncommand: /legacy\nprotocols:\n  http: true\n", legacy);
        std::fs::write(&path, explicit_legacy_command).unwrap();
        assert!(Project::load(&service).is_err(), "{runtime}");
    }
}

#[test]
fn absent_event_policy_rejects_provider_binding_before_contacting_cluster() {
    let root = tempfile::tempdir().unwrap();
    project::initialize(root.path(), Some("example"), "nodejs").unwrap();
    let path = root.path().join("lampyr.yaml");
    let source = std::fs::read_to_string(&path).unwrap();
    let start = source.find("events:").unwrap();
    let end = source.find("resources:").unwrap();
    std::fs::write(path, format!("{}{}", &source[..start], &source[end..])).unwrap();
    let error = lampyr_cli::event_source::run(
        lampyr_cli::artifact::Operation::SourceCreate,
        "kind",
        root.path(),
        None,
        "default",
        "example-source",
        &lampyr_cli::host::Host {
            verbose: false,
            cancellation: Default::default(),
        },
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("events are not enabled"));
}
