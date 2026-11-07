// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::time::Duration;

use hyperlight_unikraft::SandboxBuilder;
use lampyr_cli::artifact::Artifact;
use lampyr_cli::docker_bridge::DockerBridge;
use lampyr_cli::runtime::{Cancellation, Runtime};
use serde_json::json;

const PROBE: &str = r#"
import subprocess

def invoke(request):
    result = subprocess.run(request["args"], text=True, capture_output=True, timeout=request.get("timeout", 30))
    return {"exit": result.returncode, "stdout": result.stdout, "stderr": result.stderr}
"#;

fn builder(artifact: &Artifact) -> SandboxBuilder {
    let files = artifact.verified_boot_files().unwrap();
    let builder = SandboxBuilder::from_initrd(files.rootfs).scratch_mb(512);
    match files.kernel {
        Some(kernel) => builder.kernel(kernel),
        None => builder,
    }
}

#[test]
fn kind_and_docker_binaries_execute_inside_the_guest() {
    let artifact = Artifact::load(
        std::env::var_os("LAMPYR_TEST_KIND_ARTIFACT")
            .expect("set LAMPYR_TEST_KIND_ARTIFACT to the Kind provider artifact"),
        "kind",
    )
    .unwrap();
    let mut runtime = Runtime::boot(builder(&artifact)).unwrap();
    let cancellation = Cancellation::default();
    runtime
        .load(PROBE, Duration::from_secs(10), &cancellation)
        .unwrap();
    for (program, argument, expected) in [
        ("kind", "version", "v0.33.0"),
        ("docker", "--version", "29.4.1"),
    ] {
        let output = runtime
            .call(
                "invoke",
                &json!({"args":[format!("/usr/local/bin/{program}"), argument]}),
                Duration::from_secs(45),
                &cancellation,
            )
            .unwrap();
        assert_eq!(output.value["exit"], 0, "{}", output.value);
        assert!(
            output.value["stdout"].as_str().unwrap().contains(expected),
            "{}",
            output.value
        );
    }
}

#[test]
fn guest_kind_creates_and_deletes_a_cluster_using_only_docker() {
    let artifact = Artifact::load(
        std::env::var_os("LAMPYR_TEST_KIND_ARTIFACT")
            .expect("set LAMPYR_TEST_KIND_ARTIFACT to the Kind provider artifact"),
        "kind",
    )
    .unwrap();
    let bridge = DockerBridge::from_environment().unwrap();
    let configured = bridge
        .configure(builder(&artifact))
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("KIND_EXPERIMENTAL_PROVIDER", "docker");
    let mut runtime = Runtime::boot(configured).unwrap();
    let cancellation = Cancellation::default();
    runtime
        .load(PROBE, Duration::from_secs(10), &cancellation)
        .unwrap();
    let name = format!(
        "lampyr-guest-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp()
    );
    let created = runtime.call(
            "invoke",
            &json!({"timeout":240, "args":[
                "/usr/local/bin/kind", "create", "cluster", "--name", name,
                "--image", "kindest/node:v1.35.8@sha256:07b2536e30b803ed61d1677a79df6115f798ce64c80f9e22f6ed45afd09323c0",
                "--kubeconfig", "/tmp/proof-kubeconfig", "--wait", "180s"
            ]}),
            Duration::from_secs(270),
            &cancellation,
        );
    if let Err(error) = &created {
        eprintln!("guest Kind create failed for {name}: {error:#}");
    }
    let connection = runtime.call(
        "invoke",
        &json!({"args":["/usr/local/bin/kind", "get", "kubeconfig", "--name", name]}),
        Duration::from_secs(45),
        &cancellation,
    );
    drop(runtime);
    let mut runtime = Runtime::boot(
        bridge
            .configure(builder(&artifact))
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .env("KIND_EXPERIMENTAL_PROVIDER", "docker"),
    )
    .unwrap();
    runtime
        .load(PROBE, Duration::from_secs(10), &cancellation)
        .unwrap();
    let deleted = runtime.call(
        "invoke",
        &json!({"timeout":120,"args":["/usr/local/bin/kind", "delete", "cluster", "--name", name]}),
        Duration::from_secs(150),
        &cancellation,
    );
    let deleted = deleted.expect("guest must remain usable for owned cluster cleanup");
    assert_eq!(deleted.value["exit"], 0, "{}", deleted.value);
    let created = created.unwrap();
    assert_eq!(created.value["exit"], 0, "{}", created.value);
    let connection = connection.unwrap();
    assert_eq!(connection.value["exit"], 0, "{}", connection.value);
    let config: serde_json::Value =
        serde_yaml_ng::from_str(connection.value["stdout"].as_str().unwrap()).unwrap();
    assert_eq!(config["kind"], "Config");
    assert_eq!(config["current-context"], format!("kind-{name}"));
}

#[test]
fn guest_docker_uses_authorized_socket_bridge() {
    let artifact = Artifact::load(
        std::env::var_os("LAMPYR_TEST_KIND_ARTIFACT")
            .expect("set LAMPYR_TEST_KIND_ARTIFACT to the Kind provider artifact"),
        "kind",
    )
    .unwrap();
    let bridge = DockerBridge::from_environment().unwrap();
    let mut runtime = Runtime::boot(bridge.configure(builder(&artifact)))
        .expect("authorize only the Docker daemon API for the Kind guest");
    let cancellation = Cancellation::default();
    runtime
        .load(PROBE, Duration::from_secs(10), &cancellation)
        .unwrap();
    let output = runtime
        .call(
            "invoke",
            &json!({"args":["/usr/local/bin/docker", "version", "--format", "{{.Server.Version}}"]}),
            Duration::from_secs(45),
            &cancellation,
        )
        .unwrap();
    assert_eq!(output.value["exit"], 0, "{}", output.value);
    assert!(!output.value["stdout"].as_str().unwrap().trim().is_empty());
}
