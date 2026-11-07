// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use lampyr_cli::{assets, host::Host, runtime::Cancellation};

#[derive(Parser)]
#[command(about = "Prepare the pinned Lampyr runtime build assets")]
struct Args {
    #[arg(long, default_value = ".")]
    root: PathBuf,
    /// Prepare a runtime base and print its --build-context assignment.
    #[arg(long)]
    rootfs: Option<String>,
    #[arg(long, default_value = "linux/amd64")]
    platform: String,
}

fn run(args: Args) -> Result<()> {
    let cancellation = Cancellation::default();
    let interrupted = cancellation.clone();
    ctrlc::set_handler(move || interrupted.cancel()).context("install signal handlers")?;
    let host = Host {
        verbose: false,
        cancellation,
    };
    let root = std::path::absolute(args.root)?;
    match args.rootfs {
        Some(runtime) => {
            println!(
                "{}",
                assets::runtime_base(&root, &runtime, &args.platform, &host)?
            );
            Ok(())
        }
        None => assets::setup(&root, &host),
    }
}

fn main() {
    if let Err(error) = run(Args::parse()) {
        eprintln!("runtime assets: {error:#}");
        std::process::exit(1);
    }
}
