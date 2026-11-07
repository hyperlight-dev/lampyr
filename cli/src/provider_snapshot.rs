// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};
use clap::Parser;
use hyperlight_unikraft::SandboxBuilder;
use lampyr_cli::artifact::HostCompatibility;

#[derive(Parser)]
#[command(about = "Create a warm snapshot for a packaged provider guest")]
struct Args {
    #[arg(long)]
    rootfs: PathBuf,
    #[arg(long)]
    kernel: Option<PathBuf>,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    memory_mib: usize,
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
    let output = std::path::absolute(args.output)?;
    let parent = output
        .parent()
        .context("provider snapshot output has no parent")?;
    std::fs::create_dir_all(parent)?;
    let staging = tempfile::Builder::new()
        .prefix(".provider-snapshot-")
        .tempdir_in(parent)?;
    let mut builder = SandboxBuilder::from_initrd(&args.rootfs).scratch_mb(args.memory_mib);
    if let Some(kernel) = args.kernel {
        builder = builder.kernel(kernel);
    }
    let mut sandbox = builder.boot().context("boot provider guest for snapshot")?;
    sandbox
        .snapshot_to(staging.path())
        .context("save provider runtime snapshot")?;
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
