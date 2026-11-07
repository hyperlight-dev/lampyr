// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use super::*;
use crate::artifact::{Artifact, FileDigest, Manifest, Operation};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::json;
use std::collections::BTreeSet;
use std::net::TcpListener;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;

const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const CONFIG: &str = "application/vnd.hyperlight.snapshot.config.v5+json";
static DEFAULT_CACHE_TEST: Mutex<()> = Mutex::new(());

#[test]
fn optional_registry_fallback_distinguishes_unavailability_from_integrity_errors() {
    assert!(SnapshotRegistry::is_unavailable(&unavailable(
        "registry unavailable"
    )));
    assert!(!SnapshotRegistry::is_unavailable(&anyhow::anyhow!(
        "snapshot registry checksum mismatch"
    )));
    assert!(SnapshotRegistry::is_unavailable(&blob_stream_error(
        std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset")
    )));
    assert!(!SnapshotRegistry::is_unavailable(&blob_stream_error(
        std::io::Error::other(oci_client::errors::DigestError::VerificationError {
            expected: "sha256:expected".into(),
            actual: "sha256:actual".into(),
        })
    )));
    assert!(SnapshotRegistry::is_unavailable(&registry_error(
        OciDistributionError::ServerError {
            code: 503,
            url: "https://registry.example".into(),
            message: "unavailable".into(),
        }
    )));
    assert!(!SnapshotRegistry::is_unavailable(&registry_error(
        OciDistributionError::ManifestParsingError("invalid".into())
    )));
}

#[cfg(unix)]
fn test_helper_credentials(server: &str) -> Credentials {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let script = directory.path().join("helper");
    std::fs::write(
        &script,
        "#!/bin/sh\nread server\nprintf '%s' '{\"Username\":\"user\",\"Secret\":\"secret\"}'\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    helper_credentials(&script, server, &Cancellation::default())
        .unwrap()
        .unwrap()
}

impl Credentials {
    fn basic(&self) -> Result<String> {
        Ok(format!(
            "Basic {}",
            STANDARD.encode(format!(
                "{}:{}",
                self.username.as_str(),
                self.secret.as_str()
            ))
        ))
    }
}

#[test]
fn oci_client_rejects_untrusted_tls_certificates() {
    let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(
        vec![certified.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der()).into(),
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
        let mut stream = rustls::StreamOwned::new(connection, socket);
        assert!(stream.read(&mut [0_u8; 1]).is_err());
    });
    let cache = tempfile::tempdir().unwrap();
    let mut registry =
        SnapshotRegistry::new(&format!("https://{address}/provider/test"), cache.path()).unwrap();
    registry.docker_config = None;
    assert!(
        registry
            .pull(&fixture().digest, &host())
            .unwrap_err()
            .to_string()
            .contains("TLS")
    );
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn insecure_token_realms_are_rejected_before_credentials_are_sent() {
    for (redirect, host) in [
        (false, "localhost"),
        (false, "127.0.0.1"),
        (true, "localhost"),
        (true, "127.0.0.1"),
    ] {
        let calls = Arc::new(Mutex::new(0));
        let requests = calls.clone();
        let insecure = Mock::new(move |_| {
            *requests.lock().unwrap() += 1;
            Reply::new(200, br#"{"token":"insecure-token"}"#.to_vec())
        });
        let port = insecure.address.rsplit_once(':').unwrap().1;
        let insecure_url = format!("http://{host}:{port}/token");
        let origin = Arc::new(Mutex::new(String::new()));
        let registry_origin = origin.clone();
        let mock = Mock::new_tls(move |request| {
            if request.path == "/v2/" {
                let realm = if redirect {
                    format!("{}/token", registry_origin.lock().unwrap())
                } else {
                    insecure_url.clone()
                };
                Reply::new(401, vec![])
                    .header("WWW-Authenticate", &format!("Bearer realm=\"{realm}\""))
            } else {
                assert!(redirect && request.path.starts_with("/token"));
                Reply::new(307, vec![]).header("Location", &insecure_url)
            }
        });
        *origin.lock().unwrap() = format!("https://{}", mock.address);
        let cache = tempfile::tempdir().unwrap();
        let registry = client(&mock, cache.path());
        let mut session = mock.session(&registry);
        let reference = session.reference(&fixture().digest).unwrap();
        let result =
            session.authenticate_with(&reference, Some(test_helper_credentials(&mock.address)));
        let error = result.expect_err("accepted insecure token realm");
        assert!(
            format!("{error:#}").contains("HTTPS"),
            "unexpected error: {error:#}"
        );
        assert!(!format!("{error:#}").contains("secret"));
        assert_eq!(*calls.lock().unwrap(), 0, "credentials sent over HTTP");
    }
}

#[cfg(unix)]
#[test]
fn helper_credentials_authenticate_through_verified_https_token_realm() {
    let fixture = fixture();
    let digest = fixture.digest.clone();
    let origin = Arc::new(Mutex::new(String::new()));
    let registry_origin = origin.clone();
    let mock = Mock::new_tls(move |request| {
        if request.path.starts_with("/token") {
            assert_eq!(
                request.authorization.as_deref(),
                Some("Basic dXNlcjpzZWNyZXQ=")
            );
            Reply::new(200, br#"{"token":"secure-token"}"#.to_vec())
        } else if request.authorization.as_deref() != Some("Bearer secure-token") {
            Reply::new(401, vec![]).header(
                "WWW-Authenticate",
                &format!("Bearer realm=\"{}/token\"", registry_origin.lock().unwrap()),
            )
        } else {
            Reply::new(200, fixture.routes[&request.path].clone())
        }
    });
    *origin.lock().unwrap() = format!("https://{}", mock.address);
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let mut session = mock.session(&registry);
    let reference = session.reference(&digest).unwrap();
    session
        .authenticate_with(&reference, Some(test_helper_credentials(&mock.address)))
        .unwrap();
    assert!(session.manifest(&digest, INDEX, false).unwrap().is_some());
}

#[cfg(unix)]
#[test]
fn oci_client_redirects_do_not_forward_registry_authorization_across_origins() {
    let fixture = fixture();
    let digest = fixture.digest.clone();
    let index_path = format!("/v2/provider/test/manifests/{digest}");
    let index = fixture.routes[&index_path].clone();
    let target = Mock::new(move |request| {
        assert!(request.authorization.is_none());
        Reply::new(200, index.clone())
    });
    let target_url = format!("http://{}/index", target.address);
    let mock = Mock::new(move |request| {
        if request.path == "/v2/" {
            Reply::new(401, vec![]).header("WWW-Authenticate", "Basic realm=\"registry\"")
        } else if request.path == index_path {
            assert!(
                request
                    .authorization
                    .as_ref()
                    .is_some_and(|auth| auth.starts_with("Basic "))
            );
            Reply::new(307, vec![]).header("Location", &target_url)
        } else {
            Reply::new(200, fixture.routes[&request.path].clone())
        }
    });
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let mut session = Session::new(&registry).unwrap();
    let reference = session.reference(&digest).unwrap();
    session
        .authenticate_with(&reference, Some(test_helper_credentials(&mock.address)))
        .unwrap();
    assert!(session.manifest(&digest, INDEX, false).unwrap().is_some());
}

#[test]
fn unsupported_identity_tokens_fail_without_network_or_exposing_secrets() {
    let mock = Mock::new(|_| panic!("identity-token refresh must not fall back to anonymous"));
    let cache = tempfile::tempdir().unwrap();
    let docker = tempfile::tempdir().unwrap();
    std::fs::write(
        docker.path().join("config.json"),
        serde_json::to_vec(&json!({"auths":{&mock.address: {"identitytoken":"private-token"}}}))
            .unwrap(),
    )
    .unwrap();
    let mut registry = client(&mock, cache.path());
    registry.docker_config = Some(docker.path().to_owned());
    let error = registry
        .pull(&fixture().digest, &host())
        .unwrap_err()
        .to_string();
    assert!(error.contains("inline Docker credentials"));
    assert!(!error.contains("private-token"));
}

#[test]
fn request_deadlines_and_invalid_token_responses_are_explicit_errors() {
    let mock = Mock::new(|_| {
        std::thread::sleep(Duration::from_millis(100));
        Reply::new(200, b"{}".to_vec())
    });
    let cache = tempfile::tempdir().unwrap();
    let mut registry = client(&mock, cache.path());
    registry.timeout = Duration::from_millis(20);
    assert!(registry.pull(&fixture().digest, &host()).is_err());
    for body in [b"not-json-private".to_vec(), b"{}".to_vec()] {
        let realm = Arc::new(Mutex::new(String::new()));
        let token_realm = realm.clone();
        let mock = Mock::new(move |request| {
            if request.path.starts_with("/token") {
                Reply::new(200, body.clone())
            } else {
                Reply::new(401, vec![]).header(
                    "WWW-Authenticate",
                    &format!("Bearer realm=\"{}/token\"", token_realm.lock().unwrap()),
                )
            }
        });
        *realm.lock().unwrap() = format!("http://{}", mock.address);
        let registry = client(&mock, cache.path());
        let error = registry
            .pull(&fixture().digest, &host())
            .unwrap_err()
            .to_string();
        assert!(!error.contains("private"));
        assert!(error.contains("docker login"));
    }
}

struct Request {
    path: String,
    authorization: Option<String>,
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    pace: Option<Duration>,
    cut_after: Option<usize>,
}

impl Reply {
    fn new(status: u16, body: impl Into<Vec<u8>>) -> Self {
        Self {
            status,
            headers: vec![],
            body: body.into(),
            pace: None,
            cut_after: None,
        }
    }
    /// Sends the body one byte at a time, waiting `delay` before each byte.
    fn paced(mut self, delay: Duration) -> Self {
        self.pace = Some(delay);
        self
    }
    fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }
    fn cut_after(mut self, bytes: usize) -> Self {
        self.cut_after = Some(bytes);
        self
    }
}

struct Mock {
    address: String,
    certificate: Option<Vec<u8>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Mock {
    fn new(handler: impl Fn(Request) -> Reply + Send + 'static) -> Self {
        Self::start(handler, false)
    }

    fn new_tls(handler: impl Fn(Request) -> Reply + Send + 'static) -> Self {
        Self::start(handler, true)
    }

    fn start(handler: impl Fn(Request) -> Reply + Send + 'static, tls: bool) -> Self {
        let (tls_config, certificate) = if tls {
            let certified = rcgen::generate_simple_self_signed(vec!["127.0.0.1".into()]).unwrap();
            let config = rustls::ServerConfig::builder_with_provider(Arc::new(
                rustls::crypto::ring::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![certified.cert.der().clone()],
                rustls::pki_types::PrivatePkcs8KeyDer::from(certified.signing_key.serialize_der())
                    .into(),
            )
            .unwrap();
            (Some(Arc::new(config)), Some(certified.cert.der().to_vec()))
        } else {
            (None, None)
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap().to_string();
        listener.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_read_timeout(Some(Duration::from_secs(3)))
                            .unwrap();
                        if let Some(config) = &tls_config {
                            let mut connection =
                                rustls::ServerConnection::new(config.clone()).unwrap();
                            if connection.complete_io(&mut stream).is_ok() {
                                respond(
                                    &mut rustls::StreamOwned::new(connection, stream),
                                    &handler,
                                );
                            }
                        } else {
                            respond(&mut stream, &handler);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(e) => panic!("mock registry accept: {e}"),
                }
            }
        });
        Self {
            address,
            certificate,
            stop,
            thread: Some(thread),
        }
    }
    fn repository(&self) -> String {
        let scheme = if self.certificate.is_some() {
            "https"
        } else {
            "http"
        };
        format!("{scheme}://{}/provider/test", self.address)
    }

    fn session<'a>(&self, registry: &'a SnapshotRegistry) -> Session<'a> {
        let mut session = Session::new(registry).unwrap();
        if let Some(certificate) = &self.certificate {
            session.client = registry_client(
                ClientConfig {
                    protocol: ClientProtocol::Https,
                    extra_root_certificates: vec![oci_client::client::Certificate {
                        encoding: oci_client::client::CertificateEncoding::Der,
                        data: certificate.clone(),
                    }],
                    ..Default::default()
                },
                session.failures.clone(),
                registry.blob_route(),
            )
            .unwrap();
        }
        session
    }
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.thread.take().unwrap().join().unwrap();
    }
}

fn respond(stream: &mut (impl Read + Write), handler: &impl Fn(Request) -> Reply) {
    let reply = handler(request(stream));
    let mut headers = format!(
        "HTTP/1.1 {} test\r\nContent-Length: {}\r\nConnection: close\r\n",
        reply.status,
        reply.body.len()
    );
    for (name, value) in reply.headers {
        headers.push_str(&format!("{name}: {value}\r\n"));
    }
    headers.push_str("\r\n");
    // Timeout tests may close the socket before the response is ready.
    let Some(delay) = reply.pace else {
        let body = &reply.body[..reply
            .cut_after
            .unwrap_or(reply.body.len())
            .min(reply.body.len())];
        let _ = stream
            .write_all(headers.as_bytes())
            .and_then(|_| stream.write_all(body));
        return;
    };
    if stream
        .write_all(headers.as_bytes())
        .and_then(|_| stream.flush())
        .is_err()
    {
        return;
    }
    for byte in &reply.body {
        std::thread::sleep(delay);
        if stream
            .write_all(std::slice::from_ref(byte))
            .and_then(|_| stream.flush())
            .is_err()
        {
            return;
        }
    }
}

fn request(stream: &mut impl Read) -> Request {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        stream.read_exact(&mut byte).unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() <= PAYLOAD_LIMIT);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let mut lines = headers.lines();
    let mut first = lines.next().unwrap().split_whitespace();
    let _method = first.next().unwrap();
    let path = first.next().unwrap().into();
    let mut authorization = None;
    let mut length = 0;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("authorization") {
                authorization = Some(value.trim().into());
            }
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
    }
    assert!(length <= PAYLOAD_LIMIT);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).unwrap();
    Request {
        path,
        authorization,
    }
}

fn host() -> HostCompatibility {
    HostCompatibility {
        architecture: "x86_64".into(),
        hypervisor: Some("kvm".into()),
        cpu_vendor: Some("GenuineIntel".into()),
    }
}

fn descriptor(content: &[u8], media: &str) -> OciDescriptor {
    OciDescriptor {
        media_type: media.into(),
        digest: format!("sha256:{}", hex(&Sha256::digest(content))),
        size: content.len() as u64,
        annotations: BTreeMap::new(),
    }
}

struct Fixture {
    digest: String,
    routes: BTreeMap<String, Vec<u8>>,
    layer: OciDescriptor,
}

fn fixture() -> Fixture {
    let memory = b"snapshot memory";
    let layer = descriptor(memory, "application/vnd.hyperlight.snapshot.memory.v2");
    // A second memory layer, as a snapshot taken on top of another has:
    // hyperlight-host 0.19 writes the delta as its own layer.
    let second = b"second memory layer";
    let second_layer = descriptor(second, "application/vnd.hyperlight.snapshot.memory.v2");
    let transport = descriptor(
        b"transport",
        "application/vnd.hyperlight.snapshot.transport.v1",
    );
    let config_bytes = serde_json::to_vec(&json!({
        "arch": host().architecture, "hypervisor": host().hypervisor, "cpu_vendor": host().cpu_vendor
    })).unwrap();
    let config = descriptor(&config_bytes, CONFIG);
    let manifest = serde_json::to_vec(&SnapshotManifest {
        schema_version: 2,
        media_type: MANIFEST.into(),
        artifact_type: CONFIG.into(),
        config: config.clone(),
        layers: vec![layer.clone(), second_layer.clone(), transport.clone()],
    })
    .unwrap();
    let mut selected = descriptor(&manifest, MANIFEST);
    selected.annotations = BTreeMap::from([
        ("dev.hyperlight.snapshot.arch".into(), host().architecture),
        (
            "org.opencontainers.image.ref.name".into(),
            format!(
                "{}-{}",
                crate::assets::VERSION,
                hyperlight_unikraft::SNAPSHOT_KEY
            ),
        ),
        ("dev.hyperlight.snapshot.hypervisor".into(), "kvm".into()),
        (
            "dev.hyperlight.snapshot.cpu.vendor".into(),
            "GenuineIntel".into(),
        ),
    ]);
    let index = serde_json::to_vec(&SnapshotIndex {
        schema_version: 2,
        media_type: INDEX.into(),
        manifests: vec![selected.clone()],
    })
    .unwrap();
    let index_digest = descriptor(&index, INDEX).digest;
    Fixture {
        digest: index_digest.clone(),
        routes: BTreeMap::from([
            ("/v2/".into(), b"{}".to_vec()),
            (format!("/v2/provider/test/manifests/{index_digest}"), index),
            (
                format!("/v2/provider/test/manifests/{}", selected.digest),
                manifest,
            ),
            (
                format!("/v2/provider/test/blobs/{}", config.digest),
                config_bytes,
            ),
            (
                format!("/v2/provider/test/blobs/{}", layer.digest),
                memory.to_vec(),
            ),
            (
                format!("/v2/provider/test/blobs/{}", second_layer.digest),
                second.to_vec(),
            ),
            (
                format!("/v2/provider/test/blobs/{}", transport.digest),
                b"transport".to_vec(),
            ),
        ]),
        layer,
    }
}

fn client(mock: &Mock, cache: &Path) -> SnapshotRegistry {
    let mut registry = SnapshotRegistry::new(&mock.repository(), cache).unwrap();
    registry.docker_config = None;
    registry
}

fn artifact(root: &Path, digest: &str) {
    std::fs::write(root.join("rootfs.cpio"), b"rootfs").unwrap();
    std::fs::write(root.join("provider.py"), b"# provider").unwrap();
    let file = |path: &str, bytes: &[u8]| FileDigest {
        path: path.into(),
        sha256: hex(&Sha256::digest(bytes)),
    };
    let manifest = Manifest {
        protocol_version: 1,
        id: "test".into(),
        version: "1".into(),
        runtime_version: crate::assets::VERSION.into(),
        memory_mib: 256,
        rootfs: file("rootfs.cpio", b"rootfs"),
        program: file("provider.py", b"# provider"),
        kernel: None,
        cold_compatibility: None,
        snapshot_index: Some(FileDigest {
            path: "snapshot/index.json".into(),
            sha256: digest[7..].into(),
        }),
        snapshot_repository: None,
        environment: BTreeMap::new(),
        network_hosts: vec![],
        source_kinds: BTreeSet::new(),
        operations: BTreeMap::from([(Operation::Describe, BTreeSet::new())]),
    };
    std::fs::write(
        root.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
}

fn use_default_cache(path: &Path) {
    *TEST_DEFAULT_CACHE.lock().unwrap() = Some(path.to_owned());
}

fn set_snapshot_repository(root: &Path, repository: &str) {
    let path = root.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    manifest["snapshotRepository"] = repository.into();
    std::fs::write(path, serde_json::to_vec(&manifest).unwrap()).unwrap();
}

#[test]
fn manifest_repository_connection_reset_warns_and_cold_boots() {
    let _serial = DEFAULT_CACHE_TEST.lock().unwrap();
    let fixture = fixture();
    let routes = fixture.routes.clone();
    let layer = fixture.layer.digest.clone();
    let mock = Mock::new(move |request| {
        if request.path == "/v2/" {
            return Reply::new(200, b"{}".to_vec());
        }
        let body = routes[&request.path].clone();
        if request.path.ends_with(&layer) {
            Reply::new(200, body.clone()).cut_after(body.len() / 2)
        } else {
            Reply::new(200, body)
        }
    });
    let source = tempfile::tempdir().unwrap();
    artifact(source.path(), &fixture.digest);
    set_snapshot_repository(source.path(), &mock.repository());
    let cache = tempfile::tempdir().unwrap();
    use_default_cache(cache.path());
    let loaded = Artifact::load_for_host(source.path(), "test", &host()).unwrap();
    assert!(loaded.snapshot.is_none());
    *TEST_DEFAULT_CACHE.lock().unwrap() = None;
}

#[test]
fn manifest_repository_initial_connection_failures_cold_boot() {
    let _serial = DEFAULT_CACHE_TEST.lock().unwrap();
    let fixture = fixture();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let unavailable_repository = format!("http://{}/provider/test", listener.local_addr().unwrap());
    drop(listener);
    let source = tempfile::tempdir().unwrap();
    artifact(source.path(), &fixture.digest);
    set_snapshot_repository(source.path(), &unavailable_repository);
    let cache = tempfile::tempdir().unwrap();
    use_default_cache(cache.path());
    assert!(
        Artifact::load_for_host(source.path(), "test", &host())
            .unwrap()
            .snapshot
            .is_none()
    );

    let mock = Mock::new(|_| Reply::new(503, b"unavailable".to_vec()));
    let source = tempfile::tempdir().unwrap();
    artifact(source.path(), &fixture.digest);
    set_snapshot_repository(source.path(), &mock.repository());
    let cache = tempfile::tempdir().unwrap();
    use_default_cache(cache.path());
    assert!(
        Artifact::load_for_host(source.path(), "test", &host())
            .unwrap()
            .snapshot
            .is_none()
    );
    *TEST_DEFAULT_CACHE.lock().unwrap() = None;
}

#[test]
fn manifest_repository_untrusted_tls_remains_fatal() {
    let _serial = DEFAULT_CACHE_TEST.lock().unwrap();
    let fixture = fixture();
    let mock = Mock::new_tls(|_| Reply::new(200, b"{}".to_vec()));
    let source = tempfile::tempdir().unwrap();
    artifact(source.path(), &fixture.digest);
    set_snapshot_repository(source.path(), &mock.repository());
    let cache = tempfile::tempdir().unwrap();
    use_default_cache(cache.path());
    assert!(Artifact::load_for_host(source.path(), "test", &host()).is_err());
    *TEST_DEFAULT_CACHE.lock().unwrap() = None;
}

#[test]
fn manifest_repository_invalid_index_response_remains_fatal() {
    let _serial = DEFAULT_CACHE_TEST.lock().unwrap();
    let fixture = fixture();
    let digest = fixture.digest.clone();
    let mock = Mock::new(move |request| {
        if request.path == "/v2/" {
            Reply::new(200, b"{}".to_vec())
        } else if request.path.ends_with(&digest) {
            Reply::new(200, vec![b'x'; PAYLOAD_LIMIT + 1])
        } else {
            Reply::new(404, b"missing".to_vec())
        }
    });
    let source = tempfile::tempdir().unwrap();
    artifact(source.path(), &fixture.digest);
    set_snapshot_repository(source.path(), &mock.repository());
    let cache = tempfile::tempdir().unwrap();
    use_default_cache(cache.path());
    let error = Artifact::load_for_host(source.path(), "test", &host())
        .err()
        .unwrap();
    assert!(!SnapshotRegistry::is_unavailable(&error));
    *TEST_DEFAULT_CACHE.lock().unwrap() = None;
}

#[test]
fn assembled_provider_index_pulls_and_loads_from_cache() {
    let fixture = fixture();
    let source = tempfile::tempdir().unwrap();
    artifact(source.path(), &fixture.digest);
    let native = source.path().join("snapshot");
    std::fs::create_dir_all(native.join("blobs/sha256")).unwrap();
    // Only content routes name a blob; the registry's `/v2/` probe route does not.
    for (route, bytes) in fixture
        .routes
        .iter()
        .filter(|(route, _)| route.contains("sha256:"))
    {
        let hash = route.rsplit("sha256:").next().unwrap();
        std::fs::write(native.join("blobs/sha256").join(hash), bytes).unwrap();
    }
    std::fs::write(
        native.join("index.json"),
        &fixture.routes[&format!("/v2/provider/test/manifests/{}", fixture.digest)],
    )
    .unwrap();
    std::fs::write(
        native.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let output = directory.path().join("assembled");
    let status = std::process::Command::new("python3")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../providers/snapshot_oci.py"
        ))
        .args(["assemble", "--artifact"])
        .arg(source.path())
        .arg("--variant")
        .arg(source.path())
        .arg("--output")
        .arg(&output)
        .status()
        .unwrap();
    assert!(status.success());
    let blobs = output.join("blobs/sha256");
    let mock = Mock::new(move |request| {
        if request.path == "/v2/" {
            return Reply::new(200, b"{}".to_vec());
        }
        let hash = request.path.rsplit("sha256:").next().unwrap();
        assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
        Reply::new(200, std::fs::read(blobs.join(hash)).unwrap())
    });
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    std::fs::remove_file(output.join("index.json")).unwrap();
    let loaded =
        Artifact::load_for_host_with_registry(&output, "test", &host(), Some(&registry)).unwrap();
    assert!(loaded.snapshot.unwrap().join("index.json").is_file());
    drop(mock);
    let offline =
        Artifact::load_for_host_with_registry(&output, "test", &host(), Some(&registry)).unwrap();
    assert!(offline.snapshot.unwrap().join("index.json").is_file());
}

#[test]
fn pulls_verified_layout_and_startup_consumes_it_offline() {
    let fixture = fixture();
    let digest = fixture.digest.clone();
    let calls = Arc::new(Mutex::new(Vec::new()));
    let requests = calls.clone();
    let mock = Mock::new(move |request| {
        requests.lock().unwrap().push(request.path.clone());
        Reply::new(
            200,
            fixture
                .routes
                .get(&request.path)
                .expect("only selected content may be fetched")
                .clone(),
        )
    });
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let layout = registry.pull(&digest, &host()).unwrap().unwrap();
    assert!(layout.join("oci-layout").is_file());
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|path| path.as_str() != "/v2/")
            .count(),
        // The index, the manifest, the config and every layer, each once.
        6
    );
    let source = tempfile::tempdir().unwrap();
    artifact(source.path(), &digest);
    drop(mock);
    let loaded =
        Artifact::load_for_host_with_registry(source.path(), "test", &host(), Some(&registry))
            .unwrap();
    let selected_digest = loaded.snapshot_digest.as_ref().unwrap();
    let restored = loaded.snapshot.unwrap();
    assert_eq!(restored, layout.canonicalize().unwrap());
    assert!(restored.join("index.json").is_file());
    let selected: SnapshotIndex =
        serde_json::from_slice(&std::fs::read(restored.join("index.json")).unwrap()).unwrap();
    assert_eq!(selected_digest, &selected.manifests[0].digest);
    let manifest: SnapshotManifest = serde_json::from_slice(
        &std::fs::read(
            restored
                .join("blobs/sha256")
                .join(&selected.manifests[0].digest[7..]),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(
        std::fs::read(
            restored
                .join("blobs/sha256")
                .join(&manifest.layers[0].digest[7..])
        )
        .unwrap(),
        b"snapshot memory"
    );
    assert_eq!(
        std::fs::read(
            restored
                .join("blobs/sha256")
                .join(&manifest.layers[1].digest[7..])
        )
        .unwrap(),
        b"second memory layer"
    );
    assert_eq!(
        std::fs::read(
            restored
                .join("blobs/sha256")
                .join(&manifest.layers[2].digest[7..])
        )
        .unwrap(),
        b"transport"
    );
    assert!(!source.path().join("snapshot/index.json").exists());
    // Existing local indexes are authoritative; corruption is never masked by the cache.
    std::fs::create_dir(source.path().join("snapshot")).unwrap();
    std::fs::write(source.path().join("snapshot/index.json"), b"corrupt").unwrap();
    assert!(
        Artifact::load_for_host_with_registry(source.path(), "test", &host(), Some(&registry))
            .is_err()
    );
}

#[test]
fn fallback_is_only_absent_index_or_no_compatible_variant() {
    let fixture = fixture();
    let digest = fixture.digest.clone();
    let calls = Arc::new(Mutex::new(0));
    let count = calls.clone();
    let mock = Mock::new(move |request| {
        if request.path != "/v2/" {
            *count.lock().unwrap() += 1;
        }
        Reply::new(200, fixture.routes.get(&request.path).unwrap().clone())
    });
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let source = tempfile::tempdir().unwrap();
    artifact(source.path(), &digest);
    let incompatible = HostCompatibility {
        cpu_vendor: Some("AuthenticAMD".into()),
        ..host()
    };
    assert!(
        Artifact::load_for_host_with_registry(
            source.path(),
            "test",
            &incompatible,
            Some(&registry)
        )
        .unwrap()
        .snapshot
        .is_none()
    );
    assert_eq!(*calls.lock().unwrap(), 1);
    for status in [404, 401, 403, 429, 500] {
        let mock = Mock::new(move |request| {
            if request.path == "/v2/" {
                Reply::new(200, vec![])
            } else {
                Reply::new(status, b"secret response body".to_vec())
            }
        });
        let cache = tempfile::tempdir().unwrap();
        let registry = client(&mock, cache.path());
        let result =
            Artifact::load_for_host_with_registry(source.path(), "test", &host(), Some(&registry));
        if status == 404 {
            assert!(result.unwrap().snapshot.is_none());
        } else {
            let error = result.err().unwrap();
            assert!(!format!("{error:#}").contains("secret response"));
        }
    }
    // Without a configured registry a registry-only artifact cold-boots, like an absent index.
    let loaded =
        Artifact::load_for_host_with_registry(source.path(), "test", &host(), None).unwrap();
    assert!(loaded.snapshot.is_none() && loaded.snapshot_digest.is_none());
}

#[test]
fn oversized_metadata_bodies_are_rejected_while_streaming() {
    let mock = Mock::new(|request| {
        if request.path == "/v2/" {
            Reply::new(200, b"{}".to_vec())
        } else {
            Reply::new(200, vec![b'x'; 4 * PAYLOAD_LIMIT])
        }
    });
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let mut session = mock.session(&registry);
    let digest = fixture().digest;
    assert!(session.manifest(&digest, INDEX, true).is_err());
    // The transport stops reading at the limit instead of oci-client buffering the whole body.
    let path = format!("/v2/provider/test/manifests/{digest}");
    assert!(session.failures.lock().unwrap().contains(&(path, None)));
}

#[test]
fn only_registry_blob_requests_stream_unbounded() {
    let realm = Arc::new(Mutex::new(String::new()));
    let token_realm = realm.clone();
    let mock = Mock::new(move |request| {
        if request.path == "/v2/" {
            Reply::new(401, vec![]).header(
                "WWW-Authenticate",
                &format!("Bearer realm=\"{}\"", token_realm.lock().unwrap()),
            )
        } else {
            Reply::new(200, vec![b'x'; 4 * PAYLOAD_LIMIT])
        }
    });
    // A token endpoint named like a blob path must not escape the metadata limit.
    *realm.lock().unwrap() = format!("http://{}/v2/provider/test/blobs/token", mock.address);
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let mut session = mock.session(&registry);
    let reference = session.reference(&fixture().digest).unwrap();
    assert!(session.authenticate_with(&reference, None).is_err());
    assert!(
        session
            .failures
            .lock()
            .unwrap()
            .contains(&("/v2/provider/test/blobs/token".to_owned(), None))
    );

    // A blob-shaped token endpoint on another host is bounded too.
    let blob_path = format!("/v2/provider/test/blobs/sha256:{}", "a".repeat(64));
    let other = Mock::new(|_| Reply::new(200, vec![b'x'; 4 * PAYLOAD_LIMIT]));
    *realm.lock().unwrap() = format!("http://{}{blob_path}", other.address);
    let mut session = mock.session(&registry);
    assert!(session.authenticate_with(&reference, None).is_err());
    assert!(
        session
            .failures
            .lock()
            .unwrap()
            .contains(&(blob_path.clone(), None))
    );

    // So is a digest-shaped token endpoint on the registry's own host.
    *realm.lock().unwrap() = format!("http://{}{blob_path}", mock.address);
    let mut session = mock.session(&registry);
    assert!(session.authenticate_with(&reference, None).is_err());
    assert!(
        session
            .failures
            .lock()
            .unwrap()
            .contains(&(blob_path.clone(), None))
    );

    // A 401 on a real blob path is buffered by oci-client, so it is bounded as well.
    let unauthorized = Mock::new(|request| {
        if request.path == "/v2/" {
            Reply::new(200, b"{}".to_vec())
        } else {
            Reply::new(401, vec![b'x'; 4 * PAYLOAD_LIMIT])
        }
    });
    let registry = client(&unauthorized, cache.path());
    let mut session = unauthorized.session(&registry);
    let layer = fixture().layer;
    let directory = tempfile::tempdir().unwrap();
    let mut file = cap_std::fs::File::from_std(tempfile::tempfile_in(directory.path()).unwrap());
    assert!(session.download(&layer, "blobs", &mut file).is_err());
    let layer_path = format!("/v2/provider/test/blobs/{}", layer.digest);
    assert!(
        session
            .failures
            .lock()
            .unwrap()
            .contains(&(layer_path, None))
    );
}

#[test]
fn hosts_without_native_compatibility_cold_boot_without_the_registry() {
    let mock = Mock::new(|_| panic!("an incompatible host must not contact the registry"));
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let source = tempfile::tempdir().unwrap();
    artifact(source.path(), &fixture().digest);
    for host in [
        HostCompatibility {
            hypervisor: None,
            ..host()
        },
        HostCompatibility {
            cpu_vendor: None,
            ..host()
        },
    ] {
        let loaded =
            Artifact::load_for_host_with_registry(source.path(), "test", &host, Some(&registry))
                .unwrap();
        assert!(loaded.snapshot.is_none() && loaded.snapshot_digest.is_none());
    }
}

#[test]
fn swallowed_authentication_failures_block_index_404_fallback() {
    // oci-client repeats authentication before each request and ignores its errors: the explicit
    // probe succeeds anonymously, the repeated one fails, and the index then answers 404.
    let probes = Arc::new(Mutex::new(0));
    let counter = probes.clone();
    let mock = Mock::new(move |request| {
        if request.path == "/v2/" {
            let mut probes = counter.lock().unwrap();
            *probes += 1;
            if *probes == 1 {
                Reply::new(200, b"{}".to_vec())
            } else {
                Reply::new(500, vec![])
            }
        } else {
            Reply::new(404, vec![])
        }
    });
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let error = registry.pull(&fixture().digest, &host()).unwrap_err();
    assert!(format!("{error:#}").contains("authentication or request failed"));
    assert!(
        *probes.lock().unwrap() >= 2,
        "the repeated probe was not exercised"
    );
    // A plain 404 with healthy authentication still falls back.
    let mock = Mock::new(|request| {
        if request.path == "/v2/" {
            Reply::new(200, b"{}".to_vec())
        } else {
            Reply::new(404, vec![])
        }
    });
    let registry = client(&mock, cache.path());
    assert!(registry.pull(&fixture().digest, &host()).unwrap().is_none());
}

#[test]
fn structured_registry_errors_only_allow_index_http_404_fallback() {
    for (failed, status) in [
        ("index", 404),
        ("index", 400),
        ("index", 401),
        ("index", 403),
        ("index", 429),
        ("index", 500),
        ("manifest", 404),
        ("blob", 404),
        ("probe", 404),
        ("token", 404),
    ] {
        let fixture = fixture();
        let digest = fixture.digest.clone();
        let index_path = format!("/v2/provider/test/manifests/{digest}");
        let realm = Arc::new(Mutex::new(String::new()));
        let token_realm = realm.clone();
        let mock = Mock::new(move |request| {
            if failed == "token" && request.path == "/v2/" {
                return Reply::new(401, vec![]).header(
                    "WWW-Authenticate",
                    &format!("Bearer realm=\"{}/token\"", token_realm.lock().unwrap()),
                );
            }
            let fails = match failed {
                "index" => request.path == index_path,
                "manifest" => {
                    request.path.starts_with("/v2/provider/test/manifests/")
                        && request.path != index_path
                }
                "blob" => request.path.ends_with(&fixture.layer.digest),
                "probe" => request.path == "/v2/",
                "token" => request.path.starts_with("/token"),
                _ => unreachable!(),
            };
            if fails {
                Reply::new(
                    status,
                    br#"{"errors":[{"code":"MANIFEST_UNKNOWN","message":"private response details"}]}"#.to_vec(),
                )
                .header("Content-Type", "application/json")
            } else {
                Reply::new(200, fixture.routes[&request.path].clone())
            }
        });
        *realm.lock().unwrap() = format!("http://{}", mock.address);
        let cache = tempfile::tempdir().unwrap();
        let registry = client(&mock, cache.path());
        let source = tempfile::tempdir().unwrap();
        artifact(source.path(), &digest);
        let result =
            Artifact::load_for_host_with_registry(source.path(), "test", &host(), Some(&registry));
        if failed == "index" && status == 404 {
            assert!(result.unwrap().snapshot.is_none());
        } else {
            let error = result.err().expect("only index HTTP 404 permits fallback");
            assert!(!format!("{error:#}").contains("private response details"));
        }
        assert!(!cache.path().join(&digest[7..]).join("index.json").exists());
    }
}

#[test]
fn slow_blob_downloads_are_bounded_by_stalls_not_total_time() {
    let fixture = fixture();
    let digest = fixture.digest.clone();
    let layer = fixture.layer.digest.clone();
    let mock = Mock::new(move |request| {
        let reply = Reply::new(200, fixture.routes.get(&request.path).unwrap().clone());
        if request.path.ends_with(&layer) {
            reply.paced(Duration::from_millis(100))
        } else {
            reply
        }
    });
    let cache = tempfile::tempdir().unwrap();
    let mut registry = client(&mock, cache.path());
    // The 15-byte memory layer takes ~1.5 s at 100 ms per byte: longer than the timeout, but every
    // chunk arrives well within it. Stalls are covered by the client's read timeout as well.
    registry.timeout = Duration::from_secs(1);
    assert!(registry.pull(&digest, &host()).unwrap().is_some());
}

#[test]
fn selected_missing_corrupt_truncated_and_oversized_content_fail() {
    for failure in [
        "missing",
        "corrupt",
        "short",
        "large",
        "index",
        "oversized-index",
    ] {
        let fixture = fixture();
        let digest = fixture.digest.clone();
        let layer = fixture.layer.digest.clone();
        let mock = Mock::new(move |request| {
            if request.path.ends_with(&layer) {
                match failure {
                    "missing" => return Reply::new(404, vec![]),
                    "corrupt" => return Reply::new(200, vec![b'x'; fixture.layer.size as usize]),
                    "short" => return Reply::new(200, vec![b'x'; fixture.layer.size as usize - 1]),
                    "large" => return Reply::new(200, vec![b'x'; fixture.layer.size as usize + 1]),
                    _ => {}
                }
            }
            if request.path.ends_with(&fixture.digest) {
                match failure {
                    "index" => return Reply::new(200, b"wrong index".to_vec()),
                    "oversized-index" => return Reply::new(200, vec![b'x'; PAYLOAD_LIMIT + 1]),
                    _ => {}
                }
            }
            Reply::new(200, fixture.routes.get(&request.path).unwrap().clone())
        });
        let cache = tempfile::tempdir().unwrap();
        let registry = client(&mock, cache.path());
        assert!(registry.pull(&digest, &host()).is_err(), "{failure}");
        assert!(!cache.path().join(&digest[7..]).join("index.json").exists());
        assert!(
            std::fs::read_dir(cache.path().join(&digest[7..]))
                .unwrap()
                .all(|e| !e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".pull-"))
        );
    }
}

#[test]
fn malformed_metadata_and_descriptor_limits_fail_before_layer_download() {
    for failure in [
        "manifest-size",
        "layer-size",
        "transport-size",
        "layer-digest",
        "second-layer-digest",
        "manifest-format",
        "config-content",
        "config-mismatch",
        "index-format",
        "variant-count",
    ] {
        let mut fixture = fixture();
        let index_path = format!("/v2/provider/test/manifests/{}", fixture.digest);
        let mut index: SnapshotIndex =
            serde_json::from_slice(&fixture.routes.remove(&index_path).unwrap()).unwrap();
        let selected = &mut index.manifests[0];
        let manifest_path = format!("/v2/provider/test/manifests/{}", selected.digest);
        let mut manifest: SnapshotManifest =
            serde_json::from_slice(&fixture.routes.remove(&manifest_path).unwrap()).unwrap();
        match failure {
            "manifest-size" => selected.size = PAYLOAD_LIMIT as u64 + 1,
            "layer-size" => manifest.layers[0].size = (4 << 30) + 1,
            "transport-size" => manifest.layers[2].size = (2 << 20) + 1,
            "second-layer-digest" => manifest.layers[1].digest = "sha256:../escape".into(),
            "layer-digest" => manifest.layers[0].digest = "sha256:../escape".into(),
            "manifest-format" => manifest.artifact_type = "unrelated".into(),
            "config-content" => {
                manifest.config = descriptor(b"[]", CONFIG);
                fixture.routes.insert(
                    format!("/v2/provider/test/blobs/{}", manifest.config.digest),
                    b"[]".to_vec(),
                );
            }
            "config-mismatch" => {
                let config =
                    br#"{"arch":"aarch64","hypervisor":"kvm","cpu_vendor":"GenuineIntel"}"#;
                manifest.config = descriptor(config, CONFIG);
                fixture.routes.insert(
                    format!("/v2/provider/test/blobs/{}", manifest.config.digest),
                    config.to_vec(),
                );
            }
            "index-format" => index.schema_version = 1,
            "variant-count" => index.manifests = vec![selected.clone(); 129],
            _ => unreachable!(),
        }
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let mut manifest_descriptor = descriptor(&bytes, MANIFEST);
        if failure == "manifest-size" {
            manifest_descriptor.size = PAYLOAD_LIMIT as u64 + 1;
        }
        let selected = &mut index.manifests[0];
        selected.digest = manifest_descriptor.digest.clone();
        selected.size = manifest_descriptor.size;
        fixture.routes.insert(
            format!("/v2/provider/test/manifests/{}", manifest_descriptor.digest),
            bytes,
        );
        let bytes = serde_json::to_vec(&index).unwrap();
        let digest = descriptor(&bytes, INDEX).digest;
        fixture
            .routes
            .insert(format!("/v2/provider/test/manifests/{digest}"), bytes);
        let config = manifest.config.digest.clone();
        let mock = Mock::new(move |request| {
            // The config is the only blob metadata may fetch: no layer.
            assert!(
                !request.path.contains("/blobs/") || request.path.ends_with(&config),
                "invalid metadata must not download a layer: {}",
                request.path
            );
            Reply::new(200, fixture.routes.get(&request.path).unwrap().clone())
        });
        let cache = tempfile::tempdir().unwrap();
        let registry = client(&mock, cache.path());
        assert!(registry.pull(&digest, &host()).is_err(), "{failure}");
    }
}

#[test]
fn corrupt_cache_is_an_error_not_a_download_or_cold_boot() {
    let fixture = fixture();
    let digest = fixture.digest.clone();
    let layer_hash = fixture.layer.digest[7..].to_owned();
    let mock = Mock::new(move |request| {
        Reply::new(200, fixture.routes.get(&request.path).unwrap().clone())
    });
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let layout = registry.pull(&digest, &host()).unwrap().unwrap();
    drop(mock);
    std::fs::write(layout.join("blobs/sha256").join(layer_hash), b"bad").unwrap();
    let error = registry.pull(&digest, &host()).unwrap_err();
    assert!(format!("{error:#}").contains("cached snapshot blob"));
}

#[test]
fn cached_metadata_is_checked_before_network_and_never_replaced() {
    for (name, contents) in [
        ("index.json", b"bad".as_slice()),
        ("oci-layout", b"bad".as_slice()),
        (
            "oci-layout",
            br#"{"imageLayoutVersion":"2.0.0"}"#.as_slice(),
        ),
    ] {
        let fixture = fixture();
        let digest = fixture.digest.clone();
        let calls = Arc::new(Mutex::new(0));
        let requests = calls.clone();
        let mock = Mock::new(move |request| {
            *requests.lock().unwrap() += 1;
            Reply::new(200, fixture.routes.get(&request.path).unwrap().clone())
        });
        let cache = tempfile::tempdir().unwrap();
        let registry = client(&mock, cache.path());
        let layout = registry.pull(&digest, &host()).unwrap().unwrap();
        // Even an incomplete cache must reject metadata before downloading.
        std::fs::remove_file(layout.join("blobs/sha256").join(&digest[7..])).unwrap();
        std::fs::write(layout.join(name), contents).unwrap();
        let before = *calls.lock().unwrap();
        assert!(registry.pull(&digest, &host()).is_err(), "{name}");
        assert_eq!(*calls.lock().unwrap(), before);
        assert_eq!(std::fs::read(layout.join(name)).unwrap(), contents);
    }
}

#[cfg(unix)]
#[test]
fn symlinked_cache_metadata_is_rejected_without_replacement() {
    use std::os::unix::fs::symlink;
    for name in ["index.json", "oci-layout"] {
        let fixture = fixture();
        let digest = fixture.digest.clone();
        let mock = Mock::new(move |request| {
            Reply::new(200, fixture.routes.get(&request.path).unwrap().clone())
        });
        let cache = tempfile::tempdir().unwrap();
        let registry = client(&mock, cache.path());
        let layout = registry.pull(&digest, &host()).unwrap().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(outside.path(), std::fs::read(layout.join(name)).unwrap()).unwrap();
        std::fs::remove_file(layout.join(name)).unwrap();
        symlink(outside.path(), layout.join(name)).unwrap();
        drop(mock);
        assert!(registry.pull(&digest, &host()).is_err());
        assert!(
            std::fs::symlink_metadata(layout.join(name))
                .unwrap()
                .is_symlink()
        );
    }
}

#[cfg(unix)]
#[test]
fn cache_staging_renames_the_same_private_file_and_cleans_failures() {
    use cap_std::fs::{MetadataExt as _, PermissionsExt as _};
    use std::os::unix::fs::MetadataExt as _;
    let cache = tempfile::tempdir().unwrap();
    let root = Dir::open_ambient_dir(cache.path(), ambient_authority()).unwrap();
    let mut identity = (0, 0);
    let mut file = stage(&root, "blob", |file| {
        let metadata = file.metadata()?;
        identity = (metadata.dev(), metadata.ino());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        assert_eq!(
            metadata.dev(),
            std::fs::metadata(cache.path()).unwrap().dev()
        );
        file.write_all(b"verified blob")?;
        Ok(())
    })
    .unwrap();
    let metadata = std::fs::metadata(cache.path().join("blob")).unwrap();
    assert_eq!(identity, (metadata.dev(), metadata.ino()));
    assert_eq!(read_limited(&mut file, 64).unwrap(), b"verified blob");
    assert!(
        stage(&root, "failed", |file| {
            file.write_all(b"partial")?;
            anyhow::bail!("verification failed")
        })
        .is_err()
    );
    let entries: Vec<_> = std::fs::read_dir(cache.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("blob")]);
}

#[test]
fn stale_staging_files_from_killed_pulls_are_removed() {
    let fixture = fixture();
    let digest = fixture.digest.clone();
    let mock = Mock::new(move |request| {
        Reply::new(200, fixture.routes.get(&request.path).unwrap().clone())
    });
    let cache = tempfile::tempdir().unwrap();
    let layout = cache.path().join(&digest[7..]);
    std::fs::create_dir_all(&layout).unwrap();
    std::fs::set_permissions(
        cache.path(),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(layout.join(".pull-0123456789abcdef"), b"partial").unwrap();
    let registry = client(&mock, cache.path());
    registry.pull(&digest, &host()).unwrap().unwrap();
    assert!(!layout.join(".pull-0123456789abcdef").exists());
}

#[test]
fn offline_pulls_preserve_valid_metadata_and_return_the_selected_digest() {
    let mut fixture = fixture();
    let old_index = format!("/v2/provider/test/manifests/{}", fixture.digest);
    let mut index: SnapshotIndex =
        serde_json::from_slice(fixture.routes.get(&old_index).unwrap()).unwrap();
    let expected = index.manifests[0].digest.clone();
    let mut incompatible = index.manifests[0].clone();
    incompatible.digest = format!("sha256:{}", "0".repeat(64));
    incompatible.annotations.insert(
        "dev.hyperlight.snapshot.cpu.vendor".into(),
        "AuthenticAMD".into(),
    );
    index.manifests.insert(0, incompatible);
    let bytes = serde_json::to_vec(&index).unwrap();
    fixture.digest = descriptor(&bytes, INDEX).digest;
    fixture.routes.insert(
        format!("/v2/provider/test/manifests/{}", fixture.digest),
        bytes,
    );
    let digest = fixture.digest.clone();
    let mock = Mock::new(move |request| {
        Reply::new(200, fixture.routes.get(&request.path).unwrap().clone())
    });
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let layout = registry.pull_selected(&digest, &host()).unwrap().unwrap();
    let index = std::fs::read(layout.path.join("index.json")).unwrap();
    assert_eq!(layout.digest.as_ref(), Some(&expected));
    let marker = b"{ \"imageLayoutVersion\": \"1.0.0\" }\n";
    std::fs::write(layout.path.join("oci-layout"), marker).unwrap();
    drop(mock);
    let again = registry.pull_selected(&digest, &host()).unwrap().unwrap();
    assert_eq!(again.digest, layout.digest);
    let source = tempfile::tempdir().unwrap();
    artifact(source.path(), &digest);
    let loaded =
        Artifact::load_for_host_with_registry(source.path(), "test", &host(), Some(&registry))
            .unwrap();
    assert_eq!(loaded.snapshot_digest.as_ref(), Some(&expected));
    assert_eq!(loaded.snapshot.as_ref(), Some(&layout.path));
    assert_eq!(
        std::fs::read(layout.path.join("index.json")).unwrap(),
        index
    );
    assert_eq!(
        std::fs::read(layout.path.join("oci-layout")).unwrap(),
        marker
    );
}

#[cfg(unix)]
#[test]
fn helper_basic_and_bearer_credentials_are_used_without_exposing_secrets() {
    for bearer in [false, true] {
        let fixture = fixture();
        let digest = fixture.digest.clone();
        let realm = Arc::new(Mutex::new(String::new()));
        let token_realm = realm.clone();
        let mock = Mock::new(move |request| {
            if request.path.starts_with("/token") {
                assert_eq!(
                    request.authorization.as_deref(),
                    Some("Basic dXNlcjpzZWNyZXQ=")
                );
                assert!(
                    request
                        .path
                        .contains("scope=repository%3Aprovider%2Ftest%3Apull")
                );
                return Reply::new(200, br#"{"access_token":"test-token"}"#.to_vec());
            }
            let expected = if bearer {
                "Bearer test-token"
            } else {
                "Basic dXNlcjpzZWNyZXQ="
            };
            if request.authorization.as_deref() != Some(expected) {
                let challenge = if bearer {
                    format!(
                        "Bearer realm=\"{}/token\",service=\"local,registry\"",
                        token_realm.lock().unwrap()
                    )
                } else {
                    "Basic realm=\"local\"".into()
                };
                return Reply::new(401, vec![]).header("WWW-Authenticate", &challenge);
            }
            Reply::new(200, fixture.routes.get(&request.path).unwrap().clone())
        });
        *realm.lock().unwrap() = format!("http://{}", mock.address);
        let cache = tempfile::tempdir().unwrap();
        let registry = client(&mock, cache.path());
        let mut session = Session::new(&registry).unwrap();
        let reference = session.reference(&digest).unwrap();
        session
            .authenticate_with(&reference, Some(test_helper_credentials(&mock.address)))
            .unwrap();
        assert!(session.manifest(&digest, INDEX, false).unwrap().is_some());
    }
}

#[test]
fn references_keep_the_configured_registry_host() {
    let cache = tempfile::tempdir().unwrap();
    let digest = format!("sha256:{}", "a".repeat(64));
    for (repository, server, name) in [
        ("myregistry/team/snapshots", "myregistry", "team/snapshots"),
        ("https://[::1]:5000/provider", "[::1]:5000", "provider"),
        (
            "docker.io/example/provider",
            "registry-1.docker.io",
            "example/provider",
        ),
    ] {
        let registry = SnapshotRegistry::new(repository, cache.path()).unwrap();
        let session = Session::new(&registry).unwrap();
        let reference = session.reference(&digest).unwrap();
        assert_eq!(reference.registry(), server, "{repository}");
        assert_eq!(reference.resolve_registry(), server, "{repository}");
        assert_eq!(reference.repository(), name, "{repository}");
        assert_eq!(reference.digest(), Some(digest.as_str()), "{repository}");
    }
}

#[test]
fn docker_config_errors_are_redacted_and_helpers_take_precedence() {
    let docker = tempfile::tempdir().unwrap();
    let file = docker.path().join("config.json");
    for content in [
        json!({"auths":{"registry.example": "secret-wrong-type"}}),
        json!({"credsStore": ["secret-wrong-type"]}),
    ] {
        std::fs::write(&file, serde_json::to_vec(&content).unwrap()).unwrap();
        let error = credentials(
            Some(docker.path()),
            "registry.example",
            &Cancellation::default(),
        )
        .err()
        .unwrap();
        assert!(!format!("{error:#}").contains("secret"));
    }
    std::fs::write(
        &file,
        serde_json::to_vec(&json!({
            "auths": {"registry.example": {"auth": "dXNlcjpzZWNyZXQ="}},
            "credHelpers": {"registry.example": "lampyr-test-missing-helper"},
            "credsStore": "also-missing-helper"
        }))
        .unwrap(),
    )
    .unwrap();
    let error = credentials(
        Some(docker.path()),
        "registry.example",
        &Cancellation::default(),
    )
    .err()
    .unwrap();
    assert!(format!("{error:#}").contains("helper is unavailable"));
    std::fs::write(
        &file,
        serde_json::to_vec(&json!({
            "auths": {"https://index.docker.io/v1/": {"auth": "dXNlcjpzZWNyZXQ="}}
        }))
        .unwrap(),
    )
    .unwrap();
    assert!(
        credentials(
            Some(docker.path()),
            "registry-1.docker.io",
            &Cancellation::default()
        )
        .is_err()
    );
    let cache = tempfile::tempdir().unwrap();
    let registry = SnapshotRegistry::new("docker.io/example/provider", cache.path()).unwrap();
    assert_eq!(registry.server, "registry-1.docker.io");
}

#[test]
fn empty_docker_helpers_disable_helpers_but_reject_inline_credentials() {
    let docker = tempfile::tempdir().unwrap();
    for settings in [
        json!({"credsStore": ""}),
        json!({"credHelpers": {"registry.example": ""}, "credsStore": "missing-global-helper"}),
        json!({"credHelpers": {"https://index.docker.io/v1/": ""}, "credsStore": "missing-global-helper"}),
    ] {
        let server = if settings["credHelpers"]
            .get("https://index.docker.io/v1/")
            .is_some()
        {
            "registry-1.docker.io"
        } else {
            "registry.example"
        };
        let mut config = settings;
        config["auths"] = json!({server: {"auth": "dXNlcjpzZWNyZXQ="}});
        std::fs::write(
            docker.path().join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let error = credentials(Some(docker.path()), server, &Cancellation::default())
            .err()
            .unwrap();
        assert!(error.to_string().contains("inline Docker credentials"));
        config["auths"] = json!({server: {}});
        std::fs::write(
            docker.path().join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        assert!(
            credentials(Some(docker.path()), server, &Cancellation::default())
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn cancellation_interrupts_registry_headers_bodies_and_token_exchange() {
    for stage in ["headers", "index-body", "blob-body", "token-body"] {
        check_registry_cancellation(stage, Duration::ZERO);
    }
}

#[test]
fn cancellation_fixture_startup_does_not_consume_the_cancellation_deadline() {
    // Simulate a fixture thread that cannot run promptly, without loading the host.
    check_registry_cancellation("headers", Duration::from_secs(4));
}

fn check_registry_cancellation(stage: &'static str, fixture_startup_delay: Duration) {
    let fixture = fixture();
    let digest = fixture.digest.clone();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (ready, waiting) = std::sync::mpsc::channel();
    let server = std::thread::spawn(move || {
        std::thread::sleep(fixture_startup_delay);
        loop {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(3)))
                .unwrap();
            let request = request(&mut stream);
            let token = stage == "token-body";
            if token && request.path.starts_with("/v2/") {
                write!(stream, "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\nWWW-Authenticate: Bearer realm=\"http://{address}/token\"\r\n\r\n").unwrap();
                continue;
            }
            let stall = match stage {
                "headers" => true,
                "index-body" => request.path != "/v2/",
                "blob-body" => request.path.ends_with(&fixture.layer.digest),
                _ => request.path.starts_with("/token"),
            };
            if stall {
                if stage != "headers" {
                    stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10000\r\nConnection: close\r\n\r\n{").unwrap();
                }
                ready.send(()).unwrap();
                let mut byte = [0];
                // Cancellation must shut down the client connection, not wait for this timeout.
                let _ = stream.read(&mut byte);
                break;
            }
            let body = fixture.routes.get(&request.path).unwrap();
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .unwrap();
            stream.write_all(body).unwrap();
        }
    });
    let cache = tempfile::tempdir().unwrap();
    let cancellation = Cancellation::default();
    let mut registry =
        SnapshotRegistry::new(&format!("http://{address}/provider/test"), cache.path())
            .unwrap()
            .with_cancellation(cancellation.clone());
    registry.docker_config = None;
    let cancel = std::thread::spawn(move || {
        // Fixture startup is not part of the cancellation deadline.
        waiting.recv().unwrap();
        let started = Instant::now();
        cancellation.cancel();
        started
    });
    let error = registry.pull(&digest, &host()).unwrap_err();
    let finished = Instant::now();
    assert!(
        format!("{error:#}").contains("cancelled"),
        "{stage}: {error:#}"
    );
    let cancelled = cancel.join().unwrap();
    assert!(
        finished.duration_since(cancelled) < Duration::from_secs(1),
        "{stage}: {error:#}"
    );
    assert!(!cache.path().join(&digest[7..]).join("index.json").exists());
    server.join().unwrap();
}

#[cfg(unix)]
#[test]
fn cancellation_terminates_running_credential_helper() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("helper");
    let ready = temp.path().join("ready");
    std::fs::write(
        &script,
        format!("#!/bin/sh\n: > '{}'\nsleep 30\n", ready.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let cancellation = Cancellation::default();
    let token = cancellation.clone();
    let cancel = std::thread::spawn(move || {
        let started = Instant::now();
        while !ready.exists() && started.elapsed() < Duration::from_secs(2) {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(ready.exists(), "helper never started");
        token.cancel();
    });
    let started = Instant::now();
    assert!(helper_credentials(&script, "registry.example", &cancellation).is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    cancel.join().unwrap();
}

#[test]
fn unsafe_repositories_digests_and_symlink_cache_paths_are_rejected() {
    let cache = tempfile::tempdir().unwrap();
    for repository in [
        "http://example.com/a",
        "https://user:pass@example.com/a",
        "https://example.com/a:tag",
        "https://example.com/a?query",
        "https://example.com/a@sha256:abc",
        "https://example.com/",
        "https://example.com/a/%2f..",
    ] {
        assert!(
            SnapshotRegistry::new(repository, cache.path()).is_err(),
            "{repository}"
        );
    }
    let mock = Mock::new(|_| panic!("invalid digests must not access the registry"));
    let registry = client(&mock, cache.path());
    for digest in ["sha256:../escape", "sha512:abcd", "sha256:AAAA", ""] {
        assert!(registry.pull(digest, &host()).is_err());
    }
    #[cfg(unix)]
    {
        let digest = format!("sha256:{}", "a".repeat(64));
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), cache.path().join(&digest[7..])).unwrap();
        assert!(registry.pull(&digest, &host()).is_err());
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }
}

#[cfg(unix)]
#[test]
fn symlink_blobs_and_busy_cache_fail_without_network_or_outside_writes() {
    let mock = Mock::new(|_| panic!("unsafe or locked cache must not access the registry"));
    let digest = format!("sha256:{}", "a".repeat(64));
    for symlink in ["blobs", "blobs/sha256", "blobs/sha256/blob", ".lock"] {
        let cache = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let layout = cache.path().join(&digest[7..]);
        let path = if symlink.ends_with("/blob") {
            layout.join("blobs/sha256").join(&digest[7..])
        } else {
            layout.join(symlink)
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(outside.path(), &path).unwrap();
        let registry = client(&mock, cache.path());
        assert!(registry.pull(&digest, &host()).is_err(), "{symlink}");
        assert!(std::fs::read_dir(outside.path()).unwrap().next().is_none());
    }
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let layout = cache.path().join(&digest[7..]);
    std::fs::create_dir(&layout).unwrap();
    let lock = std::fs::File::create(layout.join(".lock")).unwrap();
    lock.lock().unwrap();
    assert!(
        format!("{:#}", registry.pull(&digest, &host()).unwrap_err()).contains("cache is in use")
    );
}

#[cfg(unix)]
#[test]
fn credential_helper_protocol_is_bounded_and_failures_are_redacted() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let script = temp.path().join("docker-credential-test");
    std::fs::write(&script, "#!/bin/sh\n[ \"$1\" = get ] || exit 1\nread server\n[ \"$server\" = registry.example ] || exit 1\nprintf '%s' '{\"Username\":\"user\",\"Secret\":\"secret\"}'\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let credentials = helper_credentials(&script, "registry.example", &Cancellation::default())
        .unwrap()
        .unwrap();
    assert_eq!(
        credentials.basic().unwrap().as_str(),
        "Basic dXNlcjpzZWNyZXQ="
    );
    std::fs::write(
        &script,
        "#!/bin/sh\necho 'secret output'; echo 'secret error' >&2; exit 1\n",
    )
    .unwrap();
    let error = helper_credentials(&script, "registry.example", &Cancellation::default())
        .err()
        .unwrap();
    assert!(!format!("{error:#}").contains("secret"));
    std::fs::write(&script, "#!/bin/sh\nprintf '%s' '\"secret-json-string\"'\n").unwrap();
    let error = helper_credentials(&script, "registry.example", &Cancellation::default())
        .err()
        .unwrap();
    assert!(!format!("{error:#}").contains("secret"));
    std::fs::write(
        &script,
        "#!/bin/sh\nprintf '%s' 'credentials not found in native keychain'; exit 1\n",
    )
    .unwrap();
    assert!(
        helper_credentials(&script, "registry.example", &Cancellation::default())
            .unwrap()
            .is_none()
    );
    std::fs::write(&script, "#!/bin/sh\nhead -c 70000 /dev/zero\n").unwrap();
    assert!(helper_credentials(&script, "registry.example", &Cancellation::default()).is_err());
}

#[test]
fn rejected_token_requests_block_index_404_fallback() {
    let probes = Arc::new(Mutex::new(0));
    let counter = probes.clone();
    let origin = Arc::new(Mutex::new(String::new()));
    let realm = origin.clone();
    let mock = Mock::new(move |request| {
        if request.path == "/v2/" {
            let mut probes = counter.lock().unwrap();
            *probes += 1;
            if *probes == 1 {
                Reply::new(200, b"{}".to_vec())
            } else {
                Reply::new(401, vec![]).header(
                    "WWW-Authenticate",
                    &format!("Bearer realm=\"{}/token\"", realm.lock().unwrap()),
                )
            }
        } else if request.path.starts_with("/token") {
            Reply::new(401, b"denied".to_vec())
        } else {
            Reply::new(404, vec![])
        }
    });
    *origin.lock().unwrap() = format!("http://{}", mock.address);
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let result = registry.pull(&fixture().digest, &host());
    assert!(*probes.lock().unwrap() >= 2);
    assert!(
        result.is_err(),
        "silently cold-booted: {:?}",
        result.map(|r| r.is_none())
    );
}

#[test]
fn unsafe_token_realms_block_index_404_fallback() {
    let probes = Arc::new(Mutex::new(0));
    let counter = probes.clone();
    let mock = Mock::new(move |request| {
        if request.path == "/v2/" {
            let mut probes = counter.lock().unwrap();
            *probes += 1;
            if *probes == 1 {
                Reply::new(200, b"{}".to_vec())
            } else {
                Reply::new(401, vec![]).header(
                    "WWW-Authenticate",
                    "Bearer realm=\"http://attacker.example/token\"",
                )
            }
        } else {
            Reply::new(404, vec![])
        }
    });
    let cache = tempfile::tempdir().unwrap();
    let registry = client(&mock, cache.path());
    let result = registry.pull(&fixture().digest, &host());
    assert!(*probes.lock().unwrap() >= 2);
    assert!(
        result.is_err(),
        "silently cold-booted: {:?}",
        result.map(|r| r.is_none())
    );
}
