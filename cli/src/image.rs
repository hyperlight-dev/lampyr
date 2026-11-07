// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;
use std::fs::{File, Metadata as FileMetadata, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::assets;
use crate::project::{Project, validate_name};

pub const SCHEMA: &str = "lampyr.dev/artifact/v1alpha2";
pub const TARGET: &str = "linux-amd64-kvm";

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Metadata {
    pub schema_version: String,
    pub revision_id: String,
    pub project: Project,
    pub target: Target,
    pub source: Source,
    pub runtime: BTreeMap<String, String>,
    pub artifacts: BTreeMap<String, String>,
    pub provenance: Provenance,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Target {
    pub os: String,
    pub arch: String,
    pub hypervisor: String,
    pub compatibility_class: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub digest: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Provenance {
    pub build_type: String,
    pub builder: String,
}

impl Metadata {
    pub fn create(
        project: Project,
        source: String,
        artifacts: BTreeMap<String, String>,
        vendor: String,
    ) -> Result<Self> {
        let mut metadata = Self {
            schema_version: SCHEMA.to_owned(),
            revision_id: String::new(),
            project,
            target: Target {
                os: "linux".to_owned(),
                arch: "amd64".to_owned(),
                hypervisor: "kvm".to_owned(),
                compatibility_class: TARGET.to_owned(),
            },
            source: Source { digest: source },
            runtime: BTreeMap::from([
                (
                    "hyperlightUnikraft".to_owned(),
                    assets::RELEASE_COMMIT.to_owned(),
                ),
                ("unikraft".to_owned(), assets::UNIKRAFT_COMMIT.to_owned()),
                ("hyperlightHost".to_owned(), assets::VERSION.to_owned()),
                (
                    "hyperlightBinary".to_owned(),
                    format!("sha256:{}", assets::linux_binary_hash()),
                ),
                ("cpuVendor".to_owned(), vendor),
            ]),
            artifacts,
            provenance: Provenance {
                build_type: "https://lampyr.dev/build/snapshot/v1".to_owned(),
                builder: "ly".to_owned(),
            },
        };
        metadata.revision_id = metadata.revision_id()?;
        Ok(metadata)
    }

    /// Adds a runtime fact and recomputes the Revision ID over it, so two
    /// artifacts that differ only in that fact are different Revisions.
    pub fn record_runtime(&mut self, name: &str, value: String) -> Result<()> {
        self.runtime.insert(name.to_owned(), value);
        self.revision_id = self.revision_id()?;
        Ok(())
    }

    fn revision_id(&self) -> Result<String> {
        #[derive(Serialize)]
        struct Input<'a> {
            project: &'a Project,
            target: &'a Target,
            source: &'a str,
            runtime: &'a BTreeMap<String, String>,
            artifacts: BTreeMap<&'static str, &'a str>,
        }
        let input = Input {
            project: &self.project,
            target: &self.target,
            source: &self.source.digest,
            runtime: &self.runtime,
            artifacts: BTreeMap::from([
                ("kernel", self.digest("kernel")?),
                ("rootfs", self.digest("rootfs")?),
            ]),
        };
        // Revision identity is also verified by the unchanged Go controllers.
        let json = protocol_json(&input)?;
        Ok(hex(Sha256::digest(json.as_bytes()).as_slice()))
    }

    pub fn digest(&self, name: &str) -> Result<&str> {
        let digest = self
            .artifacts
            .get(name)
            .with_context(|| format!("artifact digest {name:?} is missing"))?;
        ensure!(
            is_sha256_digest(digest),
            "invalid artifact digest for {name}"
        );
        Ok(digest)
    }

    pub fn local_image(&self, directory: &Path) -> Result<String> {
        validate_name(&self.project.name)?;
        let digest = file_digest(&directory.join("metadata.json"))?;
        Ok(format!(
            "lampyr.local/{}:{}",
            self.project.name,
            digest.trim_start_matches("sha256:")
        ))
    }
}

pub fn verify(directory: &Path) -> Result<Metadata> {
    let metadata: Metadata =
        serde_json::from_reader(open_regular(&directory.join("metadata.json"))?.take(1024 * 1024))
            .context("decode artifact metadata")?;
    ensure!(
        metadata.schema_version == SCHEMA,
        "unsupported artifact schema"
    );
    metadata.project.validate()?;
    ensure!(
        metadata.target.os == "linux"
            && metadata.target.arch == "amd64"
            && metadata.target.hypervisor == "kvm"
            && metadata.target.compatibility_class == format!("linux-{}-kvm", metadata.target.arch),
        "unsupported snapshot target"
    );
    ensure!(
        !metadata.runtime.contains_key("localSourceTree"),
        "custom source-runtime artifacts are no longer supported; rebuild with the released runtime"
    );
    for (name, actual) in [
        ("kernel", file_digest(&directory.join("kernel"))?),
        (
            "snapshot",
            directory_digest(&directory.join("snapshot"), &[])?,
        ),
    ] {
        ensure!(
            actual == metadata.digest(name)?,
            "{name} digest does not match metadata"
        );
    }
    ensure!(
        metadata.revision_id == metadata.revision_id()?,
        "revision ID does not match metadata"
    );
    Ok(metadata)
}

pub fn snapshot_vendor(snapshot: &Path) -> Result<String> {
    #[derive(Deserialize)]
    struct Index {
        manifests: Vec<Manifest>,
    }
    #[derive(Deserialize)]
    struct Manifest {
        #[serde(default)]
        annotations: BTreeMap<String, String>,
    }
    let index: Index =
        serde_json::from_reader(open_regular(&snapshot.join("index.json"))?.take(1024 * 1024))
            .context("read snapshot index")?;
    index
        .manifests
        .into_iter()
        .find_map(|manifest| {
            manifest
                .annotations
                .get("dev.hyperlight.snapshot.cpu.vendor")
                .map(|vendor| vendor.trim().to_owned())
                .filter(|vendor| !vendor.is_empty())
        })
        .context("snapshot CPU vendor annotation is missing")
}

pub fn file_digest(path: &Path) -> Result<String> {
    let mut file = open_regular(path)?;
    let mut hash = Sha256::new();
    hash_reader(&mut file, &mut hash)?;
    Ok(format!("sha256:{}", hex(hash.finalize().as_slice())))
}

pub fn directory_digest(root: &Path, ignored: &[PathBuf]) -> Result<String> {
    ensure!(
        std::fs::symlink_metadata(root)?.is_dir(),
        "digest root must be a real directory"
    );
    let mut entries = Vec::new();
    collect(root, Path::new(""), ignored, &mut entries)?;
    entries.sort_by(|left, right| left.as_os_str().cmp(right.as_os_str()));
    let mut hash = Sha256::new();
    for relative in entries {
        let path = root.join(&relative);
        let metadata = std::fs::symlink_metadata(&path)?;
        let name = relative
            .to_str()
            .context("artifact paths must be UTF-8")?
            .replace(std::path::MAIN_SEPARATOR, "/");
        if metadata.is_symlink() {
            let target = std::fs::read_link(&path)?;
            let bytes = target.as_os_str().as_encoded_bytes();
            hash.update(format!("{name}\0{:o}\0{}\0", go_mode(&metadata), bytes.len()).as_bytes());
            hash.update(bytes);
        } else {
            let mut file = open_regular(&path)?;
            let metadata = file.metadata()?;
            hash.update(
                format!("{name}\0{:o}\0{}\0", go_mode(&metadata), metadata.len()).as_bytes(),
            );
            let count = hash_reader(&mut file, &mut hash)?;
            ensure!(
                count == metadata.len(),
                "file changed while hashing {}",
                path.display()
            );
        }
    }
    Ok(format!("sha256:{}", hex(hash.finalize().as_slice())))
}

fn collect(
    root: &Path,
    relative: &Path,
    ignored: &[PathBuf],
    entries: &mut Vec<PathBuf>,
) -> Result<()> {
    for entry in std::fs::read_dir(root.join(relative))? {
        let entry = entry?;
        let relative = relative.join(entry.file_name());
        if ignored.iter().any(|ignore| relative.starts_with(ignore)) {
            continue;
        }
        let kind = entry.file_type()?;
        if kind.is_dir() {
            collect(root, &relative, ignored, entries)?;
        } else if kind.is_file() || kind.is_symlink() {
            entries.push(relative);
        } else {
            anyhow::bail!("cannot hash special file {}", relative.display());
        }
    }
    Ok(())
}

pub(crate) fn open_regular(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options
        .open(path)
        .with_context(|| format!("read {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "{} must be a regular file",
        path.display()
    );
    Ok(file)
}

fn hash_reader(reader: &mut impl Read, hash: &mut Sha256) -> Result<u64> {
    let mut buffer = [0_u8; 64 * 1024];
    let mut count = 0;
    loop {
        let size = reader.read(&mut buffer)?;
        if size == 0 {
            return Ok(count);
        }
        hash.update(&buffer[..size]);
        count += size as u64;
    }
}

#[cfg(unix)]
fn go_mode(metadata: &FileMetadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode();
    (mode & 0o777)
        | if mode & 0o4000 != 0 { 1 << 23 } else { 0 }
        | if mode & 0o2000 != 0 { 1 << 22 } else { 0 }
        | if mode & 0o1000 != 0 { 1 << 20 } else { 0 }
        | if metadata.is_symlink() { 1 << 27 } else { 0 }
}

#[cfg(not(unix))]
fn go_mode(metadata: &FileMetadata) -> u32 {
    if metadata.is_symlink() {
        (1 << 27) | 0o777
    } else if metadata.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

pub(crate) fn is_sha256_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

pub(crate) fn protocol_json(value: &impl Serialize) -> Result<String> {
    Ok(serde_json::to_string(value)?
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_revision_matches_go_fixture() {
        let metadata: Metadata =
            serde_json::from_str(include_str!("../../tests/artifacts/standalone.json")).unwrap();
        metadata.project.validate().unwrap();
        assert_eq!(metadata.revision_id().unwrap(), metadata.revision_id);
    }
}
