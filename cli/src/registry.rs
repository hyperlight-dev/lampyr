// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::Deserialize;

use crate::host::Host;
use crate::image::{self, Metadata};
use crate::service;

pub struct Options<'a> {
    pub directory: &'a Path,
    pub artifact: Option<&'a Path>,
    pub image: &'a str,
}

pub struct Prepared {
    pub directory: PathBuf,
    pub artifact: PathBuf,
    pub metadata: Metadata,
    image_id: String,
}

pub fn prepare(options: &Options<'_>, host: &Host) -> Result<Prepared> {
    validate_tag(options.image)?;
    let directory = options
        .directory
        .canonicalize()
        .context("resolve service directory")?;
    let artifact = options
        .artifact
        .map(Path::to_path_buf)
        .unwrap_or_else(|| directory.join(".lampyr/artifact").join(image::TARGET));
    let artifact = artifact
        .canonicalize()
        .context("resolve local artifact directory")?;
    let metadata = image::verify(&artifact)?;
    let local = metadata.local_image(&artifact)?;
    let image_id = verify_packaged(&directory, &local, &artifact, &metadata, host)?;
    Ok(Prepared {
        directory,
        artifact,
        metadata,
        image_id,
    })
}

pub fn push(options: Options<'_>, host: &Host) -> Result<String> {
    let prepared = host.step("Checking your local artifact", || prepare(&options, host))?;
    publish(&prepared, options.image, host)
}

pub fn ensure_published(prepared: &Prepared, image: &str, host: &Host) -> Result<String> {
    validate_tag(image)?;
    let mut command = host.command("docker")?;
    command.current_dir(&prepared.directory).args([
        "buildx",
        "imagetools",
        "inspect",
        "--raw",
        image,
    ]);
    let output = host.output(&mut command)?;
    if output.exit == 0 {
        ensure!(!output.truncated, "registry manifest exceeds 8 MiB");
        #[derive(Deserialize)]
        struct Manifest {
            config: Option<Config>,
        }
        #[derive(Deserialize)]
        struct Config {
            digest: String,
        }
        let manifest: Manifest =
            serde_json::from_str(&output.stdout).context("decode registry manifest")?;
        let immutable = resolve(image, &prepared.directory, host)?;
        if manifest
            .config
            .is_some_and(|config| config.digest == prepared.image_id)
            || immutable.ends_with(&format!("@{}", prepared.image_id))
        {
            return Ok(immutable);
        }
    } else {
        let message = format!("{}{}", output.stderr, output.stdout).to_ascii_lowercase();
        let missing = [
            "manifest unknown",
            "manifest not found",
            "no such manifest",
            "name_unknown",
        ]
        .iter()
        .any(|marker| message.contains(marker))
            || message.lines().any(|line| {
                line.trim() == format!("error: {}: not found", image.to_ascii_lowercase())
            });
        if !missing {
            host.require_success(&command, &output)?;
        }
    }
    publish(prepared, image, host)
}

fn publish(prepared: &Prepared, image: &str, host: &Host) -> Result<String> {
    host.step("Sending your image to the registry", || {
        host.run(
            host.command("docker")?
                .current_dir(&prepared.directory)
                .args(["tag", &prepared.image_id, image]),
        )?;
        host.run(
            host.command("docker")?
                .current_dir(&prepared.directory)
                .args(["push", image]),
        )
    })?;
    host.step("Resolving the immutable image digest", || {
        resolve(image, &prepared.directory, host)
    })
}

pub fn resolve(image: &str, directory: &Path, host: &Host) -> Result<String> {
    if let Some((repository, digest)) = image.split_once('@') {
        validate_digest(digest)?;
        ensure!(!repository.is_empty(), "image repository is empty");
        return Ok(image.to_owned());
    }
    let digest = host.capture(host.command("docker")?.current_dir(directory).args([
        "buildx",
        "imagetools",
        "inspect",
        "--format",
        "{{json .Manifest.Digest}}",
        image,
    ]))?;
    let digest: String = serde_json::from_str(&digest).context("decode registry digest")?;
    validate_digest(&digest)?;
    let repository = if let Some((repository, _)) = tag_parts(image) {
        repository
    } else {
        image
    };
    Ok(format!("{repository}@{digest}"))
}

pub fn validate_tag(image: &str) -> Result<()> {
    validate_build_tag(image)?;
    ensure!(
        !image
            .split('/')
            .next()
            .is_some_and(|registry| registry.eq_ignore_ascii_case("lampyr.local")),
        "lampyr.local images are local build-cache tags, not registry destinations; use --image REGISTRY/REPOSITORY:TAG"
    );
    Ok(())
}

pub fn validate_build_tag(image: &str) -> Result<()> {
    ensure!(!image.trim().is_empty(), "--image is required");
    ensure!(
        !image.starts_with('-') && !image.chars().any(char::is_whitespace),
        "invalid registry image"
    );
    ensure!(
        !image.contains('@'),
        "--image must be a repository tag, not a digest"
    );
    let (repository, tag) = tag_parts(image).context("--image must include an explicit tag")?;
    ensure!(
        !repository.is_empty()
            && !tag.is_empty()
            && tag.len() <= 128
            && tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
            && tag
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_'),
        "invalid registry image tag"
    );
    Ok(())
}

fn tag_parts(image: &str) -> Option<(&str, &str)> {
    let colon = image.rfind(':')?;
    if image.rfind('/').is_some_and(|slash| colon < slash) {
        return None;
    }
    Some((&image[..colon], &image[colon + 1..]))
}

fn validate_digest(digest: &str) -> Result<()> {
    ensure!(
        image::is_sha256_digest(digest),
        "registry returned an invalid image digest"
    );
    Ok(())
}

pub fn verify_packaged(
    directory: &Path,
    image: &str,
    artifact: &Path,
    metadata: &Metadata,
    host: &Host,
) -> Result<String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Inspect {
        id: String,
        config: Config,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Config {
        entrypoint: Option<Vec<String>>,
        cmd: Option<Vec<String>>,
        env: Option<Vec<String>>,
        #[serde(default)]
        working_dir: String,
        labels: Option<BTreeMap<String, String>>,
    }
    // Resolve mutable tags once, then inspect and export by the immutable local ID.
    let encoded = host.capture(host.command("docker")?.current_dir(directory).args([
        "image",
        "inspect",
        "--format",
        "{{json .}}",
        image,
    ]))?;
    let inspect: Inspect = serde_json::from_str(&encoded).context("inspect packaged image")?;
    validate_digest(&inspect.id)?;
    let expected: Vec<String> = metadata
        .project
        .command
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    ensure!(
        inspect.config.entrypoint.as_ref() == Some(&expected)
            && inspect.config.cmd.is_none_or(|command| command.is_empty())
            && inspect.config.env.as_deref()
                == Some(&[
                    "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_owned()
                ])
            && inspect.config.working_dir == "/"
            && inspect.config.labels.is_none_or(|labels| labels.is_empty()),
        "artifact image configuration does not match project metadata"
    );
    let temporary = tempfile::Builder::new().prefix("lampyr-image-").tempdir()?;
    let exported = temporary.path().join("rootfs");
    service::export_image(directory, &inspect.id, &exported, host)?;
    ensure!(
        image::file_digest(&exported.join("artifact/metadata.json"))?
            == image::file_digest(&artifact.join("metadata.json"))?,
        "artifact image metadata does not match the local artifact"
    );
    ensure!(
        image::file_digest(&exported.join("unikernel/kernel"))? == metadata.digest("kernel")?,
        "artifact image kernel digest does not match metadata"
    );
    ensure!(
        image::directory_digest(&exported.join("unikernel/snapshot"), &[])?
            == metadata.digest("snapshot")?,
        "artifact image snapshot digest does not match metadata"
    );
    Ok(inspect.id)
}
