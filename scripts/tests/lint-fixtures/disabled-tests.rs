// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

// ruleid: rust-disabled-test
#[ignore]
fn disabled() {}

// ruleid: rust-disabled-test
#[ignore = "later"]
fn disabled_with_reason() {}

// ruleid: rust-disabled-test
#[cfg_attr(unix, ignore)]
fn conditionally_disabled() {}

// ruleid: rust-disabled-test
#[cfg_attr(unix, ignore = "later")]
fn conditionally_disabled_with_reason() {}

// ok: rust-disabled-test
#[test]
fn enabled() {
    let fixture = r#"#[ignore]"#;
    let _ = fixture;
}
