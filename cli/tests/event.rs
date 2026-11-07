// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

use lampyr_cli::event::{self, Options};
use lampyr_cli::host::Host;
use lampyr_cli::project;
use serde_json::Value;

fn endpoint(statuses: Vec<u16>) -> (String, std::thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/events", listener.local_addr().unwrap());
    listener.set_nonblocking(true).unwrap();
    let thread = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut requests = Vec::new();
        for status in statuses {
            let stream = loop {
                assert!(
                    Instant::now() < deadline,
                    "event client did not send the expected requests"
                );
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(error) => panic!("{error}"),
                }
            };
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut reader = BufReader::new(stream);
            let mut headers = String::new();
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).unwrap() > 0);
                headers.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let length: usize = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse().unwrap())
                })
                .unwrap();
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            assert_eq!(body, b"{\"message\":\"hello\"}");
            requests.push(headers.to_ascii_lowercase());
            write!(
                reader.get_mut(),
                "HTTP/1.1 {status} result\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
        }
        requests
    });
    (url, thread)
}

fn project() -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    std::fs::write(
        directory.path().join("lampyr.workspace.yaml"),
        "apiVersion: lampyr.dev/v1alpha1\nkind: Workspace\n",
    )
    .unwrap();
    let service = directory.path().join("services/event-test");
    project::initialize(&service, Some("event-test"), "nodejs").unwrap();
    let mut manifest: serde_yaml_ng::Value =
        serde_yaml_ng::from_slice(&std::fs::read(service.join("lampyr.yaml")).unwrap()).unwrap();
    manifest["events"]["maxRetries"] = 2.into();
    manifest["events"]["initialBackoff"] = "1ms".into();
    manifest["events"]["attemptTimeout"] = "1s".into();
    std::fs::write(
        service.join("lampyr.yaml"),
        serde_yaml_ng::to_string(&manifest).unwrap(),
    )
    .unwrap();
    (directory, service)
}

#[test]
#[cfg(unix)]
fn implicit_events_to_cluster_local_services_require_a_reachable_override() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    let (directory, service) = project();
    let kubectl = directory.path().join("kubectl");
    std::fs::write(&kubectl, "#!/bin/sh\nprintf '%s' '{\"metadata\":{\"labels\":{\"networking.knative.dev/visibility\":\"cluster-local\"}},\"status\":{\"url\":\"http://event-test.default.svc.cluster.local\"}}'\n").unwrap();
    std::fs::set_permissions(&kubectl, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_ly"))
        .args(["event", "publish", "--context", "test"])
        .arg(&service)
        .env("PATH", directory.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("service is cluster-local; pass --url"),
        "{error}"
    );
    assert!(!service.join(".lampyr/dead-letter.jsonl").exists());
}

#[test]
fn event_retries_keep_a_stable_id_and_stop_after_acknowledgement() {
    let (_directory, service) = project();
    let (url, server) = endpoint(vec![503, 200]);
    let result = event::publish(
        Options {
            directory: &service,
            context: None,
            url: Some(&url),
            path: "/events",
            id: Some("stable-id"),
            data: "{\"message\":\"hello\"}",
            insecure: false,
        },
        &Host {
            verbose: false,
            cancellation: Default::default(),
        },
    )
    .unwrap();
    assert_eq!(result.id, "stable-id");
    assert_eq!(result.attempts, 2);
    let requests = server.join().unwrap();
    for (index, request) in requests.iter().enumerate() {
        assert!(request.contains("ce-id: stable-id\r\n"));
        assert!(request.contains("ce-specversion: 1.0\r\n"));
        assert!(request.contains(&format!("x-lampyr-delivery-attempt: {}\r\n", index + 1)));
    }
    assert!(!service.join(".lampyr/dead-letter.jsonl").exists());
}

#[test]
fn exhausted_delivery_is_an_error_and_durably_records_the_original_event() {
    let (_directory, service) = project();
    let (url, server) = endpoint(vec![503, 503, 503]);
    let error = event::publish(
        Options {
            directory: &service,
            context: None,
            url: Some(&url),
            path: "/events",
            id: Some("failed-id"),
            data: "{\"message\":\"hello\"}",
            insecure: false,
        },
        &Host {
            verbose: false,
            cancellation: Default::default(),
        },
    )
    .err()
    .unwrap();
    assert!(
        error
            .to_string()
            .contains("failed after 3 attempts and was dead-lettered")
    );
    assert_eq!(server.join().unwrap().len(), 3);
    let path = service.join(".lampyr/dead-letter.jsonl");
    let record: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(record["id"], "failed-id");
    assert_eq!(record["attempts"], 3);
    assert_eq!(record["data"], "{\"message\":\"hello\"}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[test]
fn cancellation_interrupts_an_in_flight_delivery() {
    let (_directory, service) = project();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/events", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        let (_stream, _) = listener.accept().unwrap();
        std::thread::sleep(Duration::from_millis(500));
    });
    let cancellation = lampyr_cli::runtime::Cancellation::default();
    let cancel = cancellation.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        cancel.cancel();
    });
    let started = Instant::now();
    let error = event::publish(
        Options {
            directory: &service,
            context: None,
            url: Some(&url),
            path: "/events",
            id: Some("cancelled-id"),
            data: "{\"message\":\"hello\"}",
            insecure: false,
        },
        &Host {
            verbose: false,
            cancellation,
        },
    )
    .err()
    .unwrap();
    assert!(error.to_string().contains("cancelled"), "{error:#}");
    assert!(started.elapsed() < Duration::from_millis(400));
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn source_creation_rejects_provider_mismatch_and_missing_eventing_before_loading_a_plugin() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let (_directory, service) = project();
    let platform = tempfile::tempdir().unwrap();
    let tools = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(platform.path().join("k8s")).unwrap();
    std::fs::write(platform.path().join(".lampyr-platform"), "").unwrap();
    std::fs::write(platform.path().join("k8s/fiberd-tenant.yaml"), "").unwrap();
    let kubectl = tools.path().join("kubectl");
    std::fs::write(
        &kubectl,
        r#"#!/bin/sh
    case "$*" in
    *'get configmap lampyr-install '*) printf '%s' "$INSTALLATION";;
    *) echo unexpected >&2; exit 71;;
    esac
    "#,
    )
    .unwrap();
    std::fs::set_permissions(&kubectl, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (provider, eventing, expected) in [
        ("kind", "true", "does not own this kind installation"),
        ("azure", "false", "reinstall with --eventing"),
    ] {
        let installation = serde_json::json!({"data":{
            "provider":provider,
            "cluster":"test",
            "installationId":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "eventing":eventing
        }});
        let output = Command::new(env!("CARGO_BIN_EXE_ly"))
            .args([
                "event",
                "source",
                "create",
                "--provider",
                "azure",
                "--name",
                "blob-created",
                "--context",
                "customer",
            ])
            .arg(&service)
            .env("PATH", tools.path())
            .env("INSTALLATION", installation.to_string())
            .current_dir(platform.path())
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
