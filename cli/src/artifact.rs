// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use cap_std::ambient_authority;
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::runtime::PAYLOAD_LIMIT;
use crate::workspace::relative_path;

mod variants;
pub(crate) mod verified;
pub(crate) use variants::MEMORY;
pub use variants::{HostCompatibility, OciDescriptor, SnapshotIndex, SnapshotManifest};

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum Operation {
    Describe,
    Init,
    Apply,
    Inspect,
    Connect,
    Delete,
    Install,
    Health,
    Uninstall,
    #[serde(rename = "source-create")]
    SourceCreate,
    #[serde(rename = "source-status")]
    SourceStatus,
    #[serde(rename = "source-delete")]
    SourceDelete,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    WorkspaceRead,
    WorkspaceScaffold,
    State,
    Network,
    AuthCache,
    Docker,
    ProviderManifests,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FileDigest {
    pub path: String,
    pub sha256: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ColdCompatibility {
    pub os: String,
    pub architecture: String,
}

impl ColdCompatibility {
    fn current() -> Self {
        Self {
            os: std::env::consts::OS.to_owned(),
            architecture: std::env::consts::ARCH.to_owned(),
        }
    }

    fn validate(&self) -> Result<()> {
        for (name, value) in [
            ("OS", self.os.as_str()),
            ("architecture", self.architecture.as_str()),
        ] {
            ensure!(
                !value.is_empty()
                    && value.len() <= 32
                    && value.bytes().all(|byte| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'),
                "provider cold-boot {name} is invalid"
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SourceKind {
    pub api_version: String,
    pub kind: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Manifest {
    pub protocol_version: u32,
    pub id: String,
    pub version: String,
    pub runtime_version: String,
    #[serde(rename = "memoryMiB")]
    pub memory_mib: u32,
    pub rootfs: FileDigest,
    pub kernel: Option<FileDigest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cold_compatibility: Option<ColdCompatibility>,
    pub program: FileDigest,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_index: Option<FileDigest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snapshot_repository: Option<String>,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub network_hosts: Vec<String>,
    #[serde(default)]
    pub source_kinds: BTreeSet<SourceKind>,
    pub operations: BTreeMap<Operation, BTreeSet<Capability>>,
}

pub struct Artifact {
    pub manifest: Manifest,
    pub program: String,
    pub snapshot: Option<PathBuf>,
    pub snapshot_digest: Option<String>,
    source: PathBuf,
    files: tempfile::TempDir,
    manifest_sha256: String,
}

#[derive(Debug)]
pub struct BootFiles {
    pub rootfs: PathBuf,
    pub kernel: Option<PathBuf>,
}

impl Artifact {
    pub fn ensure_cold_compatible(&self) -> Result<()> {
        if let Some(compatibility) = &self.manifest.cold_compatibility {
            ensure!(
                compatibility == &ColdCompatibility::current(),
                "provider {} cold-boot artifact targets {}/{}, but this host is {}/{}",
                self.manifest.id,
                compatibility.os,
                compatibility.architecture,
                std::env::consts::OS,
                std::env::consts::ARCH
            );
        }
        Ok(())
    }

    /// Native layout files must remain immutable until the snapshot and its sandboxes are dropped.
    pub fn load_snapshot(&self) -> Result<std::sync::Arc<hyperlight_unikraft::Snapshot>> {
        let layout = self
            .snapshot
            .as_ref()
            .context("provider artifact has no startup snapshot")?;
        let digest = self
            .snapshot_digest
            .as_ref()
            .context("provider startup snapshot has no OCI digest")?;
        let reference = digest
            .parse::<hyperlight_host::sandbox::snapshot::OciDigest>()
            .context("parse provider snapshot digest")?;
        let snapshot = hyperlight_unikraft::Snapshot::checked_load(layout, reference)
            .context("load provider runtime snapshot")?;
        Ok(std::sync::Arc::new(snapshot))
    }

    pub fn load(path: impl AsRef<Path>, expected_id: &str) -> Result<Self> {
        Self::load_for_host(path, expected_id, &HostCompatibility::current())
    }

    pub fn load_with_cancellation(
        path: impl AsRef<Path>,
        expected_id: &str,
        cancellation: &crate::runtime::Cancellation,
    ) -> Result<Self> {
        let registry = crate::provider_registry::SnapshotRegistry::from_env()?
            .map(|registry| registry.with_cancellation(cancellation.clone()));
        Self::load_for_host_with_registry_and_cancellation(
            path,
            expected_id,
            &HostCompatibility::current(),
            registry.as_ref(),
            Some(cancellation),
        )
    }

    pub fn load_for_host(
        path: impl AsRef<Path>,
        expected_id: &str,
        host: &HostCompatibility,
    ) -> Result<Self> {
        let registry = crate::provider_registry::SnapshotRegistry::from_env()?;
        Self::load_for_host_with_registry_and_cancellation(
            path,
            expected_id,
            host,
            registry.as_ref(),
            None,
        )
    }

    pub fn load_for_host_with_registry(
        path: impl AsRef<Path>,
        expected_id: &str,
        host: &HostCompatibility,
        registry: Option<&crate::provider_registry::SnapshotRegistry>,
    ) -> Result<Self> {
        Self::load_for_host_with_registry_and_cancellation(path, expected_id, host, registry, None)
    }

    fn load_for_host_with_registry_and_cancellation(
        path: impl AsRef<Path>,
        expected_id: &str,
        host: &HostCompatibility,
        registry: Option<&crate::provider_registry::SnapshotRegistry>,
        cancellation: Option<&crate::runtime::Cancellation>,
    ) -> Result<Self> {
        let requested = path.as_ref();
        ensure_real_artifact_root(requested)?;
        let artifact_path = requested
            .canonicalize()
            .context("resolve provider artifact")?;
        ensure_stable_artifact_ancestors(&artifact_path)?;
        ensure_secure_artifact_root(&artifact_path)?;
        let directory = Dir::open_ambient_dir(&artifact_path, ambient_authority())
            .context("open provider artifact")?;
        let mut bytes = Vec::new();
        directory
            .open("manifest.json")?
            .take((PAYLOAD_LIMIT + 1) as u64)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= PAYLOAD_LIMIT,
            "provider manifest exceeds 64 KiB"
        );
        let manifest: Manifest =
            serde_json::from_slice(&bytes).context("decode provider artifact manifest")?;
        ensure!(
            manifest.protocol_version == 1,
            "unsupported provider protocol version"
        );
        ensure!(
            manifest.runtime_version == crate::assets::VERSION,
            "provider artifact targets Hyperlight runtime {}, but this ly needs {}; rebuild the provider artifact or reinstall the platform",
            manifest.runtime_version,
            crate::assets::VERSION
        );
        if let Some(compatibility) = &manifest.cold_compatibility {
            compatibility.validate()?;
        }
        ensure!(
            manifest.snapshot_repository.is_none() || manifest.snapshot_index.is_some(),
            "provider snapshot repository requires a snapshot index"
        );
        let default_registry = manifest
            .snapshot_repository
            .as_deref()
            .map(|repository| {
                let registry = crate::provider_registry::SnapshotRegistry::new(
                    repository,
                    crate::provider_registry::SnapshotRegistry::default_cache()?,
                )?;
                Ok::<_, anyhow::Error>(match cancellation {
                    Some(cancellation) => registry.with_cancellation(cancellation.clone()),
                    None => registry,
                })
            })
            .transpose()?;
        let default_registry_selected = registry.is_none() && default_registry.is_some();
        let registry = registry.or(default_registry.as_ref());
        ensure!(
            manifest.id == expected_id,
            "provider identity does not match requested provider"
        );
        ensure!(
            !manifest.id.is_empty()
                && manifest.id.len() <= 64
                && manifest
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "invalid provider identity"
        );
        ensure!(
            !manifest.version.is_empty(),
            "provider artifact version is missing"
        );
        ensure!(
            (128..=4096).contains(&manifest.memory_mib),
            "provider memory must be 128 to 4096 MiB"
        );
        ensure!(
            manifest.operations.contains_key(&Operation::Describe),
            "provider must support describe"
        );
        ensure!(
            manifest.operations[&Operation::Describe].is_empty(),
            "describe must not require capabilities"
        );
        for operation in manifest.operations.keys() {
            ensure!(
                matches!(
                    operation,
                    Operation::Describe
                        | Operation::Init
                        | Operation::Apply
                        | Operation::Inspect
                        | Operation::Connect
                        | Operation::Delete
                        | Operation::SourceCreate
                        | Operation::SourceStatus
                        | Operation::SourceDelete
                ),
                "providers may expose only infrastructure and event source operations"
            );
        }
        for (operation, capabilities) in &manifest.operations {
            if capabilities.contains(&Capability::ProviderManifests) {
                ensure!(
                    *operation == Operation::Apply,
                    "provider-manifests capability is valid only for apply"
                );
            }
        }
        ensure!(
            manifest.environment.len() <= 32,
            "too many provider environment variables"
        );
        for (name, value) in &manifest.environment {
            ensure!(
                !name.is_empty()
                    && name.len() <= 128
                    && value.len() <= 4096
                    && !value.contains('\0')
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "invalid provider environment variable"
            );
        }
        ensure!(
            manifest.network_hosts.len() <= 64,
            "too many provider network hosts"
        );
        for host in &manifest.network_hosts {
            ensure!(
                host.len() <= 253
                    && host
                        .split('.')
                        .all(|label| crate::project::validate_name(label).is_ok()),
                "invalid provider network hostname"
            );
        }
        ensure!(
            manifest.source_kinds.len() <= 32,
            "too many provider Event Source kinds"
        );
        for resource in &manifest.source_kinds {
            Self::validate_source_kind(resource)?;
        }
        let source_operations = [
            Operation::SourceCreate,
            Operation::SourceStatus,
            Operation::SourceDelete,
        ];
        ensure!(
            !source_operations
                .iter()
                .any(|operation| manifest.operations.contains_key(operation))
                || !manifest.source_kinds.is_empty(),
            "Event Source providers must declare sourceKinds"
        );
        validate_installed_file(&directory, &manifest.rootfs, &artifact_path, 2 << 30)?;
        if let Some(kernel) = &manifest.kernel {
            validate_installed_file(&directory, kernel, &artifact_path, 64 << 20)?;
        }
        let (_, program_file) = verify_installed_file(
            &directory,
            &manifest.program,
            &artifact_path,
            PAYLOAD_LIMIT as u64,
        )?;
        let program = read_verified_program(program_file, &manifest.program.sha256)?;
        let (native_snapshot, snapshot_digest) = if let Some(index) = &manifest.snapshot_index {
            relative_path(&index.path)?;
            ensure!(
                Path::new(&index.path)
                    .file_name()
                    .is_some_and(|name| name == "index.json"),
                "provider snapshot index must name a native OCI index.json"
            );
            let selected = match directory.symlink_metadata(&index.path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => match registry {
                    // No variant can match a host without a detected hypervisor or CPU vendor, so
                    // don't authenticate to the registry or fetch the index just to cold-boot.
                    Some(_) if host.hypervisor.is_none() || host.cpu_vendor.is_none() => {
                        warn_no_snapshot_variant(&manifest.id, host);
                        None
                    }
                    Some(registry) => {
                        match registry.pull_selected(&format!("sha256:{}", index.sha256), host) {
                            Ok(Some(layout)) if layout.digest.is_none() => {
                                warn_no_snapshot_variant(&manifest.id, host);
                                None
                            }
                            Ok(layout) => layout
                                .and_then(|layout| layout.digest.map(|digest| (layout.path, digest))),
                            Err(error)
                                if default_registry_selected
                                    && crate::provider_registry::SnapshotRegistry::is_unavailable(
                                        &error,
                                    ) =>
                            {
                                eprintln!(
                                    "provider {}: published snapshot is unavailable ({error}); cold-booting",
                                    manifest.id
                                );
                                None
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    // A registry-only artifact keeps its cold-boot assets, so without a registry it
                    // boots like a host with no compatible variant instead of failing.
                    None => {
                        static WARNED: std::sync::Once = std::sync::Once::new();
                        WARNED.call_once(|| {
                            eprintln!(
                                "provider {}: snapshot index is not in the artifact and LAMPYR_SNAPSHOT_REGISTRY is not set; cold-booting",
                                manifest.id
                            )
                        });
                        None
                    }
                },
                _ => {
                    let selected =
                        variants::select_layout(&directory, index, host, &artifact_path)?;
                    if selected.is_none() {
                        warn_no_snapshot_variant(&manifest.id, host);
                    }
                    selected
                }
            };
            match selected {
                Some((layout, digest)) => (Some(layout), Some(digest)),
                None => (None, None),
            }
        } else {
            (None, None)
        };
        // tempfile creates directories with the process umask, which leaves
        // them group-writable under umask 002.
        let files = private_tempdir().context("create private provider directory")?;
        std::fs::write(files.path().join("manifest.json"), &bytes)
            .context("write verified provider manifest")?;
        Ok(Self {
            manifest,
            program,
            snapshot: native_snapshot,
            snapshot_digest,
            source: artifact_path,
            files,
            manifest_sha256: Sha256::digest(&bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
        })
    }

    fn validate_source_kind(resource: &SourceKind) -> Result<()> {
        let Some((group, version)) = resource.api_version.split_once('/') else {
            anyhow::bail!("provider Event Source kinds cannot use the core Kubernetes API");
        };
        let safe_knative = group == "eventing.knative.dev"
            && matches!(resource.kind.as_str(), "Broker" | "Trigger");
        let provider_owned = group.contains('.')
            && !group.ends_with(".k8s.io")
            && !group.ends_with(".kubernetes.io")
            && !group.ends_with(".knative.dev");
        ensure!(
            (safe_knative || provider_owned)
                && !version.is_empty()
                && version.bytes().all(|byte| byte.is_ascii_alphanumeric()),
            "provider Event Source kind is not safe"
        );
        ensure!(
            resource
                .kind
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphabetic())
                && resource
                    .kind
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric()),
            "invalid provider Event Source kind"
        );
        Ok(())
    }

    pub fn directory(&self) -> &Path {
        self.files.path()
    }

    pub fn source(&self) -> &Path {
        &self.source
    }

    /// Hash cold-boot files immediately before boot, then use those installed paths directly.
    pub fn verified_boot_files(&self) -> Result<BootFiles> {
        let directory = Dir::open_ambient_dir(&self.source, ambient_authority())
            .context("open provider artifact")?;
        let (rootfs, _) =
            verify_installed_file(&directory, &self.manifest.rootfs, &self.source, 2 << 30)?;
        let kernel = self
            .manifest
            .kernel
            .as_ref()
            .map(|file| {
                verify_installed_file(&directory, file, &self.source, 64 << 20)
                    .map(|(path, _)| path)
            })
            .transpose()?;
        Ok(BootFiles { rootfs, kernel })
    }

    pub fn manifest_sha256(&self) -> &str {
        &self.manifest_sha256
    }
}

fn private_tempdir() -> std::io::Result<tempfile::TempDir> {
    let mut builder = tempfile::Builder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        builder.permissions(std::fs::Permissions::from_mode(0o700));
    }
    builder.tempdir()
}

fn ensure_secure_artifact_root(path: &Path) -> Result<()> {
    ensure_real_artifact_root(path)?;
    let metadata = std::fs::symlink_metadata(path).context("inspect provider artifact")?;
    ensure_owner_controlled(path, &metadata, "provider artifact directory")?;
    let mut entries = 0;
    ensure_secure_artifact_directory(path, 0, &mut entries)
}

fn ensure_real_artifact_root(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).context("inspect provider artifact")?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "provider artifact directory must not be a symlink"
    );
    Ok(())
}

/// Checks that no other user can replace `path` through one of its ancestors.
pub(crate) fn ensure_stable_artifact_ancestors(path: &Path) -> Result<()> {
    ensure_stable_ancestors(
        path,
        "provider artifact",
        " or keep the checkout in a directory only you can write",
    )
}

/// [`ensure_stable_artifact_ancestors`] for the platform installation.
pub(crate) fn ensure_stable_platform_ancestors(path: &Path) -> Result<()> {
    ensure_stable_ancestors(path, "platform directory", "")
}

#[cfg(unix)]
fn ensure_stable_ancestors(path: &Path, subject: &str, hint: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    const STICKY_BIT: u32 = 0o1000;
    let euid = libc_euid();
    let path = std::path::absolute(path)?;
    let mut child = path.as_path();
    while let Some(parent) = child.parent() {
        let parent_metadata = std::fs::symlink_metadata(parent)
            .with_context(|| format!("inspect {subject} ancestor {}", parent.display()))?;
        ensure!(
            parent_metadata.is_dir() && !parent_metadata.file_type().is_symlink(),
            "{subject} ancestor {} must be a real directory",
            parent.display()
        );
        ensure!(
            parent_metadata.uid() == euid || parent_metadata.uid() == 0,
            "{subject} ancestor {} must be owned by the current user or root",
            parent.display()
        );
        if let Some(reason) = crate::permissions::others_can_write(parent, &parent_metadata) {
            ensure!(
                parent_metadata.mode() & STICKY_BIT != 0,
                "{subject} ancestor {} must not be writable by other users without the sticky bit: \
                 {reason}; run `chmod go-w {}`{hint}",
                parent.display(),
                parent.display()
            );
            let child_metadata = std::fs::symlink_metadata(child)
                .with_context(|| format!("inspect {subject} path {}", child.display()))?;
            ensure!(
                child_metadata.uid() == euid || child_metadata.uid() == 0,
                "{subject} path {} beneath a sticky ancestor must be owned by the current user or root",
                child.display()
            );
        }
        child = parent;
    }
    Ok(())
}

#[cfg(not(unix))]
fn ensure_stable_ancestors(_path: &Path, _subject: &str, _hint: &str) -> Result<()> {
    Ok(())
}

fn ensure_secure_artifact_directory(path: &Path, depth: usize, entries: &mut usize) -> Result<()> {
    ensure!(
        depth <= 32,
        "provider artifact directories are nested too deeply"
    );
    for entry in std::fs::read_dir(path).with_context(|| format!("inspect {}", path.display()))? {
        let entry = entry?;
        *entries += 1;
        ensure!(
            *entries <= 4096,
            "provider artifact contains too many filesystem entries"
        );
        let entry_path = entry.path();
        let metadata = std::fs::symlink_metadata(&entry_path)
            .with_context(|| format!("inspect {}", entry_path.display()))?;
        ensure!(
            !metadata.file_type().is_symlink() && (metadata.is_dir() || metadata.is_file()),
            "provider artifact entries must be regular files or directories"
        );
        ensure_owner_controlled(&entry_path, &metadata, "provider artifact entry")?;
        if metadata.is_dir() {
            ensure_secure_artifact_directory(&entry_path, depth + 1, entries)?;
        }
    }
    Ok(())
}

fn validate_installed_file(
    directory: &Dir,
    file: &FileDigest,
    artifact_path: &Path,
    limit: u64,
) -> Result<PathBuf> {
    let (path, input) = open_installed_file(directory, file, artifact_path)?;
    ensure!(
        input.metadata()?.len() <= limit,
        "provider artifact file {} exceeds its size limit",
        file.path
    );
    Ok(path)
}

fn verify_installed_file(
    directory: &Dir,
    file: &FileDigest,
    artifact_path: &Path,
    limit: u64,
) -> Result<(PathBuf, cap_std::fs::File)> {
    let (path, mut input) = open_installed_file(directory, file, artifact_path)?;
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(
            total <= limit,
            "provider artifact file {} exceeds its size limit",
            file.path
        );
        digest.update(&buffer[..count]);
    }
    ensure!(
        digest
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            == file.sha256,
        "provider artifact checksum mismatch for {}",
        file.path
    );
    Ok((path, input))
}

fn open_installed_file(
    directory: &Dir,
    file: &FileDigest,
    artifact_path: &Path,
) -> Result<(PathBuf, cap_std::fs::File)> {
    relative_path(&file.path)?;
    ensure!(
        file.sha256.len() == 64
            && file
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid artifact SHA-256"
    );
    let path = artifact_path.join(&file.path);
    let metadata =
        std::fs::symlink_metadata(&path).with_context(|| format!("inspect {}", file.path))?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "provider artifact file must be a regular file"
    );
    ensure_owner_controlled(&path, &metadata, "provider artifact file")?;
    let input = open_artifact_file(directory, &file.path)?;
    Ok((path, input))
}

fn read_verified_program(mut input: cap_std::fs::File, expected_sha256: &str) -> Result<String> {
    input.rewind()?;
    let mut program = String::new();
    input
        .take((PAYLOAD_LIMIT + 1) as u64)
        .read_to_string(&mut program)
        .context("provider program must be UTF-8")?;
    ensure!(
        program.len() <= PAYLOAD_LIMIT,
        "provider program exceeds 64 KiB"
    );
    let digest: String = Sha256::digest(program.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    ensure!(
        digest == expected_sha256,
        "provider program checksum mismatch"
    );
    Ok(program)
}

#[cfg(unix)]
fn ensure_owner_controlled(path: &Path, metadata: &std::fs::Metadata, what: &str) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let shown = std::path::absolute(path).unwrap_or_else(|_| path.to_owned());
    ensure!(
        self::libc_euid() == metadata.uid(),
        "{what} {} must be owned by the current user",
        shown.display()
    );
    if let Some(reason) = crate::permissions::others_can_write(path, metadata) {
        anyhow::bail!(
            "{what} {} must not be writable by other users: {reason}; run `chmod go-w {}`",
            shown.display(),
            shown.display()
        );
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn libc_euid() -> u32 {
    // SAFETY: geteuid has no preconditions and returns the current process's effective user ID.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn ensure_owner_controlled(_path: &Path, _metadata: &std::fs::Metadata, _what: &str) -> Result<()> {
    Ok(())
}

fn open_artifact_file(directory: &Dir, path: &str) -> Result<cap_std::fs::File> {
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let input = directory
        .open_with(path, &options)
        .with_context(|| format!("open provider artifact file {path}"))?;
    ensure!(
        input.metadata()?.is_file(),
        "provider artifact file must be a regular file"
    );
    Ok(input)
}

pub(crate) fn validate_rootfs(path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    loop {
        let mut header = [0; 110];
        file.read_exact(&mut header)
            .context("truncated provider CPIO header")?;
        ensure!(
            &header[..6] == b"070701" || &header[..6] == b"070702",
            "provider rootfs must be a newc CPIO archive"
        );
        let field = |range: std::ops::Range<usize>| -> Result<u64> {
            let text = std::str::from_utf8(&header[range]).context("invalid CPIO header")?;
            ensure!(
                text.bytes().all(|byte| byte.is_ascii_hexdigit()),
                "invalid CPIO header"
            );
            Ok(u64::from_str_radix(text, 16)?)
        };
        let name_size = field(94..102)?;
        let file_size = field(54..62)?;
        ensure!(
            (1..=4096).contains(&name_size),
            "provider CPIO filename exceeds its limit"
        );
        let mut name = vec![0; name_size as usize];
        file.read_exact(&mut name)
            .context("truncated provider CPIO filename")?;
        ensure!(
            name.last() == Some(&0) && !name[..name.len() - 1].contains(&0),
            "invalid provider CPIO filename"
        );
        let position = file.stream_position()?;
        let contents = position.checked_add(3).context("CPIO offset overflow")? & !3;
        let end = contents
            .checked_add(file_size)
            .context("CPIO offset overflow")?;
        let next = end.checked_add(3).context("CPIO offset overflow")? & !3;
        ensure!(next <= size, "truncated provider CPIO contents");
        if &name[..name.len() - 1] == b"TRAILER!!!" {
            ensure!(file_size == 0, "invalid provider CPIO trailer");
            return Ok(());
        }
        file.seek(SeekFrom::Start(next))?;
    }
}

/// Says why a provider is about to cold-boot when its published snapshot index has no variant
/// for this host, for example an Intel host when only AMD KVM snapshots were published; the
/// provider still works, only slower, so this is a warning rather than an error.
fn no_snapshot_variant_message(provider: &str, host: &HostCompatibility) -> String {
    format!(
        "provider {provider}: no published snapshot matches this host ({}/{}/{}); cold-booting, which is slower",
        host.architecture,
        host.hypervisor.as_deref().unwrap_or("no hypervisor"),
        host.cpu_vendor.as_deref().unwrap_or("unknown CPU vendor"),
    )
}

fn warn_no_snapshot_variant(provider: &str, host: &HostCompatibility) {
    static WARNED: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());
    let mut warned = WARNED
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if warned.insert(provider.to_owned()) {
        eprintln!("{}", no_snapshot_variant_message(provider, host));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn no_snapshot_variant_message_names_the_host() {
        let host = super::HostCompatibility {
            architecture: "x86_64".into(),
            hypervisor: Some("kvm".into()),
            cpu_vendor: Some("GenuineIntel".into()),
        };
        assert_eq!(
            super::no_snapshot_variant_message("kind", &host),
            "provider kind: no published snapshot matches this host (x86_64/kvm/GenuineIntel); cold-booting, which is slower"
        );
        let unknown = super::HostCompatibility {
            architecture: "x86_64".into(),
            hypervisor: None,
            cpu_vendor: None,
        };
        assert!(
            super::no_snapshot_variant_message("k3d", &unknown)
                .contains("x86_64/no hypervisor/unknown CPU vendor")
        );
    }

    use super::*;
    use std::io::Write;

    fn archive(name: &[u8], declared_name: u32, declared_size: u32) -> Vec<u8> {
        let mut data = format!("070701{}", "00000000".repeat(13)).into_bytes();
        data[94..102].copy_from_slice(format!("{declared_name:08x}").as_bytes());
        data[54..62].copy_from_slice(format!("{declared_size:08x}").as_bytes());
        data.extend_from_slice(name);
        while !data.len().is_multiple_of(4) {
            data.push(0);
        }
        data
    }

    #[test]
    fn cpio_lengths_are_checked_before_sdk_allocation() {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&archive(b"TRAILER!!!\0", 11, 0)).unwrap();
        validate_rootfs(file.path()).unwrap();
        for bytes in [
            archive(b"", u32::MAX, 0),
            archive(b"", 0, 0),
            archive(b"bad", 4, 0),
            archive(b"bad\0", 4, u32::MAX),
            archive(b"TRAILER!!!\0", 11, 1),
        ] {
            std::fs::write(file.path(), bytes).unwrap();
            assert!(validate_rootfs(file.path()).is_err());
        }
    }
}
