// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

#[cfg(unix)]
#[test]
fn cli_sigint_interrupts_stalled_snapshot_headers_and_bodies() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    for body in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let (ready, waiting) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            if body {
                let (mut probe, _) = listener.accept().unwrap();
                probe
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut headers = Vec::new();
                while !headers.ends_with(b"\r\n\r\n") {
                    let mut byte = [0];
                    probe.read_exact(&mut byte).unwrap();
                    headers.push(byte[0]);
                }
                probe
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                    .unwrap();
            }
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let mut headers = Vec::new();
            while !headers.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                stream.read_exact(&mut byte).unwrap();
                headers.push(byte[0]);
            }
            if body {
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n{",
                    )
                    .unwrap();
            }
            ready.send(()).unwrap();
            let _ = stream.read(&mut [0]);
        });
        let home = tempfile::tempdir().unwrap();
        let docker = tempfile::tempdir().unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_ly"))
            .args([
                "provider",
                "pull-snapshots",
                "--registry",
                &format!("http://{address}/provider/test"),
                "--index",
                &format!("sha256:{}", "a".repeat(64)),
            ])
            .env("LAMPYR_HOME", home.path())
            .env("DOCKER_CONFIG", docker.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        if waiting.recv_timeout(Duration::from_secs(3)).is_err() {
            child.kill().unwrap();
            let output = child.wait_with_output().unwrap();
            panic!(
                "CLI did not request index: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let started = Instant::now();
        // The child owns this live PID; signal only this CLI process.
        assert_eq!(
            unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) },
            0
        );
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > Duration::from_secs(1) {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("CLI ignored SIGINT during stalled HTTP response");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output().unwrap();
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("cancelled"));
        assert!(output.stdout.is_empty());
        assert!(
            !home
                .path()
                .join("cache/provider-snapshots")
                .join("a".repeat(64))
                .join("index.json")
                .exists()
        );
        server.join().unwrap();
    }
}
