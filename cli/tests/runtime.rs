// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use hyperlight_unikraft::SandboxBuilder;
use lampyr_cli::runtime::{Cancellation, Runtime};
use serde_json::{Value, json};

fn runtime() -> Runtime {
    let rootfs = PathBuf::from(
        std::env::var_os("LAMPYR_TEST_ROOTFS")
            .expect("set LAMPYR_TEST_ROOTFS to the provider Python CPIO"),
    );
    let mut builder = SandboxBuilder::from_initrd(rootfs)
        .scratch_mb(256)
        .host_function("test.echo", |args| {
            let [value]: [Value; 1] =
                serde_json::from_str(args).map_err(|error| error.to_string())?;
            serde_json::to_string(&value).map_err(|error| error.to_string())
        });
    if let Some(kernel) = std::env::var_os("LAMPYR_TEST_KERNEL") {
        builder = builder.kernel(PathBuf::from(kernel));
    }
    let mut runtime = Runtime::boot(builder).expect("boot Python guest");
    runtime
        .load(
            include_str!("fixtures/probe.py"),
            Duration::from_secs(10),
            &Cancellation::default(),
        )
        .expect("load probe");
    runtime
}

#[test]
fn guest_calls_have_separate_results_and_only_registered_capabilities() {
    let mut runtime = runtime();
    let output = runtime
        .call(
            "probe",
            &json!({"value": "from host"}),
            Duration::from_secs(10),
            &Cancellation::default(),
        )
        .unwrap();
    assert_eq!(output.value, json!({"echo": "from host"}));
    assert!(output.stdout.contains("provider stdout"));
    assert!(output.stderr.contains("provider stderr"));
    assert!(!output.diagnostics_truncated);

    let error = runtime
        .call(
            "denied",
            &json!({}),
            Duration::from_secs(10),
            &Cancellation::default(),
        )
        .err()
        .expect("unregistered capability must fail");
    assert!(format!("{error:#}").contains("not.authorized"));
}

#[test]
fn deadline_interrupts_a_runaway_guest() {
    let mut runtime = runtime();
    let error = runtime
        .call(
            "runaway",
            &json!({}),
            Duration::from_millis(100),
            &Cancellation::default(),
        )
        .err()
        .expect("runaway guest must time out");
    assert!(error.to_string().contains("deadline exceeded"));
    let error = runtime
        .call(
            "probe",
            &json!({}),
            Duration::from_secs(1),
            &Cancellation::default(),
        )
        .err()
        .expect("interrupted VM must not be reused");
    assert!(error.to_string().contains("has been stopped"));
}

#[test]
fn cancellation_interrupts_an_in_flight_guest() {
    let mut runtime = runtime();
    let cancellation = Cancellation::default();
    let other = cancellation.clone();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            other.cancel();
        });
        let error = runtime
            .call(
                "runaway",
                &json!({}),
                Duration::from_secs(10),
                &cancellation,
            )
            .err()
            .expect("guest must be cancelled");
        assert!(error.to_string().contains("operation cancelled"));
    });
}

#[test]
fn deadline_and_cancellation_interrupt_parked_guests() {
    for cancel in [false, true] {
        let mut runtime = runtime();
        let cancellation = Cancellation::default();
        let other = cancellation.clone();
        let started = Instant::now();
        std::thread::scope(|scope| {
            if cancel {
                scope.spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    other.cancel();
                });
            }
            let error = runtime
                .call(
                    "parked",
                    &json!({}),
                    if cancel {
                        Duration::from_secs(10)
                    } else {
                        Duration::from_millis(100)
                    },
                    &cancellation,
                )
                .err()
                .expect("parked guest must be interrupted");
            assert!(
                error.to_string().contains(if cancel {
                    "cancelled"
                } else {
                    "deadline exceeded"
                }),
                "{error:#}"
            );
        });
        assert!(started.elapsed() < Duration::from_secs(2));
    }
}
