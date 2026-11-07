// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::time::Duration;

use hyperlight_unikraft::SandboxBuilder;
use lampyr_cli::runtime::{Cancellation, Runtime};
use serde_json::{Value, json};

#[test]
fn azure_provider_requires_dedicated_credentials_and_never_prompts_unattended() {
    use hyperlight_unikraft::{AllowList, NetworkPolicy};
    use lampyr_cli::artifact::{Artifact, Operation};
    use lampyr_cli::provider::{Authorization, Provider, Request};
    use lampyr_cli::workspace::Directory;
    use std::sync::Arc;

    let artifact = std::env::var_os("LAMPYR_TEST_AZURE_ARTIFACT")
        .expect("set LAMPYR_TEST_AZURE_ARTIFACT to the independently built Azure artifact");
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("source");
    std::fs::create_dir(&workspace).unwrap();
    let denied_hosts: &[&str] = &[];
    let mut provider = Provider::boot(
        Artifact::load(artifact, "azure").unwrap(),
        Operation::Init,
        Authorization {
            workspace: Some(Arc::new(Directory::open(&workspace).unwrap())),
            scaffold: true,
            state: Some(Arc::new(
                Directory::private(directory.path().join("state")).unwrap(),
            )),
            auth_cache: Some(Arc::new(
                Directory::private(directory.path().join("auth")).unwrap(),
            )),
            network: Some(NetworkPolicy::AllowList(
                AllowList::from_hosts(denied_hosts).unwrap(),
            )),
            ..Default::default()
        },
        Duration::from_secs(30),
        Cancellation::default(),
    )
    .unwrap();
    let error = provider
        .invoke(
            &Request {
                protocol_version: 1,
                operation: Operation::Init,
                installation_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                config: json!({ "name": "azure-auth-proof", "interactive": false,
                    "subscriptionId":"11111111-1111-1111-1111-111111111111" }),
            },
            Duration::from_secs(90),
        )
        .err()
        .expect("an empty dedicated cache must require explicit authentication");
    assert!(
        error.to_string().contains("authentication-required"),
        "{error:#}"
    );
    assert!(std::fs::read_dir(workspace).unwrap().next().is_none());
    assert!(
        std::fs::read_dir(directory.path().join("state"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn azure_provider_owns_event_source_planning_without_kubernetes_access() {
    use hyperlight_unikraft::{AllowList, NetworkPolicy};
    use lampyr_cli::artifact::{Artifact, Operation};
    use lampyr_cli::provider::{Authorization, Provider, Request};
    use lampyr_cli::workspace::Directory;
    use std::sync::Arc;

    let artifact = std::env::var_os("LAMPYR_TEST_AZURE_ARTIFACT")
        .expect("set LAMPYR_TEST_AZURE_ARTIFACT to the independently built Azure artifact");
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("source");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("event-sources.json"),
        json!({"blob-created":{
            "storageAccount":"images",
            "container":"uploads",
            "eventTypes":["Microsoft.Storage.BlobCreated"]
        }})
        .to_string(),
    )
    .unwrap();
    let state = Arc::new(Directory::private(directory.path().join("state")).unwrap());
    state
        .write_state(
            "cluster",
            &json!({
                "installationId":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "parameters":{
                    "subscriptionId":"11111111-1111-1111-1111-111111111111",
                    "resourceGroupName":"shared",
                    "eventingEnabled":true
                }
            }),
        )
        .unwrap();
    let denied_hosts: &[&str] = &[];
    let mut provider = Provider::boot(
        Artifact::load(artifact, "azure").unwrap(),
        Operation::SourceCreate,
        Authorization {
            workspace: Some(Arc::new(Directory::open(&workspace).unwrap())),
            state: Some(state),
            auth_cache: Some(Arc::new(
                Directory::private(directory.path().join("auth")).unwrap(),
            )),
            network: Some(NetworkPolicy::AllowList(
                AllowList::from_hosts(denied_hosts).unwrap(),
            )),
            ..Default::default()
        },
        Duration::from_secs(30),
        Cancellation::default(),
    )
    .unwrap();
    let output = provider
        .invoke(
            &Request {
                protocol_version: 1,
                operation: Operation::SourceCreate,
                installation_id: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
                config: json!({
                    "name":"blob-created",
                    "namespace":"default",
                    "service":"blob-handler",
                    "observations":[]
                }),
            },
            Duration::from_secs(30),
        )
        .unwrap();
    assert_eq!(output.value["sharedManifests"][0]["kind"], "Broker");
    assert_eq!(output.value["manifests"][0]["kind"], "Trigger");
    assert_eq!(output.value["manifests"][1]["kind"], "AzureEventGridSource");
}

#[test]
fn native_azure_install_refuses_to_provision_without_its_own_credentials() {
    let artifact = std::env::var_os("LAMPYR_TEST_AZURE_ARTIFACT")
        .expect("set LAMPYR_TEST_AZURE_ARTIFACT to the independently built Azure artifact");
    let directory = tempfile::tempdir().unwrap();
    let artifact = offline_auth_artifact(std::path::Path::new(&artifact), directory.path());
    let workspace = directory.path().join("workspace");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(
        workspace.join("lampyr.workspace.yaml"),
        "apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\n",
    )
    .unwrap();
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let config = directory.path().join("provider.json");
    // Valid controller inputs, so the install gets as far as Azure's own
    // credentials, which are what this proves it refuses without.
    let manifest = directory.path().join("controller.yaml");
    std::fs::write(&manifest, "").unwrap();
    std::fs::write(&config, json!({
        "fiberdImage":"example.invalid/home:offline", "routerImage":"example.invalid/router:offline",
        "controllerManifest": manifest,
        "grantImage": format!("registry.example/fiberd@sha256:{}", "0".repeat(64)),
        "helperImage": format!("registry.example/helper@sha256:{}", "1".repeat(64)),
        "zygoteRegistries": ["registry.example/templates"],
        "subscriptionId":"11111111-1111-1111-1111-111111111111"
    }).to_string()).unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_ly"))
        .args([
            "cluster",
            "install",
            "--provider",
            "azure",
            "--name",
            "azure-auth-proof",
            "--non-interactive",
        ])
        .arg("--provider-config")
        .arg(config)
        .arg("--provider-artifact")
        .arg(artifact)
        .arg("--workspace")
        .arg(&workspace)
        .env("LAMPYR_HOME", directory.path().join("data"))
        .env("KUBECONFIG", directory.path().join("kubeconfig"))
        .current_dir(root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("authentication-required"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let source = workspace.join("infra/azure/azure-auth-proof");
    assert!(std::fs::read_dir(source).unwrap().next().is_none());
    let installation = directory
        .path()
        .join("data/installations/azure/azure-auth-proof");
    let record: Value = serde_json::from_slice(
        &std::fs::read(installation.join("host/installation.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["provider"], "azure");
    assert!(!installation.join("provider/cluster.json").exists());
    assert!(!directory.path().join("kubeconfig").exists());
}

fn offline_auth_artifact(
    source: &std::path::Path,
    directory: &std::path::Path,
) -> std::path::PathBuf {
    use lampyr_cli::artifact::{Capability, Manifest, Operation};

    let mut manifest: Manifest =
        serde_json::from_slice(&std::fs::read(source.join("manifest.json")).unwrap()).unwrap();
    // Authentication must fail before network access, independently of the host DNS setup.
    assert!(
        manifest
            .operations
            .get_mut(&Operation::Init)
            .unwrap()
            .remove(&Capability::Network)
    );
    manifest.snapshot_index = None;
    let artifact = directory.join("offline-auth-artifact");
    std::fs::create_dir(&artifact).unwrap();
    for file in [&manifest.rootfs, &manifest.program]
        .into_iter()
        .chain(manifest.kernel.iter())
    {
        let destination = artifact.join(&file.path);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        let source = source.join(&file.path);
        if let Err(error) = std::fs::hard_link(&source, &destination) {
            std::fs::copy(&source, &destination).unwrap_or_else(|copy_error| {
                panic!(
                    "copy provider artifact file {} after hard-link failure ({error}): {copy_error}",
                    source.display()
                )
            });
        }
    }
    std::fs::write(
        artifact.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    artifact
}

#[test]
fn shared_eventgrid_integration_runs_in_the_packaged_guest_without_azure_tools() {
    use lampyr_cli::artifact::Artifact;

    let artifact = Artifact::load(
        std::env::var_os("LAMPYR_TEST_AZURE_ARTIFACT")
            .expect("set LAMPYR_TEST_AZURE_ARTIFACT to the independently built Azure artifact"),
        "azure",
    )
    .unwrap();
    let files = artifact.verified_boot_files().unwrap();
    let mut builder =
        SandboxBuilder::from_initrd(files.rootfs).scratch_mb(artifact.manifest.memory_mib as usize);
    if let Some(kernel) = files.kernel {
        builder = builder.kernel(kernel);
    }
    let mut runtime = Runtime::boot(builder).expect("boot integration guest");
    let cancellation = Cancellation::default();
    runtime
        .load(
            r#"
from lampyr_eventgrid import EventGrid
from pathlib import Path

def invoke(request):
    def fail(code, message):
        raise RuntimeError(code + ": " + message)
    return EventGrid(Path("/templates"), fail).manifests(request["images"], request["bindings"])
"#,
            Duration::from_secs(10),
            &cancellation,
        )
        .expect("load separately packaged integration");
    {
        let images = json!({
            "eventGridControllerImage":"registry/controller:v1",
            "eventGridAdapterImage":"registry/adapter:v1"
        });
        let bindings = json!({
            "subscriptionId":"subscription", "controllerClientId":"controller",
            "fiberdClientId":"fiberd", "publisherResourceGroup":"publishers",
            "webhookHost":"test.azurefd.net", "frontDoorId":"frontdoor",
            "publicIpName":"webhook", "resourceGroup":"dedicated"
        });
        let result = runtime
            .call(
                "invoke",
                &json!({
                    "images":images, "bindings":bindings
                }),
                Duration::from_secs(15),
                &cancellation,
            )
            .expect("render integration without Azure CLI, credentials, or network");
        let items = result.value["items"].as_array().unwrap();
        let deployments: Vec<_> = items
            .iter()
            .filter(|item| item["kind"] == "Deployment")
            .collect();
        assert_eq!(deployments.len(), 2);
        let images_by_name: std::collections::HashMap<_, _> = deployments
            .iter()
            .map(|deployment| {
                (
                    deployment["metadata"]["name"].as_str().unwrap(),
                    deployment["spec"]["template"]["spec"]["containers"][0]["image"]
                        .as_str()
                        .unwrap(),
                )
            })
            .collect();
        assert_eq!(
            images_by_name["eventgrid-controller"],
            "registry/controller:v1"
        );
        assert_eq!(images_by_name["eventgrid-adapter"], "registry/adapter:v1");
    }
}

#[test]
fn repeated_bicep_compilation_without_network_or_credentials() {
    use lampyr_cli::artifact::Artifact;

    let artifact = Artifact::load(
        std::env::var_os("LAMPYR_TEST_AZURE_ARTIFACT")
            .expect("set LAMPYR_TEST_AZURE_ARTIFACT to the independently built Azure artifact"),
        "azure",
    )
    .unwrap();
    let files = artifact.verified_boot_files().unwrap();
    let mut builder =
        SandboxBuilder::from_initrd(files.rootfs).scratch_mb(artifact.manifest.memory_mib as usize);
    if let Some(kernel) = files.kernel {
        builder = builder.kernel(kernel);
    }
    for (name, value) in &artifact.manifest.environment {
        builder = builder.env(name, value);
    }
    let mut runtime = Runtime::boot(builder).expect("boot offline Azure guest");
    let cancellation = Cancellation::default();
    runtime
        .load(
            include_str!("../../providers/azure/tests/probe.py"),
            Duration::from_secs(10),
            &cancellation,
        )
        .expect("load subprocess probe");
    for iteration in 1..=8 {
        eprintln!("Bicep compilation {iteration}/8: starting");
        let output = runtime
            .call(
                "invoke",
                &json!({"args": [
                    "/bin/az", "bicep", "build", "--file", "/templates/main.bicep",
                    "--outfile", "/tmp/lampyr-bicep-repeat.json", "--only-show-errors"
                ]}),
                Duration::from_secs(90),
                &cancellation,
            )
            .unwrap_or_else(|error| panic!("Bicep compilation {iteration}/8: {error:#}"));
        assert_eq!(
            output.value["exit"], 0,
            "Bicep compilation {iteration}/8: {}",
            output.value
        );
        eprintln!("Bicep compilation {iteration}/8: succeeded");
    }
}

#[test]
fn azure_cli_compiles_bicep_inside_the_guest_without_network_or_credentials() {
    let rootfs = std::env::var_os("LAMPYR_TEST_AZURE_ROOTFS")
        .expect("set LAMPYR_TEST_AZURE_ROOTFS to the Azure proof CPIO");
    let mut builder = SandboxBuilder::from_initrd(rootfs)
        .scratch_mb(2048)
        .env("AZURE_CORE_COLLECT_TELEMETRY", "no")
        .env("AZURE_EXTENSION_USE_DYNAMIC_INSTALL", "no")
        .env("AZURE_BICEP_USE_BINARY_FROM_PATH", "true")
        .env("DOTNET_SYSTEM_GLOBALIZATION_INVARIANT", "1")
        .env("DOTNET_EnableWriteXorExecute", "0")
        .env("COMPlus_EnableDiagnostics", "0")
        .env("DOTNET_GCHeapHardLimitPercent", "0x32")
        .env("DOTNET_gcServer", "0")
        .env("DOTNET_gcConcurrent", "0")
        .env("DOTNET_DefaultStackSize", "0x40000")
        .env("DOTNET_ThreadPool_ForceMinWorkerThreads", "1")
        .env("DOTNET_ThreadPool_ForceMaxWorkerThreads", "4");
    if let Some(kernel) = std::env::var_os("LAMPYR_TEST_KERNEL") {
        builder = builder.kernel(kernel);
    }

    let mut runtime = Runtime::boot(builder).expect("boot Azure guest");
    let cancellation = Cancellation::default();
    runtime
        .load(
            include_str!("../../providers/azure/tests/probe.py"),
            Duration::from_secs(10),
            &cancellation,
        )
        .expect("load Azure probe");
    let version = runtime
        .call(
            "invoke",
            &json!({"args": ["/bin/az", "version", "--output", "json"]}),
            Duration::from_secs(90),
            &cancellation,
        )
        .expect("invoke Azure CLI");
    assert_eq!(version.value["exit"], 0, "{}", version.value);
    let versions: Value = serde_json::from_str(version.value["stdout"].as_str().unwrap()).unwrap();
    assert_eq!(versions["azure-cli"], "2.91.0");
    let authentication = runtime
        .call(
            "invoke",
            &json!({"args": [
                "/bin/az", "account", "get-access-token", "--resource", "https://management.azure.com/"
            ]}),
            Duration::from_secs(90),
            &cancellation,
        )
        .expect("load the real Azure authentication path without credentials");
    assert_eq!(authentication.value["exit"], 1, "{}", authentication.value);
    assert!(
        authentication.value["stderr"]
            .as_str()
            .unwrap()
            .contains("az login"),
        "{}",
        authentication.value
    );
    let remapping = runtime
        .call(
            "invoke",
            &json!({"args": ["/proof/mremap"]}),
            Duration::from_secs(10),
            &cancellation,
        )
        .expect("exercise mremap semantics");
    assert_eq!(remapping.value["exit"], 0, "{}", remapping.value);
    let memory = runtime
        .call(
            "invoke",
            &json!({"args": ["/proof/memory"]}),
            Duration::from_secs(30),
            &cancellation,
        )
        .expect("exercise process mapping lifetimes");
    assert_eq!(memory.value["exit"], 0, "{}", memory.value);
    let stack = runtime
        .call(
            "invoke",
            &json!({"args": ["/proof/stack"]}),
            Duration::from_secs(10),
            &cancellation,
        )
        .expect("inspect native stack");
    eprintln!("native stack: {}", stack.value);
    assert_eq!(stack.value["exit"], 0, "{}", stack.value);
    let compiled = runtime
        .call(
            "invoke",
            &json!({"args": [
                "/bin/az", "bicep", "build", "--file", "/proof/main.bicep", "--stdout"
            ]}),
            Duration::from_secs(90),
            &cancellation,
        )
        .expect("invoke Azure CLI Bicep build");
    assert_eq!(compiled.value["exit"], 0, "{}", compiled.value);
    let template: Value = serde_json::from_str(compiled.value["stdout"].as_str().unwrap()).unwrap();
    assert_eq!(template["parameters"]["name"]["defaultValue"], "Lampyr");
    assert_eq!(template["outputs"]["greeting"]["type"], "string");
    let infrastructure = runtime
        .call(
            "invoke",
            &json!({"args": [
                "/bin/az", "bicep", "build", "--file", "/templates/main.bicep", "--stdout"
            ]}),
            Duration::from_secs(90),
            &cancellation,
        )
        .expect("compile the complete AKS and optional Eventing infrastructure");
    assert_eq!(infrastructure.value["exit"], 0, "{}", infrastructure.value);
    let infrastructure: Value =
        serde_json::from_str(infrastructure.value["stdout"].as_str().unwrap()).unwrap();
    assert_eq!(infrastructure["outputs"]["lampyr"]["type"], "object");
    assert!(
        infrastructure["resources"]
            .as_array()
            .unwrap()
            .iter()
            .any(|resource| { resource["type"] == "Microsoft.ContainerService/managedClusters" })
    );
}
