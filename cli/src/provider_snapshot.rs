// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use clap::Parser;
use hyperlight_unikraft::SandboxBuilder;
use lampyr_cli::artifact::{HostCompatibility, SnapshotIndex};
use sha2::{Digest, Sha256};

#[derive(Parser)]
#[command(about = "Create a warm snapshot for a packaged provider guest")]
struct Args {
    #[arg(long)]
    rootfs: PathBuf,
    #[arg(long)]
    program: PathBuf,
    #[arg(long)]
    kernel: Option<PathBuf>,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    memory_mib: usize,
}

fn file_hash(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

fn input_annotations(args: &Args) -> Result<BTreeMap<String, String>> {
    let kernel = match &args.kernel {
        Some(path) => file_hash(path)?,
        None => lampyr_cli::assets::embedded_kernel_sha256(std::env::consts::ARCH)?,
    };
    Ok([
        ("rootfs", file_hash(&args.rootfs)?),
        ("program", file_hash(&args.program)?),
        ("kernel", kernel),
    ]
    .into_iter()
    .map(|(field, hash)| (format!("dev.lampyr.provider.{field}.sha256"), hash))
    .collect())
}

fn run(args: Args) -> Result<()> {
    ensure!(
        (128..=4096).contains(&args.memory_mib),
        "provider memory must be 128 to 4096 MiB"
    );
    let compatibility = HostCompatibility::current();
    ensure!(
        compatibility.hypervisor.is_some() && compatibility.cpu_vendor.is_some(),
        "provider snapshots require a supported native hypervisor"
    );
    let inputs = input_annotations(&args).context("hash provider snapshot inputs")?;
    let output = std::path::absolute(&args.output)?;
    let parent = output
        .parent()
        .context("provider snapshot output has no parent")?;
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".provider-snapshot-")
        .tempdir_in(parent)?;
    let mut builder = SandboxBuilder::from_initrd(&args.rootfs).scratch_mb(args.memory_mib);
    if let Some(kernel) = &args.kernel {
        builder = builder.kernel(kernel);
    }
    let mut sandbox = builder.boot().context("boot provider guest for snapshot")?;
    sandbox
        .snapshot_to(staging.path())
        .context("save provider runtime snapshot")?;
    ensure!(
        input_annotations(&args)? == inputs,
        "provider inputs changed during snapshot capture"
    );
    let index_path = staging.path().join("index.json");
    let mut index: SnapshotIndex = serde_json::from_slice(&std::fs::read(&index_path)?)?;
    ensure!(
        !index.manifests.is_empty(),
        "captured provider snapshot has no variants"
    );
    for descriptor in &mut index.manifests {
        descriptor.annotations.extend(inputs.clone());
    }
    std::fs::write(index_path, serde_json::to_vec(&index)?)?;
    let restored = hyperlight_unikraft::load_snapshot(staging.path())
        .context("load captured provider snapshot")?;
    SandboxBuilder::from_snapshot(restored)
        .boot()
        .context("restore captured provider snapshot")?;
    if output.exists() {
        let metadata = std::fs::symlink_metadata(&output)?;
        ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "provider snapshot output must be a real directory"
        );
        std::fs::remove_dir_all(&output)?;
    }
    let staged_snapshot = staging.keep();
    std::fs::rename(staged_snapshot, &output).context("publish provider runtime snapshot")?;
    Ok(())
}

fn main() {
    if let Err(error) = run(Args::parse()) {
        eprintln!("provider snapshot: {error:#}");
        std::process::exit(1);
    }
}
