// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

#![cfg(unix)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use lampyr_cli::runtime::Cancellation;
use lampyr_cli::tools::{self, CommandSpec};

#[test]
fn only_host_defined_commands_run_and_failures_keep_their_exit_status() {
    let directory = tempfile::tempdir().unwrap();
    let tools = BTreeMap::from([(
        "probe".to_owned(),
        CommandSpec {
            program: "/bin/sh".into(),
            arguments: ["-c", "printf result; printf diagnostic >&2; exit 7"]
                .map(Into::into)
                .to_vec(),
            directory: directory.path().into(),
        },
    )]);
    let cancellation = Cancellation::default();
    let deadline = Instant::now() + Duration::from_secs(5);
    assert!(tools::run(&tools, "not-authorized", deadline, &cancellation).is_err());
    let output = tools::run(&tools, "probe", deadline, &cancellation).unwrap();
    assert_eq!(output.exit, 7);
    assert_eq!(output.stdout, "result");
    assert_eq!(output.stderr, "diagnostic");
    assert!(!output.truncated);
}

#[test]
fn deadlines_and_cancellation_stop_the_command_and_its_children() {
    let directory = tempfile::tempdir().unwrap();
    let tools = BTreeMap::from([(
        "wait".to_owned(),
        CommandSpec {
            program: "/bin/sh".into(),
            arguments: ["-c", "sleep 30 & wait"].map(Into::into).to_vec(),
            directory: directory.path().into(),
        },
    )]);
    let started = Instant::now();
    let error = tools::run(
        &tools,
        "wait",
        started + Duration::from_millis(100),
        &Cancellation::default(),
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("deadline exceeded"));
    assert!(started.elapsed() < Duration::from_secs(3));
    let cancellation = Cancellation::default();
    let other = cancellation.clone();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            other.cancel();
        });
        let error = tools::run(
            &tools,
            "wait",
            Instant::now() + Duration::from_secs(5),
            &cancellation,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("cancelled"));
    });
}

#[cfg(unix)]
#[test]
fn interactive_cancellation_allows_graceful_sigterm_cleanup() {
    let directory = tempfile::tempdir().unwrap();
    let started = directory.path().join("started");
    let stopped = directory.path().join("stopped");
    let mut command = std::process::Command::new("/bin/sh");
    command
        .env("STARTED", &started)
        .env("STOPPED", &stopped)
        .args([
            "-c",
            "trap 'printf stopped > \"$STOPPED\"; exit 0' TERM; printf started > \"$STARTED\"; while :; do :; done",
        ]);
    let cancellation = Cancellation::default();
    let cancel = cancellation.clone();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            while !started.exists() {
                std::thread::sleep(Duration::from_millis(5));
            }

            cancel.cancel();
        });
        let error = tools::execute(
            &mut command,
            tools::Output::Interactive,
            1024,
            None,
            &cancellation,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
    });
    assert_eq!(std::fs::read_to_string(stopped).unwrap(), "stopped");
}

#[cfg(unix)]
#[test]
fn interactive_cancellation_kills_descendants_after_grace_period() {
    let directory = tempfile::tempdir().unwrap();
    let started = directory.path().join("started");
    let child_pid = directory.path().join("child-pid");
    let mut command = std::process::Command::new("/bin/sh");
    command
        .env("STARTED", &started)
        .env("CHILD_PID", &child_pid)
        .args([
            "-c",
            "trap '' TERM; while :; do sleep 1; done & child=$!; trap 'printf %s \"$child\" > \"$CHILD_PID\"; exit 0' TERM; printf started > \"$STARTED\"; while :; do sleep 0.05; done",
        ]);
    let cancellation = Cancellation::default();
    let cancel = cancellation.clone();
    let started_at = Instant::now();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            while !started.exists() {
                std::thread::sleep(Duration::from_millis(5));
            }
            cancel.cancel();
        });
        let error = tools::execute(
            &mut command,
            tools::Output::Interactive,
            1024,
            None,
            &cancellation,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("cancelled"), "{error:#}");
    });
    assert!(started_at.elapsed() >= Duration::from_secs(6));
    let pid: i32 = std::fs::read_to_string(child_pid).unwrap().parse().unwrap();
    let status = unsafe { libc::kill(pid, 0) };
    if status == 0 {
        let state = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        assert!(
            state.split_whitespace().nth(2) == Some("Z"),
            "descendant {pid} is still running"
        );
    }
}
