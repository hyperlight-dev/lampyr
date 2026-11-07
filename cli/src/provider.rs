// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use hyperlight_unikraft::{Mount, NetworkPolicy, SandboxBuilder};
use indicatif::ProgressBar;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::artifact::{Artifact, Capability, Manifest, Operation, SourceKind};
use crate::runtime::{CallOutput, Cancellation, PAYLOAD_LIMIT, Runtime};
use crate::workspace::Directory;

#[derive(Default)]
pub struct Authorization {
    pub workspace: Option<Arc<Directory>>,
    pub scaffold: bool,
    pub state: Option<Arc<Directory>>,
    pub network: Option<NetworkPolicy>,
    pub resolv_conf: Option<String>,
    pub auth_cache: Option<Arc<Directory>>,
    pub docker: Option<Arc<crate::docker_bridge::DockerBridge>>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Request {
    pub protocol_version: u32,
    pub operation: Operation,
    pub installation_id: String,
    pub config: Value,
}

#[derive(Deserialize)]
#[serde(tag = "status", rename_all = "lowercase", deny_unknown_fields)]
enum Reply {
    Ok {
        #[serde(rename = "protocolVersion")]
        protocol_version: u32,
        result: Value,
    },
    Error {
        #[serde(rename = "protocolVersion")]
        protocol_version: u32,
        code: String,
        message: String,
    },
}

pub struct Provider {
    runtime: Runtime,
    operation: Operation,
    deadline: Arc<Mutex<Instant>>,
    cancellation: Cancellation,
    artifact: Artifact,
    progress: ProgressBar,
    _docker: Option<Arc<crate::docker_bridge::DockerBridge>>,
}

impl Provider {
    pub fn boot(
        artifact: Artifact,
        operation: Operation,
        authorization: Authorization,
        timeout: Duration,
        cancellation: Cancellation,
    ) -> Result<Self> {
        Self::boot_with_host(
            artifact,
            operation,
            authorization,
            timeout,
            &crate::host::Host {
                verbose: false,
                cancellation,
            },
        )
    }

    pub(crate) fn boot_with_host(
        artifact: Artifact,
        operation: Operation,
        authorization: Authorization,
        timeout: Duration,
        host: &crate::host::Host,
    ) -> Result<Self> {
        let use_snapshot = artifact.snapshot.is_some();
        Self::boot_with_host_mode(
            artifact,
            operation,
            authorization,
            timeout,
            use_snapshot,
            host,
        )
    }

    fn boot_with_host_mode(
        artifact: Artifact,
        operation: Operation,
        authorization: Authorization,
        timeout: Duration,
        use_snapshot: bool,
        host: &crate::host::Host,
    ) -> Result<Self> {
        let cancellation = host.cancellation.clone();
        let label = format!("provider {} {operation:?}", artifact.manifest.id);
        let requested = artifact
            .manifest
            .operations
            .get(&operation)
            .context("provider does not support the requested operation")?;
        for capability in requested {
            let granted = match capability {
                Capability::WorkspaceRead => authorization.workspace.is_some(),
                Capability::WorkspaceScaffold => {
                    authorization.workspace.is_some() && authorization.scaffold
                }
                Capability::State => authorization.state.is_some(),
                Capability::Network => authorization.network.is_some(),
                Capability::AuthCache => authorization.auth_cache.is_some(),
                Capability::Docker => authorization.docker.is_some(),
                Capability::ProviderManifests => true,
            };
            ensure!(
                granted,
                "provider capability {capability:?} is not authorized"
            );
        }
        let boot_files = if use_snapshot {
            None
        } else {
            artifact.ensure_cold_compatible()?;
            Some(
                host.verbose_step(&format!("{label}: verify boot files"), || {
                    artifact.verified_boot_files()
                })?,
            )
        };
        ensure!(!cancellation.is_cancelled(), "provider operation cancelled");
        let deadline = Arc::new(Mutex::new(
            Instant::now()
                .checked_add(timeout)
                .context("provider deadline is out of range")?,
        ));
        let mut builder = if let Some(files) = &boot_files {
            host.verbose_step(&format!("{label}: validate root filesystem"), || {
                crate::artifact::validate_rootfs(&files.rootfs)
            })?;
            let mut builder = SandboxBuilder::from_initrd(&files.rootfs)
                .scratch_mb(artifact.manifest.memory_mib as usize);
            if let Some(kernel) = &files.kernel {
                builder = builder.kernel(kernel);
            }
            builder
        } else {
            host.verbose_step(&format!("{label}: load provider snapshot"), || {
                artifact.load_snapshot().map(SandboxBuilder::from_snapshot)
            })?
        }
        .mount(Mount::ro(artifact.directory(), "/provider"));
        for (name, value) in &artifact.manifest.environment {
            builder = builder.env(name, value);
        }
        if requested.contains(&Capability::WorkspaceRead) {
            let directory = authorization
                .workspace
                .as_ref()
                .context("workspace grant is missing")?;
            builder = builder.mount(Mount::ro(directory.root(), "/workspace"));
        }
        if requested.contains(&Capability::WorkspaceScaffold) {
            let directory = authorization
                .workspace
                .clone()
                .context("workspace grant is missing")?;
            builder = callback(
                builder,
                "workspace.scaffold",
                deadline.clone(),
                cancellation.clone(),
                move |files: BTreeMap<String, String>| {
                    directory.scaffold(&files)?;
                    Ok(json!({}))
                },
            );
        }
        if requested.contains(&Capability::State) {
            let directory = authorization
                .state
                .clone()
                .context("state grant is missing")?;
            builder = callback(
                builder,
                "state.read",
                deadline.clone(),
                cancellation.clone(),
                move |key: String| directory.read_state(&key),
            );
            let directory = authorization
                .state
                .clone()
                .context("state grant is missing")?;
            builder = callback(
                builder,
                "state.write",
                deadline.clone(),
                cancellation.clone(),
                move |input: StateWrite| {
                    directory.write_state(&input.key, &input.value)?;
                    Ok(json!({}))
                },
            );
        }
        if requested.contains(&Capability::Network) {
            builder = builder.network(authorization.network.context("network grant is missing")?);
            if let Some(content) = authorization.resolv_conf {
                builder = builder.resolv_conf(content);
            }
        }
        if requested.contains(&Capability::AuthCache) {
            let directory = authorization
                .auth_cache
                .context("auth cache grant is missing")?;
            builder = builder.mount(Mount::rw(directory.root(), "/auth"));
        }
        let docker = if requested.contains(&Capability::Docker) {
            let docker = authorization
                .docker
                .context("Docker access is not authorized")?;
            builder = docker.configure(builder);
            Some(docker)
        } else {
            None
        };
        let progress = crate::host::progress_bar()?;
        if host.verbose {
            progress.set_draw_target(indicatif::ProgressDrawTarget::hidden());
        }
        let messages = progress.clone();
        builder = callback(
            builder,
            "progress.message",
            deadline.clone(),
            cancellation.clone(),
            move |message: String| {
                ensure!(
                    message.len() <= 8192
                        && message
                            .chars()
                            .all(|c| !c.is_control() || c == '\n' || c == '\t'),
                    "invalid provider progress message"
                );
                if messages.is_hidden() {
                    eprintln!("{message}");
                } else {
                    messages.println(&message);
                }
                messages.set_message(message);
                Ok(json!({}))
            },
        );
        let startup_stage = if use_snapshot && artifact.snapshot.is_some() {
            "restore sandbox"
        } else {
            "boot sandbox"
        };
        let mut runtime = host.verbose_step(&format!("{label}: {startup_stage}"), || {
            Runtime::boot(builder)
        })?;
        host.verbose_step(&format!("{label}: load provider program"), || {
            runtime.load(&artifact.program, timeout, &cancellation)
        })?;
        host.verbose_step(
            &format!("{label}: Describe handshake and manifest verification"),
            || {
                let described = runtime.call(
                    "invoke",
                    &serde_json::to_value(Request {
                        protocol_version: 1,
                        operation: Operation::Describe,
                        installation_id: String::new(),
                        config: json!({}),
                    })?,
                    Duration::from_secs(10),
                    &cancellation,
                )?;
                let description: Manifest = serde_json::from_value(result(described)?.value)
                    .context("decode provider description")?;
                ensure!(
                    serde_json::to_value(&description)?
                        == serde_json::to_value(&artifact.manifest)?,
                    "provider description does not match its verified artifact manifest"
                );
                Ok(())
            },
        )?;
        Ok(Self {
            runtime,
            operation,
            deadline,
            cancellation,
            artifact,
            progress,
            _docker: docker,
        })
    }

    pub(crate) fn has_capability(&self, capability: Capability) -> bool {
        self.artifact.manifest.operations[&self.operation].contains(&capability)
    }

    pub(crate) fn source_kinds(&self) -> &std::collections::BTreeSet<SourceKind> {
        &self.artifact.manifest.source_kinds
    }

    pub fn invoke(&mut self, request: &Request, timeout: Duration) -> Result<CallOutput> {
        ensure!(
            request.protocol_version == 1,
            "unsupported provider request protocol"
        );
        ensure!(
            request.operation == self.operation,
            "provider operation is outside its authorization"
        );
        ensure!(
            !request.installation_id.is_empty() && request.installation_id.len() <= 128,
            "installation identity is required"
        );
        *self
            .deadline
            .lock()
            .map_err(|_| anyhow::anyhow!("provider clock is poisoned"))? = Instant::now()
            .checked_add(timeout)
            .context("provider deadline is out of range")?;
        let request = serde_json::to_value(request)?;
        crate::host::run_step(
            &self.progress,
            &format!(
                "Running {} provider operation: {:?}",
                self.artifact.manifest.id, self.operation,
            ),
            || {
                self.runtime
                    .call("invoke", &request, timeout, &self.cancellation)
                    .and_then(result)
            },
        )
    }
}

fn result(mut output: CallOutput) -> Result<CallOutput> {
    match serde_json::from_value(output.value).context("decode provider response envelope")? {
        Reply::Ok {
            protocol_version,
            result,
        } => {
            ensure!(
                protocol_version == 1,
                "unsupported provider response protocol"
            );
            output.value = result;
            Ok(output)
        }
        Reply::Error {
            protocol_version,
            code,
            message,
        } => {
            ensure!(
                protocol_version == 1,
                "unsupported provider response protocol"
            );
            bail!("provider {code}: {message}")
        }
    }
}

fn callback<Input, Output>(
    builder: SandboxBuilder,
    name: &str,
    deadline: Arc<Mutex<Instant>>,
    cancellation: Cancellation,
    function: impl Fn(Input) -> Result<Output> + Send + Sync + 'static,
) -> SandboxBuilder
where
    Input: DeserializeOwned,
    Output: Serialize,
{
    builder.host_function(name, move |arguments| {
        let result = (|| -> Result<String> {
            ensure!(
                arguments.len() <= PAYLOAD_LIMIT,
                "host call input exceeds 64 KiB"
            );
            ensure!(!cancellation.is_cancelled(), "provider operation cancelled");
            ensure!(
                Instant::now()
                    < *deadline
                        .lock()
                        .map_err(|_| anyhow::anyhow!("provider clock is poisoned"))?,
                "provider deadline exceeded"
            );
            let [input]: [Input; 1] =
                serde_json::from_str(arguments).context("decode provider host call")?;
            let output = serde_json::to_string(&function(input)?)?;
            ensure!(
                output.len() <= PAYLOAD_LIMIT,
                "host call result exceeds 64 KiB"
            );
            Ok(output)
        })();
        result.map_err(|error| format!("{error:#}"))
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateWrite {
    key: String,
    value: Value,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_progress_finishes_on_success_and_failure_and_can_be_reused() {
        let progress = ProgressBar::hidden();
        let value = crate::host::run_step(&progress, "Compiling template", || {
            assert!(!progress.is_finished());
            Ok(42)
        })
        .unwrap();
        assert_eq!(value, 42);
        assert!(progress.is_finished());
        let error = crate::host::run_step::<()>(&progress, "Waiting for deployment", || {
            assert!(!progress.is_finished());
            bail!("deployment failed")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "deployment failed");
        assert!(progress.is_finished());
    }
}
