// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::path::PathBuf;

use lampyr_cli::host::Host;
use lampyr_cli::registry::{self, Options};

#[test]
fn deployment_publishes_missing_content_and_reuses_matching_registry_content() {
    let directory =
        PathBuf::from(std::env::var_os("LAMPYR_TEST_PROJECT").expect("set LAMPYR_TEST_PROJECT"));
    let image =
        std::env::var("LAMPYR_TEST_REGISTRY_IMAGE").expect("set LAMPYR_TEST_REGISTRY_IMAGE");
    assert!(
        image.starts_with("127.0.0.1:") || image.starts_with("localhost:"),
        "use a disposable local registry"
    );
    let host = Host {
        verbose: false,
        cancellation: Default::default(),
    };
    let prepared = registry::prepare(
        &Options {
            directory: &directory,
            artifact: None,
            image: &image,
        },
        &host,
    )
    .unwrap();
    let first = registry::ensure_published(&prepared, &image, &host).unwrap();
    assert!(first.contains("@sha256:"));
    let second = registry::ensure_published(&prepared, &image, &host).unwrap();
    assert_eq!(first, second);
    registry::verify_packaged(
        &directory,
        &first,
        &prepared.artifact,
        &prepared.metadata,
        &host,
    )
    .unwrap();
}
