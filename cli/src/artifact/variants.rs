// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use cap_std::fs::Dir;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{FileDigest, Manifest, open_artifact_file};
use crate::runtime::PAYLOAD_LIMIT;

const INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const CONFIG: &str = "application/vnd.hyperlight.snapshot.config.v5+json";
pub(crate) const MEMORY: &str = "application/vnd.hyperlight.snapshot.memory.v2";
const TRANSPORT: &str = "application/vnd.hyperlight.snapshot.transport.v1";
/// hyperlight-host 0.19 writes a snapshot's memory as up to 30 data layers
/// (its MAX_SNAPSHOT_LAYERS), plus one blob of page tables alone when no
/// data layer holds them, followed by one transport layer.
const MAX_MEMORY_LAYERS: usize = 31;
/// What a snapshot's memory layers may total.
const MEMORY_LIMIT: u64 = 4 << 30;
const TRANSPORT_LIMIT: u64 = 2 << 20;
const ARCH: &str = "dev.hyperlight.snapshot.arch";
const HYPERVISOR: &str = "dev.hyperlight.snapshot.hypervisor";
const CPU_VENDOR: &str = "dev.hyperlight.snapshot.cpu.vendor";
const REF_NAME: &str = "org.opencontainers.image.ref.name";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotIndex {
    pub schema_version: u32,
    pub media_type: String,
    pub manifests: Vec<OciDescriptor>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OciDescriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotManifest {
    pub schema_version: u32,
    pub media_type: String,
    pub artifact_type: String,
    pub config: OciDescriptor,
    pub layers: Vec<OciDescriptor>,
}

#[derive(Clone, Debug)]
pub struct HostCompatibility {
    pub architecture: String,
    pub hypervisor: Option<String>,
    pub cpu_vendor: Option<String>,
}

impl HostCompatibility {
    pub fn current() -> Self {
        let (hypervisor, cpu_vendor) = native_compatibility()
            .map(|(hypervisor, vendor)| (Some(hypervisor), Some(vendor)))
            .unwrap_or_default();
        Self {
            architecture: std::env::consts::ARCH.into(),
            hypervisor,
            cpu_vendor,
        }
    }
}

fn native_compatibility() -> Option<(String, String)> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        use std::os::unix::fs::FileTypeExt;

        let kvm = std::fs::metadata("/dev/kvm").ok()?;
        if !kvm.file_type().is_char_device()
            || std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/kvm")
                .is_err()
        {
            return None;
        }
        #[allow(unused_unsafe)] // __cpuid requires unsafe on Rust 1.91.
        let result = unsafe { std::arch::x86_64::__cpuid(0) };
        let bytes = [
            result.ebx.to_le_bytes(),
            result.edx.to_le_bytes(),
            result.ecx.to_le_bytes(),
        ]
        .concat();
        return String::from_utf8(bytes)
            .ok()
            .map(|vendor| ("kvm".to_owned(), vendor));
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        return Some(("hvf".to_owned(), "0x61".to_owned()));
    }
    #[allow(unreachable_code)]
    None
}

impl SnapshotIndex {
    /// Prefetch has no provider manifest, but still requires complete input bindings.
    pub fn select(
        &self,
        host: &HostCompatibility,
        provider: Option<&Manifest>,
    ) -> Result<Option<&OciDescriptor>> {
        ensure!(
            self.schema_version == 2 && self.media_type == INDEX,
            "unsupported provider snapshot OCI index"
        );
        ensure!(
            self.manifests.len() <= 128,
            "too many provider snapshot variants"
        );
        let (Some(hypervisor), Some(vendor)) = (&host.hypervisor, &host.cpu_vendor) else {
            return Ok(None);
        };
        let selected = self
            .manifests
            .iter()
            .filter(|descriptor| {
                descriptor.media_type == MANIFEST
                    && descriptor.annotations.get(ARCH) == Some(&host.architecture)
                    && descriptor.annotations.get(HYPERVISOR) == Some(hypervisor)
                    && descriptor.annotations.get(CPU_VENDOR) == Some(vendor)
                    && descriptor.annotations.get(REF_NAME).is_some_and(|tag| {
                        tag.strip_suffix(hyperlight_unikraft::SNAPSHOT_KEY)
                            .is_some_and(|release| release.ends_with('-'))
                    })
            })
            .min_by(|left, right| (&left.digest, left.size).cmp(&(&right.digest, right.size)));
        if let Some(selected) = selected {
            selected.validate_inputs(provider, &host.architecture)?;
        }
        Ok(selected)
    }
}

pub(super) fn select_layout(
    directory: &Dir,
    file: &FileDigest,
    host: &HostCompatibility,
    artifact_path: &Path,
    provider: &Manifest,
) -> Result<Option<(PathBuf, String)>> {
    let (bytes, _) = read_verified(directory, file, PAYLOAD_LIMIT as u64, true)?;
    let index: SnapshotIndex =
        serde_json::from_slice(&bytes).context("decode provider snapshot OCI index")?;
    let index_path = Path::new(&file.path);
    ensure!(
        index_path
            .file_name()
            .is_some_and(|name| name == "index.json"),
        "provider snapshot index must name a native OCI index.json"
    );
    let Some(descriptor) = index.select(host, Some(provider))? else {
        return Ok(None);
    };
    let parent = index_path
        .parent()
        .filter(|path| !path.as_os_str().is_empty());
    let layout = directory
        .open_dir(parent.unwrap_or(Path::new(".")))
        .context("open provider snapshot OCI layout")?;
    let mut marker = Vec::new();
    open_artifact_file(&layout, "oci-layout")?
        .take((PAYLOAD_LIMIT + 1) as u64)
        .read_to_end(&mut marker)?;
    ensure!(
        marker.len() <= PAYLOAD_LIMIT,
        "provider OCI layout marker exceeds 64 KiB"
    );
    let version: serde_json::Value =
        serde_json::from_slice(&marker).context("decode provider snapshot OCI layout marker")?;
    ensure!(
        version["imageLayoutVersion"] == "1.0.0",
        "unsupported provider snapshot OCI layout version"
    );
    let bytes = verify_blob(&layout, descriptor, PAYLOAD_LIMIT as u64)?;
    let manifest: SnapshotManifest =
        serde_json::from_slice(&bytes).context("decode selected provider snapshot OCI manifest")?;
    manifest.validate()?;
    let config = verify_blob(&layout, &manifest.config, PAYLOAD_LIMIT as u64)?;
    manifest.validate_config(&config, host)?;
    for (layer, limit) in manifest.blobs() {
        verify_blob(&layout, layer, limit)?;
    }
    let snapshot = artifact_path
        .canonicalize()?
        .join(parent.unwrap_or(Path::new(".")));
    Ok(Some((snapshot, descriptor.digest.clone())))
}

fn verify_blob(directory: &Dir, descriptor: &OciDescriptor, limit: u64) -> Result<Vec<u8>> {
    let hash = descriptor.hash(limit)?;
    let relative = format!("blobs/sha256/{hash}");
    let (bytes, size) = read_verified(
        directory,
        &FileDigest {
            path: relative,
            sha256: hash.into(),
        },
        descriptor.size,
        descriptor.media_type != MEMORY,
    )?;
    ensure!(
        size == descriptor.size,
        "provider snapshot OCI descriptor size mismatch"
    );
    Ok(bytes)
}

impl SnapshotManifest {
    pub(crate) fn validate(&self) -> Result<()> {
        let memory = self.layers.len().saturating_sub(1);
        ensure!(
            self.schema_version == 2
                && self.media_type == MANIFEST
                && self.artifact_type == CONFIG
                && self.config.media_type == CONFIG
                && (1..=MAX_MEMORY_LAYERS).contains(&memory)
                && self.layers[..memory].iter().all(|l| l.media_type == MEMORY)
                && self.layers[memory].media_type == TRANSPORT,
            "unsupported selected provider snapshot OCI manifest"
        );
        let total = self.layers[..memory]
            .iter()
            .try_fold(0_u64, |total, l| total.checked_add(l.size));
        ensure!(
            total.is_some_and(|total| total <= MEMORY_LIMIT),
            "provider snapshot memory exceeds its size limit"
        );
        Ok(())
    }

    /// Every layer of a validated manifest, with the size limit its
    /// descriptor is checked against: the memory layers, then transport.
    pub(crate) fn blobs(&self) -> impl Iterator<Item = (&OciDescriptor, u64)> {
        let memory = self.layers.len().saturating_sub(1);
        self.layers.iter().enumerate().map(move |(i, layer)| {
            (
                layer,
                if i < memory {
                    MEMORY_LIMIT
                } else {
                    TRANSPORT_LIMIT
                },
            )
        })
    }

    pub(crate) fn validate_config(&self, bytes: &[u8], host: &HostCompatibility) -> Result<()> {
        #[derive(Deserialize)]
        struct Compatibility {
            arch: String,
            hypervisor: String,
            cpu_vendor: String,
        }
        let config: Compatibility =
            serde_json::from_slice(bytes).context("decode selected provider snapshot config")?;
        ensure!(
            config.arch == host.architecture
                && Some(&config.hypervisor) == host.hypervisor.as_ref()
                && Some(&config.cpu_vendor) == host.cpu_vendor.as_ref(),
            "selected provider snapshot config does not match host compatibility"
        );
        Ok(())
    }
}

impl OciDescriptor {
    fn validate_inputs(&self, provider: Option<&Manifest>, architecture: &str) -> Result<()> {
        let kernel = provider
            .map(|provider| {
                provider.kernel.as_ref().map_or_else(
                    || crate::assets::embedded_kernel_sha256(architecture),
                    |file| Ok(file.sha256.clone()),
                )
            })
            .transpose()?;
        for (field, expected) in [
            (
                "rootfs",
                provider.map(|provider| provider.rootfs.sha256.as_str()),
            ),
            (
                "program",
                provider.map(|provider| provider.program.sha256.as_str()),
            ),
            ("kernel", kernel.as_deref()),
        ] {
            let actual = self
                .annotations
                .get(&format!("dev.lampyr.provider.{field}.sha256"));
            ensure!(
                actual.is_some_and(|hash| hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))),
                "selected provider snapshot {field} digest is missing or invalid; recapture the variant"
            );
            if let Some(expected) = expected {
                ensure!(
                    actual.map(String::as_str) == Some(expected),
                    "selected provider snapshot {field} digest does not match the provider's cold-boot asset"
                );
            }
        }
        Ok(())
    }

    pub(crate) fn hash(&self, limit: u64) -> Result<&str> {
        let hash = self
            .digest
            .strip_prefix("sha256:")
            .context("provider snapshot OCI descriptor must use SHA-256")?;
        ensure!(
            hash.len() == 64
                && hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "invalid provider snapshot OCI digest"
        );
        ensure!(
            self.size <= limit,
            "provider snapshot OCI descriptor exceeds its size limit"
        );
        Ok(hash)
    }
}

fn read_verified(
    directory: &Dir,
    file: &FileDigest,
    limit: u64,
    retain: bool,
) -> Result<(Vec<u8>, u64)> {
    crate::workspace::relative_path(&file.path)?;
    let mut input = open_artifact_file(directory, &file.path)?.into_std();
    // The memory image is the only large blob and is never retained.
    let remember = !retain;
    let metadata = input.metadata()?;
    if remember && super::verified::contains(&metadata, &file.sha256) {
        return Ok((Vec::new(), metadata.len()));
    }
    let mut digest = Sha256::new();
    let mut total = 0_u64;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(
            total <= limit,
            "provider snapshot OCI descriptor size mismatch or size limit exceeded"
        );
        digest.update(&buffer[..count]);
        if retain {
            bytes.extend_from_slice(&buffer[..count]);
        }
    }
    let actual = digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    ensure!(
        actual == file.sha256,
        "provider artifact checksum mismatch for {}",
        file.path
    );
    if remember {
        super::verified::insert(&metadata, &file.sha256);
    }
    Ok((bytes, total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn selected_variant_bindings_are_required_and_checked_before_blob_loading() {
        let host = HostCompatibility {
            architecture: "x86_64".into(),
            hypervisor: Some("kvm".into()),
            cpu_vendor: Some("GenuineIntel".into()),
        };
        let provider: Manifest = serde_json::from_value(json!({
            "protocolVersion": 1, "id": "test", "version": "1",
            "runtimeVersion": crate::assets::VERSION, "memoryMiB": 256,
            "rootfs": {"path": "rootfs.cpio", "sha256": "a".repeat(64)},
            "program": {"path": "provider.py", "sha256": "b".repeat(64)},
            "operations": {"describe": []},
        }))
        .unwrap();
        let mut selected = layer(MANIFEST, 1);
        selected.annotations = BTreeMap::from([
            (ARCH.into(), host.architecture.clone()),
            (HYPERVISOR.into(), "kvm".into()),
            (CPU_VENDOR.into(), "GenuineIntel".into()),
            (
                REF_NAME.into(),
                format!(
                    "{}-{}",
                    crate::assets::VERSION,
                    hyperlight_unikraft::SNAPSHOT_KEY
                ),
            ),
            (
                "dev.lampyr.provider.rootfs.sha256".into(),
                provider.rootfs.sha256.clone(),
            ),
            (
                "dev.lampyr.provider.program.sha256".into(),
                provider.program.sha256.clone(),
            ),
            (
                "dev.lampyr.provider.kernel.sha256".into(),
                crate::assets::embedded_kernel_sha256(&host.architecture).unwrap(),
            ),
        ]);
        let index = SnapshotIndex {
            schema_version: 2,
            media_type: INDEX.into(),
            manifests: vec![selected],
        };
        assert!(index.select(&host, Some(&provider)).unwrap().is_some());
        for field in ["rootfs", "program", "kernel"] {
            for value in [None, Some("c".repeat(64)), Some("SHA256:bad".into())] {
                let mut invalid = index.clone();
                let key = format!("dev.lampyr.provider.{field}.sha256");
                if let Some(value) = value {
                    invalid.manifests[0].annotations.insert(key, value);
                } else {
                    invalid.manifests[0].annotations.remove(&key);
                }
                let error = invalid.select(&host, Some(&provider)).unwrap_err();
                assert!(error.to_string().contains(field), "{error}");
                let mut other = host.clone();
                other.cpu_vendor = Some("AuthenticAMD".into());
                assert!(invalid.select(&other, Some(&provider)).unwrap().is_none());
            }
        }
    }

    fn layer(media_type: &str, size: u64) -> OciDescriptor {
        OciDescriptor {
            media_type: media_type.into(),
            digest: format!("sha256:{}", "0".repeat(64)),
            size,
            annotations: BTreeMap::new(),
        }
    }

    fn manifest(layers: Vec<OciDescriptor>) -> SnapshotManifest {
        SnapshotManifest {
            schema_version: 2,
            media_type: MANIFEST.into(),
            artifact_type: CONFIG.into(),
            config: layer(CONFIG, 2),
            layers,
        }
    }

    /// hyperlight-host 0.19 splits a snapshot's memory into layers, 30 of
    /// data and one of page tables at most, and ends with its transport:
    /// every layer is checked against its limit.
    #[test]
    fn memory_in_layers_then_one_transport_is_accepted() {
        for count in [1, 2, 30, 31] {
            let mut layers = vec![layer(MEMORY, 1); count];
            layers.push(layer(TRANSPORT, 1));
            let manifest = manifest(layers);
            manifest.validate().unwrap();
            let limits: Vec<u64> = manifest.blobs().map(|(_, limit)| limit).collect();
            let mut want = vec![MEMORY_LIMIT; count];
            want.push(TRANSPORT_LIMIT);
            assert_eq!(limits, want);
        }
    }

    #[test]
    fn other_layer_layouts_are_refused() {
        let memory = || layer(MEMORY, 1);
        let transport = || layer(TRANSPORT, 1);
        for (name, layers) in [
            ("no layers", vec![]),
            ("no memory", vec![transport()]),
            ("no transport", vec![memory(), memory()]),
            ("transport first", vec![transport(), memory()]),
            (
                "too many memory layers",
                [vec![memory(); 32], vec![transport()]].concat(),
            ),
            (
                "an older memory layer",
                vec![
                    layer("application/vnd.hyperlight.snapshot.memory.v1", 1),
                    transport(),
                ],
            ),
            (
                "memory over its limit in all",
                vec![layer(MEMORY, MEMORY_LIMIT), layer(MEMORY, 1), transport()],
            ),
            (
                "memory sizes that overflow",
                vec![layer(MEMORY, u64::MAX), layer(MEMORY, 1), transport()],
            ),
        ] {
            assert!(manifest(layers).validate().is_err(), "{name} was accepted");
        }
    }
}
