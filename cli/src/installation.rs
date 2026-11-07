// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::{fs::File, io::Read};

use anyhow::{Context, Result, ensure};
use hyperlight_unikraft::{AllowList, NetworkPolicy};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::artifact::{Artifact, Capability, Operation};
use crate::host::Host;
use crate::provider::{Authorization, Provider, Request};
use crate::workspace::Directory;

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Record {
    schema: u32,
    pub id: String,
    pub provider: String,
    pub name: String,
    #[serde(rename = "workspace")]
    pub infrastructure: PathBuf,
    #[serde(default)]
    pub artifact: Option<PathBuf>,
    #[serde(default)]
    artifact_sha256: Option<String>,
    pub docker: bool,
}

pub struct Installation {
    pub record: Record,
    pub(crate) host_state: Arc<Directory>,
    pub provider_state: Arc<Directory>,
    pub source: Arc<Directory>,
    pub artifact: PathBuf,
    artifact_sha256: String,
    root: PathBuf,
    workspace_root: PathBuf,
    cancellation: crate::runtime::Cancellation,
    _lock: std::fs::File,
}

fn default_artifact(root: &Path, provider: &str) -> Result<PathBuf> {
    let project = root.join("providers").join(provider);
    let packaged = project.join("artifact");
    let path = if packaged.try_exists()? {
        packaged
    } else {
        project.join("build/rootfs")
    };
    path.canonicalize().with_context(|| {
        format!(
            "could not locate {provider} provider artifact at {}; specify \
             --provider-artifact with the path to a built provider artifact",
            path.display()
        )
    })
}

fn artifact_digest(path: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    File::open(path.join("manifest.json"))
        .context("open provider artifact manifest")?
        .take(crate::runtime::PAYLOAD_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= crate::runtime::PAYLOAD_LIMIT,
        "provider artifact manifest exceeds 64 KiB"
    );
    Ok(Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

fn verify_artifact_identity(path: &Path, expected: &str) -> Result<()> {
    ensure!(
        expected.len() == 64
            && expected
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "invalid provider artifact identity"
    );
    ensure!(
        artifact_digest(path)? == expected,
        "provider artifact manifest changed after approval"
    );
    Ok(())
}

fn approve_explicit_artifact(
    record: &mut Record,
    path: &Path,
    cancellation: &crate::runtime::Cancellation,
) -> Result<bool> {
    let artifact = Artifact::load_with_cancellation(path, &record.provider, cancellation)?;
    let digest = artifact.manifest_sha256().to_owned();
    if record.artifact.as_deref() == Some(path)
        && record.artifact_sha256.as_deref() == Some(&digest)
    {
        return Ok(false);
    }
    record.artifact = Some(path.to_owned());
    record.artifact_sha256 = Some(digest);
    Ok(true)
}

impl Installation {
    pub fn open(
        root: &Path,
        provider: &str,
        name: &str,
        workspace_root: Option<&Path>,
        artifact: Option<&Path>,
    ) -> Result<Self> {
        Self::open_with_cancellation(
            root,
            provider,
            name,
            workspace_root,
            artifact,
            crate::runtime::Cancellation::default(),
        )
    }

    pub fn open_with_cancellation(
        root: &Path,
        provider: &str,
        name: &str,
        workspace_root: Option<&Path>,
        artifact: Option<&Path>,
        cancellation: crate::runtime::Cancellation,
    ) -> Result<Self> {
        crate::project::validate_name(provider)?;
        crate::project::validate_name(name)?;
        let selected_workspace = workspace_root
            .map(crate::customer_workspace::root)
            .transpose()?;
        let explicit_artifact = artifact
            .map(|path| path.canonicalize().context("resolve provider artifact"))
            .transpose()?;
        let directory = crate::platform::home_dir()?
            .join("installations")
            .join(provider)
            .join(name);
        let installation_root = Directory::private(&directory)?;
        let host_state = Arc::new(Directory::private(installation_root.root().join("host"))?);
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let lock = options.open(host_state.root().join("installation.lock"))?;
        lock.try_lock()
            .context("another command is using this installation")?;
        let (mut record, workspace_root, created) =
            if let Some(value) = host_state.read_state("installation")? {
                let record: Record =
                    serde_json::from_value(value).context("decode installation identity")?;
                ensure!(
                    record.schema == 2 && record.provider == provider && record.name == name,
                    "incompatible installation identity"
                );
                ensure!(
                    record.id.len() == 32
                        && record
                            .id
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                    "invalid installation identity"
                );
                let workspace_root = crate::customer_workspace::discover(&record.infrastructure)?;
                if let Some(selected) = &selected_workspace {
                    ensure!(
                        selected == &workspace_root,
                        "installation belongs to Workspace at {}",
                        workspace_root.display()
                    );
                }
                (record, workspace_root, false)
            } else {
                let workspace_root = match selected_workspace {
                    Some(workspace) => workspace,
                    None => crate::customer_workspace::discover(&std::env::current_dir()?)?,
                };
                let infrastructure = workspace_root.join("infra").join(provider).join(name);
                std::fs::create_dir_all(&infrastructure)?;
                ensure!(
                    std::fs::read_dir(&infrastructure)?
                        .next()
                        .transpose()?
                        .is_none(),
                    "new infrastructure workspace must be empty: {}",
                    infrastructure.display()
                );
                let identity = format!(
                    "{provider}:{}:{}",
                    std::process::id(),
                    SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
                );
                let id = Sha256::digest(identity)
                    .iter()
                    .take(16)
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                let artifact = match explicit_artifact.clone() {
                    Some(path) => path,
                    None => default_artifact(root, provider)?,
                };
                let artifact_sha256 = artifact_digest(&artifact)?;
                let record = Record {
                    schema: 2,
                    id,
                    provider: provider.into(),
                    name: name.into(),
                    infrastructure: infrastructure.canonicalize()?,
                    artifact: Some(artifact),
                    artifact_sha256: Some(artifact_sha256),
                    docker: false,
                };
                host_state.write_state("installation", &serde_json::to_value(&record)?)?;
                (record, workspace_root, true)
            };
        let source = Arc::new(Directory::open(&record.infrastructure)?);
        let provider_state = Arc::new(Directory::private(
            installation_root.root().join("provider"),
        )?);
        if !created && record.artifact.is_none() && explicit_artifact.is_none() {
            anyhow::bail!(
                "installation has no trusted provider artifact; rerun with --provider-artifact"
            );
        }
        if let Some(path) = &explicit_artifact
            && approve_explicit_artifact(&mut record, path, &cancellation)?
        {
            host_state.write_state("installation", &serde_json::to_value(&record)?)?;
        }
        let artifact = record
            .artifact
            .clone()
            .context("installation has no trusted provider artifact")?;
        let artifact_sha256 = record
            .artifact_sha256
            .clone()
            .context("installation has no trusted provider artifact identity")?;
        verify_artifact_identity(&artifact, &artifact_sha256)?;
        Ok(Self {
            record,
            host_state,
            provider_state,
            source,
            artifact,
            artifact_sha256,
            root: root.canonicalize()?,
            workspace_root,
            cancellation,
            _lock: lock,
        })
    }

    pub fn authorize_docker(&mut self, allow: bool) -> Result<()> {
        if allow && !self.record.docker {
            self.record.docker = true;
            self.host_state
                .write_state("installation", &serde_json::to_value(&self.record)?)?;
        }
        Ok(())
    }

    pub fn load_artifact(&self) -> Result<Artifact> {
        verify_artifact_identity(&self.artifact, &self.artifact_sha256)?;
        let artifact = Artifact::load_with_cancellation(
            &self.artifact,
            &self.record.provider,
            &self.cancellation,
        )?;
        ensure!(
            artifact.manifest_sha256() == self.artifact_sha256,
            "provider artifact manifest changed after approval"
        );
        Ok(artifact)
    }

    pub(crate) fn platform_context(&self, host: &Host) -> Result<crate::platform_host::Context> {
        Ok(crate::platform_host::Context {
            root: self.root.clone(),
            workspace: self.workspace_root.clone(),
            state: self.host_state.clone(),
            installation_id: self.record.id.clone(),
            provider: self.record.provider.clone(),
            name: self.record.name.clone(),
            host: host.clone(),
        })
    }

    pub fn provider(&self, operation: Operation, host: &Host) -> Result<Provider> {
        let label = format!("provider {} {operation:?}", self.record.provider);
        let artifact = host.verbose_step(
            &format!(
                "{label}: load and verify artifact at {}",
                self.artifact.display()
            ),
            || self.load_artifact(),
        )?;
        let authorization = host.verbose_step(
            &format!("{label}: configure network, DNS and grants"),
            || {
                let capabilities = artifact
                    .manifest
                    .operations
                    .get(&operation)
                    .context("provider does not support the requested operation")?;
                let parent = self
                    .host_state
                    .root()
                    .parent()
                    .context("installation has no private parent")?;
                let hosts: Vec<&str> = artifact
                    .manifest
                    .network_hosts
                    .iter()
                    .map(String::as_str)
                    .collect();
                let network = if capabilities.contains(&Capability::Network) {
                    ensure!(
                        !hosts.is_empty(),
                        "network provider must declare permitted hosts"
                    );
                    Some(NetworkPolicy::AllowList(AllowList::from_hosts(&hosts)?))
                } else {
                    None
                };
                let authorization = Authorization {
                    workspace: Some(self.source.clone()),
                    scaffold: matches!(operation, Operation::Init | Operation::Install),
                    state: Some(self.provider_state.clone()),
                    network,
                    resolv_conf: if capabilities.contains(&Capability::Network) {
                        Some(provider_resolver_config(
                            Path::new("/etc/resolv.conf"),
                            Path::new("/run/systemd/resolve/resolv.conf"),
                        )?)
                    } else {
                        None
                    },
                    auth_cache: if capabilities.contains(&Capability::AuthCache) {
                        Some(Arc::new(Directory::private(parent.join("auth"))?))
                    } else {
                        None
                    },
                    docker: if capabilities.contains(&Capability::Docker) {
                        ensure!(
                            self.record.docker,
                            "provider Docker access is not authorized; grant it with --allow-docker"
                        );
                        Some(Arc::new(
                            crate::docker_bridge::DockerBridge::from_environment()?,
                        ))
                    } else {
                        None
                    },
                };
                Ok(authorization)
            },
        )?;
        Provider::boot_with_host(
            artifact,
            operation,
            authorization,
            Duration::from_secs(60),
            host,
        )
    }

    pub fn invoke_with(
        &self,
        provider: &mut Provider,
        operation: Operation,
        config: Value,
        host: &Host,
    ) -> Result<Value> {
        self.invoke_with_timeout(provider, operation, config, host, Duration::from_secs(7200))
    }

    pub fn invoke_with_timeout(
        &self,
        provider: &mut Provider,
        operation: Operation,
        config: Value,
        host: &Host,
        timeout: Duration,
    ) -> Result<Value> {
        let output = provider.invoke(
            &Request {
                protocol_version: 1,
                operation,
                installation_id: self.record.id.clone(),
                config,
            },
            timeout,
        )?;
        if host.verbose {
            eprint!("{}{}", output.stdout, output.stderr);
        }

        ensure!(
            !output.diagnostics_truncated,
            "provider diagnostics exceeded their transport limit"
        );
        Ok(output.value)
    }

    pub fn invoke(&self, operation: Operation, config: Value, host: &Host) -> Result<Value> {
        let mut provider = self.provider(operation, host)?;
        self.invoke_with(&mut provider, operation, config, host)
    }
}

fn usable_resolver_config(configuration: &str) -> Option<String> {
    let mut found = false;
    let mut lines = Vec::new();
    for line in configuration.lines() {
        let mut fields = line
            .split(['#', ';'])
            .next()
            .unwrap_or("")
            .split_whitespace();
        if fields.next() == Some("nameserver") {
            let Some(address) = fields
                .next()
                .and_then(|value| value.parse::<std::net::IpAddr>().ok())
            else {
                continue;
            };
            let address = match address {
                std::net::IpAddr::V6(address) => address
                    .to_ipv4_mapped()
                    .map_or(std::net::IpAddr::V6(address), std::net::IpAddr::V4),
                address => address,
            };
            let unusable = match address {
                std::net::IpAddr::V4(address) => {
                    address.is_loopback()
                        || address.is_unspecified()
                        || address.is_multicast()
                        || address.is_link_local()
                        || address == std::net::Ipv4Addr::new(168, 63, 129, 16)
                }
                std::net::IpAddr::V6(address) => {
                    address.is_loopback()
                        || address.is_unspecified()
                        || address.is_multicast()
                        || address.is_unicast_link_local()
                }
            };
            if unusable {
                continue;
            }
            found = true;
            lines.push(format!("nameserver {address}"));
            continue;
        }
        lines.push(line.to_owned());
    }
    found.then(|| format!("{}\n", lines.join("\n")))
}

fn provider_resolver_config(host: &Path, upstream: &Path) -> Result<String> {
    let configuration = std::fs::read_to_string(host).context("read provider DNS configuration")?;
    if let Some(configuration) = usable_resolver_config(&configuration) {
        return Ok(configuration);
    }
    let upstream_configuration = std::fs::read_to_string(upstream).with_context(|| {
        format!(
            "provider DNS has only unusable nameservers and cannot read {}; configure your \
             organization's non-loopback DNS servers in /etc/resolv.conf or {}",
            upstream.display(),
            upstream.display()
        )
    })?;
    usable_resolver_config(&upstream_configuration).context(
        "provider DNS has no usable upstream nameserver; loopback DNS stubs cannot be reached \
         from the guest. Configure your organization's non-loopback DNS servers in \
         /etc/resolv.conf or /run/systemd/resolve/resolv.conf",
    )
}

pub fn forget_auth(provider: &str, name: &str) -> Result<()> {
    crate::project::validate_name(provider)?;
    crate::project::validate_name(name)?;
    let root = crate::platform::home_dir()?
        .join("installations")
        .join(provider)
        .join(name);
    if !root.try_exists()? {
        return Ok(());
    }
    let installation_root = Directory::private(&root)?;
    let directory = Directory::private(installation_root.root().join("host"))?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(directory.root().join("installation.lock"))?;
    lock.try_lock()
        .context("another command is using this installation")?;
    let path = installation_root.root().join("auth");
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) => {
            ensure!(metadata.is_dir(), "authentication cache must not be a link");
            std::fs::remove_dir_all(path)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error).context("inspect provider authentication cache"),
    }
    Ok(())
}

#[cfg(test)]
mod resolver_tests {
    use super::*;
    use crate::artifact::{FileDigest, Manifest};
    use std::collections::{BTreeMap, BTreeSet};

    fn test_artifact(provider: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let rootfs = b"rootfs";
        let program = b"print('provider')";
        let digest = |path: &str, bytes: &[u8]| FileDigest {
            path: path.into(),
            sha256: Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        };
        let manifest = Manifest {
            protocol_version: 1,
            id: provider.into(),
            version: "1".into(),
            runtime_version: crate::assets::VERSION.into(),
            memory_mib: 256,
            rootfs: digest("rootfs.cpio", rootfs),
            kernel: None,
            cold_compatibility: None,
            program: digest("provider.py", program),
            snapshot_index: None,
            snapshot_repository: None,
            environment: BTreeMap::new(),
            network_hosts: Vec::new(),
            source_kinds: BTreeSet::new(),
            operations: BTreeMap::from([(Operation::Describe, BTreeSet::new())]),
        };
        std::fs::write(directory.path().join("rootfs.cpio"), rootfs).unwrap();
        std::fs::write(directory.path().join("provider.py"), program).unwrap();
        std::fs::write(
            directory.path().join("manifest.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        directory
    }

    #[test]
    fn missing_default_artifact_names_provider_path_and_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let error = default_artifact(directory.path(), "azure")
            .unwrap_err()
            .to_string();
        assert!(error.contains("could not locate azure provider artifact"));
        let expected_path = directory.path().join("providers/azure/build/rootfs");
        assert!(error.contains(&expected_path.display().to_string()));
        assert!(!error.contains("just azure::build"));
        assert!(error.contains("--provider-artifact"));
    }

    #[test]
    fn provider_artifact_identity_detects_manifest_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let manifest = directory.path().join("manifest.json");
        std::fs::write(&manifest, b"trusted").unwrap();
        let digest = artifact_digest(directory.path()).unwrap();
        verify_artifact_identity(directory.path(), &digest).unwrap();
        std::fs::write(manifest, b"replacement").unwrap();
        assert!(
            verify_artifact_identity(directory.path(), &digest)
                .unwrap_err()
                .to_string()
                .contains("changed after approval")
        );
    }

    #[test]
    fn explicit_provider_artifact_approval_updates_persisted_identity() {
        let first = test_artifact("test");
        let second = test_artifact("test");
        let mut record = Record {
            schema: 2,
            id: "a".repeat(32),
            provider: "test".into(),
            name: "example".into(),
            infrastructure: PathBuf::from("/workspace"),
            artifact: Some(first.path().to_owned()),
            artifact_sha256: Some(artifact_digest(first.path()).unwrap()),
            docker: true,
        };
        assert!(
            approve_explicit_artifact(
                &mut record,
                second.path(),
                &crate::runtime::Cancellation::default()
            )
            .unwrap()
        );
        assert_eq!(record.artifact.as_deref(), Some(second.path()));
        let expected = artifact_digest(second.path()).unwrap();
        assert_eq!(record.artifact_sha256.as_deref(), Some(expected.as_str()));
        assert!(
            !approve_explicit_artifact(
                &mut record,
                second.path(),
                &crate::runtime::Cancellation::default()
            )
            .unwrap()
        );
    }

    #[test]
    fn approved_digest_is_checked_against_the_private_artifact_snapshot() {
        let source = test_artifact("test");
        let approved = artifact_digest(source.path()).unwrap();
        let artifact = Artifact::load(source.path(), "test").unwrap();
        std::fs::write(source.path().join("manifest.json"), b"replacement").unwrap();
        verify_artifact_identity(artifact.directory(), &approved).unwrap();
        assert!(verify_artifact_identity(source.path(), &approved).is_err());
    }

    #[test]
    fn stub_resolvers_use_configured_upstream_and_preserve_search_policy() -> Result<()> {
        let directory = tempfile::tempdir_in(".")?;
        let host = directory.path().join("stub");
        let upstream = directory.path().join("upstream");
        std::fs::write(&host, "nameserver 127.0.0.53\n")?;
        std::fs::write(
            &upstream,
            "search corp.example\nnameserver 10.20.30.40\noptions edns0\n",
        )?;
        assert_eq!(
            provider_resolver_config(&host, &upstream)?,
            "search corp.example\nnameserver 10.20.30.40\noptions edns0\n"
        );
        std::fs::write(
            &host,
            "nameserver ::1\nnameserver 10.1.2.3 # corporate DNS\n",
        )?;
        assert_eq!(
            provider_resolver_config(&host, &upstream)?,
            "nameserver 10.1.2.3\n"
        );
        std::fs::write(
            &host,
            concat!(
                "nameserver ::ffff:127.0.0.1\n",
                "nameserver fe80::1\n",
                "nameserver 169.254.169.254\n",
                "nameserver 168.63.129.16\n",
                "nameserver ::ffff:10.1.2.3 # normalized\n",
            ),
        )?;
        assert_eq!(
            provider_resolver_config(&host, &upstream)?,
            "nameserver 10.1.2.3\n"
        );
        Ok(())
    }

    #[test]
    fn unusable_resolvers_fail_without_substituting_public_dns() -> Result<()> {
        let directory = tempfile::tempdir_in(".")?;
        let host = directory.path().join("stub");
        let upstream = directory.path().join("upstream");
        std::fs::write(&host, "nameserver 127.0.0.1\n")?;
        for contents in [
            "nameserver ::1\nnameserver 0.0.0.0\n",
            "# no DNS configured\n",
            "nameserver not-an-address\n",
        ] {
            std::fs::write(&upstream, contents)?;
            let error = provider_resolver_config(&host, &upstream).unwrap_err();
            assert!(error.to_string().contains("Configure your organization's"));
        }
        std::fs::remove_file(&upstream)?;
        assert!(
            provider_resolver_config(&host, &upstream)
                .unwrap_err()
                .to_string()
                .contains("configure your organization's")
        );
        Ok(())
    }
}
