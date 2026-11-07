// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

#![cfg(unix)]

use base64::Engine;
use serde_json::json;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

fn initialize_workspace(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("k8s")).unwrap();
    std::fs::write(root.join(".lampyr-platform"), "test").unwrap();
    std::fs::write(
        root.join("lampyr.workspace.yaml"),
        "apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\n",
    )
    .unwrap();
    std::fs::write(root.join("k8s/fiberd-tenant.yaml"), "").unwrap();
}

/// Controller inputs an install takes before it runs the provider, their
/// manifest written beside the configuration that names it.
fn controller_inputs(directory: &std::path::Path) -> String {
    std::fs::write(directory.join("controller.yaml"), "").unwrap();
    format!(
        "controllerManifest: controller.yaml\n\
         grantImage: registry.example/fiberd@sha256:{}\n\
         helperImage: registry.example/helper@sha256:{}\n",
        "0".repeat(64),
        "1".repeat(64)
    )
}

#[test]
fn provider_discovery_never_builds_missing_or_invalid_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("platform");
    let project = root.join("providers/third-party");
    let tools = directory.path().join("tools");
    initialize_workspace(&root);
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::write(project.join("Justfile"), "build:\n    exit 1\n").unwrap();
    let just = tools.join("just");
    std::fs::write(&just, "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$BUILD_LOG\"\necho 'fixture builder stopped' >&2\nexit 71\n").unwrap();
    std::fs::set_permissions(&just, std::fs::Permissions::from_mode(0o700)).unwrap();
    let inputs = directory.path().join("inputs.yaml");
    std::fs::write(
        &inputs,
        format!(
            "{}zygoteRegistries: [registry.example/templates]\n",
            controller_inputs(directory.path())
        ),
    )
    .unwrap();
    for case in ["missing", "explicit", "invalid", "packaged", "no-inputs"] {
        let log = directory.path().join(format!("{case}.log"));
        if case == "invalid" {
            std::fs::create_dir_all(project.join("build/rootfs")).unwrap();
            std::fs::write(project.join("build/rootfs/manifest.json"), "{}").unwrap();
        }
        if case == "packaged" {
            std::fs::create_dir_all(project.join("artifact")).unwrap();
            std::fs::write(project.join("artifact/manifest.json"), "{}").unwrap();
        }
        let mut command = Command::new(env!("CARGO_BIN_EXE_ly"));
        command
            .args(["cluster", "install", "--provider", "third-party", "--name"])
            .arg(format!("provider-{case}"))
            .arg("--workspace")
            .arg(&root)
            .env("PATH", &tools)
            .env("BUILD_LOG", &log)
            .env("LAMPYR_HOME", directory.path().join("data"))
            .current_dir(&root);
        if case != "no-inputs" {
            command.arg("--provider-config").arg(&inputs);
        }
        if case == "explicit" {
            command
                .arg("--provider-artifact")
                .arg(directory.path().join("missing-explicit"));
        }
        let output = command.output().unwrap();
        assert!(!output.status.success());
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!log.exists(), "{case} unexpectedly ran a provider build");
        assert!(
            error.contains(match case {
                "missing" => "could not locate third-party provider artifact",
                "explicit" => "resolve provider artifact",
                // A first install without the controller inputs stops before
                // it loads the packaged provider, let alone runs it.
                "no-inputs" => "controllerManifest is required",
                _ => "decode provider artifact manifest",
            }),
            "{error}"
        );
    }
}

#[test]
fn an_aks_install_without_zygote_registries_stops_before_the_provider_runs() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("platform");
    initialize_workspace(&root);
    // A provider that ran would fail to decode this manifest.
    let artifact = root.join("providers/azure/artifact");
    std::fs::create_dir_all(&artifact).unwrap();
    std::fs::write(artifact.join("manifest.json"), "{}").unwrap();
    let inputs = directory.path().join("inputs.yaml");
    std::fs::write(&inputs, controller_inputs(directory.path())).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["cluster", "install", "--provider", "azure", "--workspace"])
        .arg(&root)
        .arg("--provider-config")
        .arg(&inputs)
        .env("PATH", directory.path().join("no-tools"))
        .env("LAMPYR_HOME", directory.path().join("data"))
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("zygoteRegistries") && error.contains("anonymous pulls"),
        "{error}"
    );
}

#[test]
fn workspace_option_rejects_an_infrastructure_source_directory() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("workspace");
    initialize_workspace(&root);
    let infrastructure = root.join("infra/kind/example");
    std::fs::create_dir_all(&infrastructure).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["cluster", "health", "--provider", "kind", "--workspace"])
        .arg(&infrastructure)
        .env("LAMPYR_HOME", directory.path().join("data"))
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--workspace must point to the directory containing lampyr.workspace.yaml")
    );
}

#[test]
fn existing_cluster_health_uses_only_kubernetes_without_loading_provider_artifacts() {
    let directory = tempfile::tempdir().unwrap();
    let tools = directory.path().join("tools");
    let root = directory.path().join("platform");
    let artifact = root.join("providers/third-party/artifact");
    std::fs::create_dir_all(&tools).unwrap();
    std::fs::create_dir_all(&artifact).unwrap();
    initialize_workspace(&root);
    std::fs::write(artifact.join("manifest.json"), "{}").unwrap();
    let kubectl = tools.join("kubectl");
    std::fs::write(&kubectl, r#"#!/bin/sh
case "$*" in
*'config current-context'*) printf arbitrary-profile;;
*'get configmap lampyr-install '*) printf '{"data":{"installationId":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","provider":"third-party","cluster":"actual"}}';;
*'get secret lampyr-runtime '*) ;;
*'get nodes '*) printf '{"items":[{"spec":{"taints":[{"effect":"NoSchedule"}]}},{"status":{"capacity":{"hyperlight.dev/hypervisor":"2k"}}}]}';;
*'get pods '*) printf '{"items":[]}';;
*'rollout status '*|*'wait '*) ;;
*) echo unexpected >&2; exit 71;;
esac
"#).unwrap();
    std::fs::set_permissions(&kubectl, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args([
            "cluster",
            "health",
            "--approve-provider-manifests",
            "--workspace",
        ])
        .arg(&root)
        .env("PATH", tools)
        .env("LAMPYR_HOME", directory.path().join("data"))
        .current_dir(directory.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["healthy"], true);
    assert_eq!(result["context"], "arbitrary-profile");
    assert!(!directory.path().join("data").exists());
}

#[test]
fn cli_has_no_built_in_provider_configuration_flags() {
    for forbidden in [
        "--subscription-id",
        "--tenant-id",
        "--location",
        "--node-vm-size",
        "--node-count",
        "--kind-config",
        "--http-port",
        "--platform-root",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_ly"))
            .args(["cluster", "install", forbidden, "example"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unexpected argument"),
            "{forbidden}"
        );
    }

    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["cluster", "install", "--allow-docker"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("require --provider"));
}

#[test]
fn eventing_is_an_explicit_cluster_install_capability() {
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["cluster", "health", "--eventing"])
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--eventing is only valid with cluster install")
    );

    let config = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(config.path(), "eventing: true\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["cluster", "install", "--provider-config"])
        .arg(config.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("use --eventing instead of eventing fields")
    );
}

#[test]
fn persisted_runtime_manifests_cannot_inject_kubectl_options() {
    let directory = tempfile::tempdir().unwrap();
    let tools = directory.path().join("tools");
    let root = directory.path().join("platform");
    std::fs::create_dir_all(&tools).unwrap();
    initialize_workspace(&root);
    let kubectl = tools.join("kubectl");
    std::fs::write(&kubectl, r#"#!/bin/sh
    printf '%s\n' "$*" >> "$CALL_LOG"
    case "$*" in
    *'config current-context'*) printf customer;;
    *'get configmap lampyr-install '*) printf '{"data":{"installationId":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","cluster":"actual"}}';;
    *'get secret lampyr-runtime '*) printf '%s' "$RUNTIME_SECRET";;
    *) echo 'unexpected mutation' >&2; exit 71;;
    esac
    "#).unwrap();
    std::fs::set_permissions(&kubectl, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (field, value) in [
        ("name", "--kubeconfig=/guest/exec.yaml"),
        ("kind", "--all"),
        ("namespace", "--kubeconfig=/guest/exec.yaml"),
    ] {
        for inventory in ["before", "after", "cleanup"] {
            let mut manifest = json!({"apiVersion":"v1","kind":"ConfigMap",
                    "metadata":{"name":"valid","namespace":"default"}});
            if field == "kind" {
                manifest[field] = json!(value);
            } else {
                manifest["metadata"][field] = json!(value);
            }

            let mut plan = json!({"before":[],"after":[],"cleanup":[]});
            plan[inventory] = json!([manifest]);
            let record = json!({
                "plan":plan,
                "settings":{},
                "eventing":false,
            });
            let secret = json!({
                "metadata":{"labels":{"lampyr.dev/installation-id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}},
                "data":{"runtime":base64::engine::general_purpose::STANDARD.encode(serde_json::to_vec(&record).unwrap())}
            });
            let log = directory.path().join(format!("{field}-{inventory}.log"));
            let output = Command::new(env!("CARGO_BIN_EXE_ly"))
                .args(["cluster", "uninstall", "--context", "customer"])
                .env("PATH", &tools)
                .env("CALL_LOG", &log)
                .env("RUNTIME_SECRET", secret.to_string())
                .env("LAMPYR_HOME", directory.path().join("data"))
                .current_dir(&root)
                .output()
                .unwrap();
            let error = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success());
            assert!(error.contains("invalid Kubernetes manifest"), "{error}");
            let calls = std::fs::read_to_string(log).unwrap();
            assert!(!calls.contains("delete"));
            assert!(!calls.contains(value));
        }
    }
}

#[test]
fn providerless_uninstall_refuses_live_event_source_inventory() {
    let directory = tempfile::tempdir().unwrap();
    let tools = directory.path().join("tools");
    let root = directory.path().join("platform");
    std::fs::create_dir_all(&tools).unwrap();
    initialize_workspace(&root);
    let kubectl = tools.join("kubectl");
    std::fs::write(
            &kubectl,
            r#"#!/bin/sh
    case "$*" in
    *'config current-context'*) printf customer;;
    *'get configmap lampyr-install '*) printf '{"data":{"installationId":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","cluster":"actual"}}';;
    *'get secret lampyr-runtime '*) ;;
    *'get secrets --selector '*) printf '{"items":[{"metadata":{"name":"lampyr-source-live"},"data":{"source":"e30="}}]}';;
    *) echo "unexpected: $*" >&2; exit 71;;
    esac
    "#,
        )
        .unwrap();
    std::fs::set_permissions(&kubectl, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["cluster", "uninstall", "--context", "customer"])
        .env("PATH", &tools)
        .env("LAMPYR_HOME", directory.path().join("data"))
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("event source inventory remains"), "{error}");
}

#[test]
fn providerless_uninstall_refuses_unlabeled_live_azure_sources() {
    let directory = tempfile::tempdir().unwrap();
    let tools = directory.path().join("tools");
    let root = directory.path().join("platform");
    std::fs::create_dir_all(&tools).unwrap();
    initialize_workspace(&root);
    let kubectl = tools.join("kubectl");
    std::fs::write(
        &kubectl,
        r#"#!/bin/sh
case "$*" in
*'config current-context'*) printf customer;;
*'get configmap lampyr-install '*) printf '{"data":{"installationId":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","cluster":"actual"}}';;
*'get secret lampyr-runtime '*) ;;
*'get secrets --selector '*) printf '{"items":[]}';;
*'get crd azureeventgridsources.sources.lampyr.dev '*) printf '{"kind":"CustomResourceDefinition"}';;
*'get azureeventgridsources.sources.lampyr.dev --all-namespaces --output=json'*) printf '{"items":[{"metadata":{"namespace":"default","name":"unlabeled"}}]}';;
*) echo "unexpected: $*" >&2; exit 71;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&kubectl, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["cluster", "uninstall", "--context", "customer"])
        .env("PATH", &tools)
        .env("LAMPYR_HOME", directory.path().join("data"))
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("live provider Event Sources remain"),
        "{error}"
    );
    assert!(error.contains("default/unlabeled"), "{error}");
}
