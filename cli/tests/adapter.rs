// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Output};

use lampyr_cli::project::{self, AdapterMetadata, Project};

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ly"))
        .current_dir(root)
        .args(args)
        // Upgrade commands must work without any language, Docker, platform or VM.
        .env("PATH", "")
        .env("LAMPYR_HOME", root.join("no-platform"))
        .output()
        .unwrap()
}

fn success(output: Output) {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
    fn walk(root: &Path, at: &Path, files: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(at).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let key = path
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned();
            if entry.file_type().unwrap().is_dir() {
                files.insert(format!("{key}/"), Vec::new());
                walk(root, &path, files);
            } else {
                files.insert(key, std::fs::read(path).unwrap());
            }
        }
    }
    let mut files = BTreeMap::new();
    walk(root, root, &mut files);
    files
}

fn old_baseline(service: &Path, path: &str, base: &[u8], ours: &[u8]) {
    let adapter = service.join(".lampyr-adapter");
    let baseline = adapter.join("baseline").join(path);
    std::fs::write(&baseline, base).unwrap();
    std::fs::write(adapter.join(path), ours).unwrap();
    let path = adapter.join("adapter.json");
    let mut metadata: AdapterMetadata =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    metadata.template_version = "0.0.1".into();
    metadata.files.insert(
        baseline.file_name().unwrap().to_str().unwrap().into(),
        lampyr_cli::image::file_digest(&baseline).unwrap(),
    );
    std::fs::write(path, serde_json::to_vec_pretty(&metadata).unwrap()).unwrap();
}

#[test]
fn upgrades_resolve_standalone_and_workspace_names_using_existing_runtime() {
    let parent = tempfile::tempdir().unwrap();
    for (runtime, source) in [
        ("nodejs", "server.js"),
        ("python", "server.py"),
        ("go", "server.go"),
        ("rust", "server.rs"),
        ("dotnet", "Program.cs"),
    ] {
        let service = parent.path().join(runtime);
        project::initialize(&service, Some(runtime), runtime).unwrap();
        let adapter = service.join(".lampyr-adapter");
        let expected = std::fs::read(adapter.join(source)).unwrap();
        old_baseline(&service, source, b"old template\n", b"old template\n");
        success(cli(
            parent.path(),
            &["scaffold", "service", runtime, "--upgrade"],
        ));
        assert_eq!(Project::load(&service).unwrap().runtime, runtime);
        assert_eq!(std::fs::read(adapter.join(source)).unwrap(), expected);
        assert_eq!(
            std::fs::read(adapter.join("baseline").join(source)).unwrap(),
            expected
        );
    }
    let workspace = parent.path().join("workspace");
    success(cli(
        parent.path(),
        &["scaffold", "workspace", workspace.to_str().unwrap()],
    ));
    success(cli(
        &workspace,
        &["scaffold", "service", "api", "--runtime", "python"],
    ));
    let service = workspace.join("services/api");
    old_baseline(&service, "server.py", b"old template\n", b"old template\n");
    let nested = workspace.join("infra/nested");
    std::fs::create_dir(&nested).unwrap();
    success(cli(&nested, &["scaffold", "service", "api", "--upgrade"]));
    assert_eq!(Project::load(service).unwrap().runtime, "python");
}

#[test]
fn cli_conflict_preview_is_colored_nonzero_and_byte_identical_until_opted_in() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("api");
    project::initialize(&service, Some("api"), "python").unwrap();
    old_baseline(&service, "server.py", b"old\n", b"customer\n");
    let before = tree(&service);
    let digest_before = lampyr_cli::image::directory_digest(&service, &[]).unwrap();
    let output = cli(
        parent.path(),
        &[
            "scaffold",
            "service",
            "api",
            "--upgrade",
            "--color",
            "always",
        ],
    );
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("\x1b[31m-customer"));
    assert!(stderr.contains("\x1b[32m+"));
    assert!(stderr.contains("nothing written"));
    assert_eq!(tree(&service), before);
    assert_eq!(
        lampyr_cli::image::directory_digest(&service, &[]).unwrap(),
        digest_before
    );
    let output = cli(
        parent.path(),
        &[
            "scaffold",
            "service",
            "api",
            "--upgrade",
            "--color",
            "never",
        ],
    );
    assert!(!output.status.success());
    assert!(!output.stderr.contains(&0x1b));
    assert_eq!(tree(&service), before);
    let output = cli(
        parent.path(),
        &[
            "scaffold",
            "service",
            "api",
            "--upgrade",
            "--write-conflicts",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("conflicts written"));
    let written = tree(&service);
    assert_ne!(written, before);
    for path in [
        "handler.py",
        "lampyr.yaml",
        "Dockerfile",
        ".gitignore",
        ".lampyr-adapter/adapter.json",
        ".lampyr-adapter/baseline/server.py",
    ] {
        assert_eq!(written[path], before[path], "{path}");
    }
    assert_ne!(
        lampyr_cli::image::directory_digest(&service, &[]).unwrap(),
        digest_before
    );
    let project = Project::load(&service).unwrap();
    assert!(
        lampyr_cli::adapter::diagnose(&service, &project)
            .unwrap_err()
            .to_string()
            .contains("upgrade is pending")
    );
    let output = cli(parent.path(), &["scaffold", "service", "api", "--upgrade"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unresolved conflict markers"));
    assert_eq!(tree(&service), written);
    std::fs::write(
        service.join(".lampyr-adapter/server.py"),
        "customer resolved\n\"\"\"\n=======\n\"\"\"\n",
    )
    .unwrap();
    std::fs::write(
        service.join(".lampyr-adapter/README.md"),
        "Notes\n=======\n",
    )
    .unwrap();
    success(cli(
        parent.path(),
        &["scaffold", "service", "api", "--upgrade"],
    ));
    assert_eq!(
        std::fs::read(service.join(".lampyr-adapter/server.py")).unwrap(),
        b"customer resolved\n\"\"\"\n=======\n\"\"\"\n"
    );
    assert_eq!(
        std::fs::read(service.join(".lampyr-adapter/README.md")).unwrap(),
        b"Notes\n=======\n"
    );
    assert!(!service.join(".lampyr-adapter/pending.json").exists());
    assert!(!service.join(".lampyr-adapter/pending-baseline").exists());
    assert_eq!(Project::load(&service).unwrap().runtime, "python");
}

#[test]
fn cli_clean_merge_keeps_customer_changes_and_user_owned_bytes() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("api");
    project::initialize(&service, Some("api"), "python").unwrap();
    let server = service.join(".lampyr-adapter/server.py");
    let target = std::fs::read(&server).unwrap();
    let mut base = b"# removed in installed version\n".to_vec();
    base.extend_from_slice(&target);
    let mut ours = base.clone();
    ours.extend_from_slice(b"\n# customer change\n");
    old_baseline(&service, "server.py", &base, &ours);
    std::fs::write(service.join("handler.py"), "customer handler\n").unwrap();
    std::fs::write(service.join("Dockerfile"), "customer final packaging\n").unwrap();
    let before = tree(&service);
    success(cli(
        parent.path(),
        &["scaffold", "service", "api", "--upgrade"],
    ));
    let mut expected = target.clone();
    expected.extend_from_slice(b"\n# customer change\n");
    assert_eq!(std::fs::read(&server).unwrap(), expected);
    for path in ["handler.py", "lampyr.yaml", "Dockerfile", ".gitignore"] {
        assert_eq!(std::fs::read(service.join(path)).unwrap(), before[path]);
    }
    assert_eq!(
        std::fs::read(service.join(".lampyr-adapter/baseline/server.py")).unwrap(),
        target
    );
}

#[test]
fn modified_and_outdated_adapters_warn_before_build_dev_tools_without_rewrites() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("api");
    project::initialize(&service, Some("api"), "python").unwrap();
    old_baseline(&service, "server.py", b"old\n", b"customized\n");
    let before = tree(&service);
    for args in [
        vec!["build", "api"],
        vec!["dev", "api"],
        vec!["deploy", "api", "--image", "registry.example/service:test"],
    ] {
        let output = cli(parent.path(), &args);
        assert!(!output.status.success()); // Deliberately no python or Docker in PATH.
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("runtime adapter is modified"), "{stderr}");
        assert!(stderr.contains("runtime adapter is outdated"), "{stderr}");
        assert!(stderr.contains("best-effort support"), "{stderr}");
        assert!(!stderr.contains("baseline was modified"), "{stderr}");
        let sources = tree(&service)
            .into_iter()
            .filter(|(path, _)| !path.starts_with(".lampyr/"))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(sources, before);
    }
    // Hash differences in editable files are never a metadata-load error.
    Project::load(&service).unwrap();
}

#[test]
fn same_version_template_drift_does_not_suggest_an_upgrade() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("api");
    project::initialize(&service, Some("api"), "python").unwrap();
    let path = service.join(".lampyr-adapter/adapter.json");
    let installed: AdapterMetadata =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    // Simulate a service scaffolded by another ly build with the same version.
    old_baseline(&service, "server.py", b"other build\n", b"other build\n");
    let mut metadata: AdapterMetadata =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    metadata.template_version = installed.template_version.clone();
    std::fs::write(&path, serde_json::to_vec_pretty(&metadata).unwrap()).unwrap();
    let output = cli(parent.path(), &["build", "api"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("at the same version"), "{stderr}");
    assert!(!stderr.contains("run ly scaffold service"), "{stderr}");
}

#[test]
fn snapshot_warmup_metadata_drift_warns_without_rewriting_adapter() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("api");
    project::initialize(&service, Some("api"), "nodejs").unwrap();
    let metadata_path = service.join(".lampyr-adapter/adapter.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
    metadata.as_object_mut().unwrap().remove("snapshotWarmup");
    std::fs::write(
        &metadata_path,
        serde_json::to_vec_pretty(&metadata).unwrap(),
    )
    .unwrap();
    let before = tree(&service);
    let output = cli(parent.path(), &["build", "api"]);
    assert!(!output.status.success()); // Deliberately no Node SDK or Docker in PATH.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("runtime adapter is modified"), "{stderr}");
    // A customer edit of warmup is a customization, not an outdated template.
    assert!(!stderr.contains("runtime adapter is outdated"), "{stderr}");
    assert_eq!(tree(&service), before);
}

#[test]
fn corrupt_pending_and_baseline_metadata_are_explicit_errors_not_fallbacks() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("api");
    project::initialize(&service, Some("api"), "nodejs").unwrap();
    let baseline = service.join(".lampyr-adapter/baseline/server.js");
    std::fs::write(&baseline, "tampered\n").unwrap();
    let before = tree(&service);
    let output = cli(parent.path(), &["scaffold", "service", "api", "--upgrade"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("baseline was modified"));
    assert_eq!(tree(&service), before);
    std::fs::copy(service.join(".lampyr-adapter/server.js"), &baseline).unwrap();
    std::fs::write(service.join(".lampyr-adapter/pending.json"), "{}").unwrap();
    let before = tree(&service);
    for args in [
        vec!["scaffold", "service", "api", "--upgrade"],
        vec!["build", "api"],
        vec!["dev", "api"],
        vec!["deploy", "api", "--image", "registry.example/service:test"],
    ] {
        let output = cli(parent.path(), &args);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("pending.json"));
        assert_eq!(tree(&service), before);
    }
}

#[test]
fn unsupported_manifests_and_unsafe_flags_fail_without_writes() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("legacy");
    project::initialize(&service, Some("legacy"), "nodejs").unwrap();
    let path = service.join("lampyr.yaml");
    let source = std::fs::read_to_string(&path).unwrap();
    std::fs::write(&path, source.replace("runtime: nodejs", "runtime: wasm")).unwrap();
    let before = tree(&service);
    let output = cli(
        parent.path(),
        &["scaffold", "service", "legacy", "--upgrade"],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("runtime \"wasm\" is not supported"));
    assert_eq!(tree(&service), before);
    for flags in [
        vec!["--write-conflicts"],
        vec!["--upgrade", "--runtime", "python"],
    ] {
        let mut args = vec!["scaffold", "service", "legacy"];
        args.extend(flags);
        assert!(!cli(parent.path(), &args).status.success());
        assert_eq!(tree(&service), before);
    }
}
#[test]
fn cli_upgrades_added_and_deleted_files_and_preserves_customer_deletion() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("api");
    project::initialize(&service, Some("api"), "python").unwrap();
    let adapter = service.join(".lampyr-adapter");
    let metadata_path = adapter.join("adapter.json");
    let mut metadata: AdapterMetadata =
        serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
    let expected_addition = std::fs::read(adapter.join("lampyr_types.py")).unwrap();
    for path in [
        "lampyr_types.py",
        "baseline/lampyr_types.py",
        "Dockerfile.rootfs",
    ] {
        std::fs::remove_file(adapter.join(path)).unwrap();
    }
    metadata.files.remove("lampyr_types.py");
    for path in ["retired.txt", "baseline/retired.txt"] {
        std::fs::write(adapter.join(path), b"retired template\n").unwrap();
    }
    metadata.files.insert(
        "retired.txt".into(),
        lampyr_cli::image::file_digest(&adapter.join("baseline/retired.txt")).unwrap(),
    );
    metadata.template_version = "0.0.1".into();
    std::fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    success(cli(
        parent.path(),
        &["scaffold", "service", "api", "--upgrade"],
    ));
    assert_eq!(
        std::fs::read(adapter.join("lampyr_types.py")).unwrap(),
        expected_addition
    );
    assert!(!adapter.join("retired.txt").exists());
    assert!(!adapter.join("baseline/retired.txt").exists());
    assert!(!adapter.join("Dockerfile.rootfs").exists());
    assert!(adapter.join("baseline/Dockerfile.rootfs").is_file());
    Project::load(&service).unwrap();
}

#[test]
fn cli_finalizes_pinned_target_bytes_instead_of_the_installed_template() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("api");
    project::initialize(&service, Some("api"), "python").unwrap();
    old_baseline(&service, "server.py", b"old\n", b"customer\n");
    assert!(
        !cli(
            parent.path(),
            &[
                "scaffold",
                "service",
                "api",
                "--upgrade",
                "--write-conflicts"
            ]
        )
        .status
        .success()
    );
    let adapter = service.join(".lampyr-adapter");
    // Fixture for pending state written by another installed CLI version.
    let path = adapter.join("pending.json");
    let mut pending: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let pinned = adapter.join("pending-baseline/server.py");
    std::fs::write(&pinned, b"pinned version's pristine server\n").unwrap();
    let before = tree(&service);
    let output = cli(parent.path(), &["scaffold", "service", "api", "--upgrade"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("baseline was modified"));
    assert_eq!(tree(&service), before);
    pending["target"]["templateVersion"] = "0.2.0".into();
    pending["target"]["files"]["server.py"] =
        lampyr_cli::image::file_digest(&pinned).unwrap().into();
    std::fs::write(path, serde_json::to_vec(&pending).unwrap()).unwrap();
    std::fs::write(adapter.join("server.py"), b"chosen resolution\n").unwrap();
    let output = cli(parent.path(), &["scaffold", "service", "api", "--upgrade"]);
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("finalizing pinned adapter target 0.2.0")
    );
    success(output);
    let metadata: AdapterMetadata =
        serde_json::from_slice(&std::fs::read(adapter.join("adapter.json")).unwrap()).unwrap();
    assert_eq!(metadata.template_version, "0.2.0");
    assert_eq!(
        std::fs::read(adapter.join("baseline/server.py")).unwrap(),
        b"pinned version's pristine server\n"
    );
    assert_eq!(
        std::fs::read(adapter.join("server.py")).unwrap(),
        b"chosen resolution\n"
    );
}

#[test]
fn cli_rejects_directory_collisions_traversal_and_symlinks_without_touching_source() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("api");
    project::initialize(&service, Some("api"), "nodejs").unwrap();
    let adapter = service.join(".lampyr-adapter");
    let metadata_path = adapter.join("adapter.json");
    let mut metadata: AdapterMetadata =
        serde_json::from_slice(&std::fs::read(&metadata_path).unwrap()).unwrap();
    std::fs::remove_file(adapter.join("types.js")).unwrap();
    std::fs::remove_file(adapter.join("baseline/types.js")).unwrap();
    metadata.files.remove("types.js");
    metadata.template_version = "0.0.1".into();
    std::fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    std::fs::create_dir(adapter.join("types.js")).unwrap();
    let before = tree(&service);
    let output = cli(parent.path(), &["scaffold", "service", "api", "--upgrade"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("collides with existing directory"));
    assert_eq!(tree(&service), before);
    metadata
        .files
        .insert("../outside".into(), format!("sha256:{}", "a".repeat(64)));
    std::fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let before = tree(&service);
    let output = cli(parent.path(), &["scaffold", "service", "api", "--upgrade"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("unsafe adapter path"));
    assert_eq!(tree(&service), before);
    metadata.files.remove("../outside");
    std::fs::write(&metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
    let external = parent.path().join("external");
    std::fs::write(&external, b"untouched outside bytes").unwrap();
    std::os::unix::fs::symlink(&external, adapter.join("linked")).unwrap();
    let digest = lampyr_cli::image::directory_digest(&service, &[]).unwrap();
    let output = cli(
        parent.path(),
        &[
            "scaffold",
            "service",
            "api",
            "--upgrade",
            "--write-conflicts",
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("read adapter path linked"));
    assert_eq!(
        lampyr_cli::image::directory_digest(&service, &[]).unwrap(),
        digest
    );
    assert_eq!(std::fs::read(external).unwrap(), b"untouched outside bytes");
}

#[test]
fn metadata_load_accepts_parent_relative_service_paths() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("api");
    project::initialize(&service, Some("api"), "python").unwrap();
    let nested = parent.path().join("nested");
    std::fs::create_dir(&nested).unwrap();
    let relative = nested.join("../api");
    assert_eq!(Project::load(relative).unwrap().name, "api");
}

#[test]
fn metadata_load_needs_only_search_permission_on_ancestors() {
    use std::os::unix::fs::PermissionsExt;
    let root = tempfile::tempdir().unwrap();
    let locked = root.path().join("locked");
    let service = locked.join("api");
    project::initialize(&service, Some("api"), "python").unwrap();
    // Enterable but not listable, like a 0711 home directory.
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o311)).unwrap();
    let loaded = Project::load(&service);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(loaded.unwrap().name, "api");
}

#[test]
fn local_tooling_output_in_the_adapter_never_blocks_loads_or_drift_checks() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("api");
    project::initialize(&service, Some("api"), "python").unwrap();
    let adapter = service.join(".lampyr-adapter");
    // What editors and language tooling leave behind next to the vendored sources.
    std::os::unix::fs::symlink("server.py", adapter.join(".#server.py")).unwrap();
    std::fs::create_dir_all(adapter.join("target/release")).unwrap();
    std::fs::write(
        adapter.join("target/release/lampyr-service"),
        vec![0u8; 5 << 20],
    )
    .unwrap();
    std::fs::write(adapter.join("target/release/a"), b"x").unwrap();
    std::fs::hard_link(
        adapter.join("target/release/a"),
        adapter.join("target/release/b"),
    )
    .unwrap();
    std::fs::create_dir_all(adapter.join("__pycache__")).unwrap();
    std::fs::write(adapter.join("__pycache__/server.cpython-312.pyc"), b"cache").unwrap();
    Project::load(&service).unwrap();
    let output = cli(parent.path(), &["build", "api"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("runtime adapter metadata"), "{stderr}");
    assert!(!stderr.contains("runtime adapter is modified"), "{stderr}");
}

#[test]
fn upgrade_accepts_the_service_directory_printed_by_build() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("My Api's");
    project::initialize(&service, Some("svc"), "python").unwrap();
    old_baseline(&service, "server.py", b"old\n", b"old\n");
    let output = cli(parent.path(), &["build", "My Api's"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let printed = stderr
        .split("run ly scaffold service ")
        .nth(1)
        .and_then(|rest| rest.split(" --upgrade").next())
        .unwrap_or_else(|| panic!("no upgrade hint: {stderr}"));
    // Parse the printed path the way a shell would when the hint is pasted.
    let parsed = Command::new("sh")
        .args(["-c", &format!("printf '%s' {printed}")])
        .output()
        .unwrap();
    assert!(parsed.status.success(), "{printed}");
    let hint = String::from_utf8(parsed.stdout).unwrap();
    assert_eq!(Path::new(&hint), service.canonicalize().unwrap());
    success(cli(
        parent.path(),
        &["scaffold", "service", &hint, "--upgrade"],
    ));
    success(cli(
        parent.path(),
        &["scaffold", "service", "./My Api's", "--upgrade"],
    ));
    let missing = cli(
        parent.path(),
        &["scaffold", "service", "./absent", "--upgrade"],
    );
    assert!(!missing.status.success());
    assert!(String::from_utf8_lossy(&missing.stderr).contains("absent"));
}

#[test]
fn legacy_metadata_edits_are_reported_as_modified_not_outdated() {
    let parent = tempfile::tempdir().unwrap();
    let service = parent.path().join("api");
    project::initialize(&service, Some("api"), "python").unwrap();
    let path = service.join(".lampyr-adapter/adapter.json");
    let mut metadata: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    let object = metadata.as_object_mut().unwrap();
    object.remove("pristine");
    object.insert("entrypoint".into(), "/custom.py".into());
    std::fs::write(&path, serde_json::to_vec_pretty(&metadata).unwrap()).unwrap();
    let output = cli(parent.path(), &["build", "api"]);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("runtime adapter is modified"), "{stderr}");
    assert!(!stderr.contains("runtime adapter is outdated"), "{stderr}");
}
