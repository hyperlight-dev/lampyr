// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use lampyr_cli::artifact::{Artifact, Operation};
use lampyr_cli::docker_bridge::DockerBridge;
use lampyr_cli::provider::{Authorization, Provider, Request};
use lampyr_cli::runtime::Cancellation;
use lampyr_cli::workspace::Directory;
use serde_json::{Value, json};

struct Installation {
    provider: &'static str,
    artifact: PathBuf,
    workspace: Arc<Directory>,
    state: Arc<Directory>,
    docker: Arc<DockerBridge>,
    id: String,
    config: Value,
    active: bool,
}

impl Installation {
    fn invoke(&self, operation: Operation) -> Result<Value> {
        let mut provider = Provider::boot(
            Artifact::load(&self.artifact, self.provider)?,
            operation,
            Authorization {
                workspace: Some(self.workspace.clone()),
                scaffold: operation == Operation::Init,
                state: Some(self.state.clone()),
                docker: Some(self.docker.clone()),
                ..Default::default()
            },
            Duration::from_secs(30),
            Cancellation::default(),
        )?;
        Ok(provider
            .invoke(
                &Request {
                    protocol_version: 1,
                    operation,
                    installation_id: self.id.clone(),
                    config: self.config.clone(),
                },
                Duration::from_secs(420),
            )?
            .value)
    }
}

impl Drop for Installation {
    fn drop(&mut self) {
        if self.active
            && let Err(error) = self.invoke(Operation::Delete)
        {
            eprintln!(
                "Owned {} provider test cleanup failed: {error:#}",
                self.provider
            );
        }
    }
}

#[test]
fn external_kind_provider_owns_a_disposable_cluster_and_preserves_customer_source() -> Result<()> {
    local_provider_lifecycle("kind", "LAMPYR_TEST_KIND_ARTIFACT")
}

#[test]
fn external_k3d_provider_owns_a_disposable_cluster_and_preserves_customer_source() -> Result<()> {
    local_provider_lifecycle("k3d", "LAMPYR_TEST_K3D_ARTIFACT")
}

fn local_provider_lifecycle(provider: &'static str, artifact_variable: &str) -> Result<()> {
    let artifact = PathBuf::from(std::env::var_os(artifact_variable).with_context(|| {
        format!("set {artifact_variable} to providers/{provider}/build/rootfs")
    })?);
    let temporary = tempfile::tempdir()?;
    let suffix = chrono::Utc::now().timestamp_nanos_opt().unwrap() as u128;
    let name = format!("lp-{}-{suffix:x}", std::process::id());
    let port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let mut installation = Installation {
        provider,
        artifact,
        workspace: Arc::new(Directory::private(temporary.path().join("source"))?),
        state: Arc::new(Directory::private(temporary.path().join("state"))?),
        docker: Arc::new(DockerBridge::from_environment()?),
        id: format!("{suffix:032x}"),
        config: json!({"name":name,"httpPort":0,"httpsPort":0,"registryPort":port}),
        active: false,
    };
    installation.invoke(Operation::Init)?;
    let source = installation
        .workspace
        .root()
        .join(format!("{provider}-config.yaml"));
    let edited = std::fs::read_to_string(&source)? + "\n# Customer-owned infrastructure.\n";
    std::fs::write(&source, &edited)?;
    installation.active = true;
    ensure!(
        installation.invoke(Operation::Apply)?["ready"] == true,
        "local infrastructure is not ready"
    );
    let cluster = installation
        .state
        .read_state("cluster")?
        .context("missing container ownership state")?;
    ensure!(
        cluster["nodes"]
            .as_object()
            .is_some_and(|nodes| !nodes.is_empty()),
        "container identities are missing"
    );
    let inspected = installation.invoke(Operation::Inspect)?;
    ensure!(
        inspected["owned"] == true,
        "infrastructure ownership inspection failed"
    );
    let connection = installation.invoke(Operation::Connect)?;
    ensure!(
        connection["kind"] == "Config",
        "connection must be a plain kubeconfig"
    );
    lampyr_cli::connection::validate(
        &serde_yaml_ng::to_string(&connection)?,
        &format!("{provider}-{name}"),
    )?;
    let mut replaced = cluster.clone();
    let node = replaced["nodes"]
        .as_object_mut()
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    *node = "replaced-container".into();
    installation.state.write_state("cluster", &replaced)?;
    let refused = installation.invoke(Operation::Inspect);
    installation.state.write_state("cluster", &cluster)?;
    ensure!(refused.is_err(), "provider adopted replaced containers");
    installation.invoke(Operation::Apply)?;
    ensure!(
        std::fs::read_to_string(&source)? == edited,
        "provider overwrote customer source"
    );
    installation.invoke(Operation::Delete)?;
    installation.active = false;
    ensure!(
        installation.invoke(Operation::Inspect)?["exists"] == false,
        "owned local cluster still exists"
    );
    Ok(())
}
