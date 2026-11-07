// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Pull-only registry support for provider snapshot OCI layouts.
use std::collections::BTreeMap;
use std::error::Error as _;
use std::fmt;
use std::future::Future;
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use cap_std::{ambient_authority, fs::Dir};
use futures_util::StreamExt;
use oci_client::{
    Client, Reference,
    client::{ClientConfig, ClientProtocol},
    errors::OciDistributionError,
    secrets::RegistryAuth,
    transport,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;
use zeroize::{Zeroize, Zeroizing};

use crate::artifact::{HostCompatibility, OciDescriptor, SnapshotIndex, SnapshotManifest};
use crate::runtime::{Cancellation, PAYLOAD_LIMIT};

const INDEX: &str = "application/vnd.oci.image.index.v1+json";
const LAYOUT: &[u8] = br#"{"imageLayoutVersion":"1.0.0"}"#;
const LOCK_WAIT: Duration = Duration::from_secs(2);
#[cfg(test)]
static TEST_DEFAULT_CACHE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// A pulled snapshot layout; `digest` names the selected variant, or is None when no variant
/// matches this host and the provider will cold-boot.
pub struct PulledLayout {
    pub path: PathBuf,
    pub digest: Option<String>,
}

/// An explicit repository, not a tag. The provider manifest supplies the index digest.
pub struct SnapshotRegistry {
    repository: Url,
    name: String,
    server: String,
    docker_config: Option<PathBuf>,
    cache: PathBuf,
    cancellation: Cancellation,
    timeout: Duration,
}

#[derive(Debug)]
struct RegistryUnavailable(String);

impl fmt::Display for RegistryUnavailable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for RegistryUnavailable {}

fn unavailable(message: impl Into<String>) -> anyhow::Error {
    RegistryUnavailable(message.into()).into()
}

impl SnapshotRegistry {
    pub fn new(repository: &str, cache: impl Into<PathBuf>) -> Result<Self> {
        let mut repository = if repository.contains("://") {
            Url::parse(repository)
        } else {
            Url::parse(&format!("https://{repository}"))
        }
        .context("invalid snapshot registry repository")?;
        safe_url(&repository)?;
        if matches!(repository.host_str(), Some("docker.io" | "index.docker.io"))
            && repository.port().is_none()
        {
            repository
                .set_host(Some("registry-1.docker.io"))
                .map_err(|_| anyhow::anyhow!("invalid Docker Hub registry host"))?;
        }
        ensure!(
            repository.query().is_none()
                && repository.fragment().is_none()
                && repository.path().len() <= 255,
            "snapshot registry must be HOST/REPOSITORY without a tag, digest or query"
        );
        let name = repository.path().trim_start_matches('/').to_owned();
        ensure!(
            !name.is_empty()
                && name.split('/').all(|part| {
                    !part.is_empty()
                        && part
                            .bytes()
                            .next()
                            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                        && part
                            .bytes()
                            .last()
                            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                        && part.bytes().all(|b| {
                            b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b)
                        })
                        && !part.contains("..")
                }),
            "invalid snapshot registry repository name"
        );
        let server = repository[url::Position::BeforeHost..url::Position::AfterPort].to_owned();
        let docker_config = std::env::var_os("DOCKER_CONFIG")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".docker")));
        Ok(Self {
            repository,
            name,
            server,
            docker_config,
            cache: cache.into(),
            cancellation: Cancellation::default(),
            timeout: Duration::from_secs(120),
        })
    }

    pub fn from_env() -> Result<Option<Self>> {
        let Some(repository) = std::env::var_os("LAMPYR_SNAPSHOT_REGISTRY") else {
            return Ok(None);
        };
        let repository = repository
            .to_str()
            .context("LAMPYR_SNAPSHOT_REGISTRY must be UTF-8")?;
        Self::new(repository, Self::default_cache()?).map(Some)
    }

    pub fn default_cache() -> Result<PathBuf> {
        #[cfg(test)]
        if let Some(path) = TEST_DEFAULT_CACHE
            .lock()
            .map_err(|_| anyhow::anyhow!("snapshot test cache lock is unavailable"))?
            .clone()
        {
            return Ok(path);
        }
        Ok(crate::platform::home_dir()?.join("cache/provider-snapshots"))
    }

    fn blob_route(&self) -> (String, String) {
        (self.server.clone(), format!("/v2/{}/blobs/", self.name))
    }

    pub fn with_cancellation(mut self, cancellation: Cancellation) -> Self {
        self.cancellation = cancellation;
        self
    }

    pub(crate) fn is_unavailable(error: &anyhow::Error) -> bool {
        error
            .chain()
            .any(|cause| cause.downcast_ref::<RegistryUnavailable>().is_some())
    }

    /// Returns None only for an absent registry index (HTTP 404).
    pub fn pull(&self, digest: &str, host: &HostCompatibility) -> Result<Option<PathBuf>> {
        Ok(self.pull_selected(digest, host)?.map(|layout| layout.path))
    }

    pub fn pull_selected(
        &self,
        digest: &str,
        host: &HostCompatibility,
    ) -> Result<Option<PulledLayout>> {
        ensure!(!self.cancellation.is_cancelled(), "snapshot pull cancelled");
        let result = self.pull_inner(digest, host);
        ensure!(!self.cancellation.is_cancelled(), "snapshot pull cancelled");
        result
    }

    fn pull_inner(&self, digest: &str, host: &HostCompatibility) -> Result<Option<PulledLayout>> {
        let index_descriptor = OciDescriptor {
            media_type: INDEX.into(),
            digest: digest.into(),
            size: 0,
            annotations: BTreeMap::new(),
        };
        let hash = index_descriptor.hash(PAYLOAD_LIMIT as u64)?;
        let private = crate::workspace::Directory::private(&self.cache)?;
        let root = Dir::open_ambient_dir(private.root(), ambient_authority())?;
        real_directory(&root, hash)?;
        let layout = root.open_dir(hash)?;
        real_directory(&layout, "blobs")?;
        real_directory(&layout.open_dir("blobs")?, "sha256")?;
        let mut options = cap_std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        }
        let lock = layout.open_with(".lock", &options)?.into_std();
        ensure!(
            lock.metadata()?.is_file(),
            "snapshot cache lock must be a regular file"
        );
        // Wait briefly: a child process forked by this or another ly process can hold the lock's open
        // file description until it execs, which is not a real competing pull.
        let deadline = Instant::now() + LOCK_WAIT;
        while let Err(error) = lock.try_lock() {
            ensure!(!self.cancellation.is_cancelled(), "snapshot pull cancelled");
            if Instant::now() >= deadline {
                return Err(anyhow::Error::new(error))
                    .context("snapshot cache is in use; retry this command");
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        sweep_staging(&layout)?;
        let existing_index = cached(
            &layout,
            "index.json",
            hash,
            None,
            PAYLOAD_LIMIT as u64,
            false,
        )?
        .map(|file| read_limited(file, PAYLOAD_LIMIT as u64))
        .transpose()?;
        let existing_marker = regular_file(&layout, "oci-layout")?;
        let has_marker = existing_marker.is_some();
        if let Some(marker) = existing_marker {
            let version: serde_json::Value =
                serde_json::from_slice(&read_limited(marker, PAYLOAD_LIMIT as u64)?)
                    .context("decode cached snapshot OCI layout marker")?;
            ensure!(
                version["imageLayoutVersion"] == "1.0.0",
                "unsupported cached snapshot OCI layout version"
            );
        }
        let mut session = Session::new(self)?;
        let index_path = format!("blobs/sha256/{hash}");
        let index = if let Some(file) = cached(
            &layout,
            &index_path,
            hash,
            None,
            PAYLOAD_LIMIT as u64,
            false,
        )? {
            read_limited(file, PAYLOAD_LIMIT as u64)?
        } else if let Some(bytes) = &existing_index {
            publish(&layout, &index_path, bytes)?;
            bytes.clone()
        } else {
            let Some(bytes) = session.manifest(digest, INDEX, true)? else {
                return Ok(None);
            };
            ensure!(
                hex(&Sha256::digest(&bytes)) == hash,
                "snapshot registry index checksum mismatch"
            );
            publish(&layout, &index_path, &bytes)?;
            bytes
        };
        let parsed: SnapshotIndex =
            serde_json::from_slice(&index).context("decode registry snapshot index")?;
        let selected = parsed.select(host)?;
        if let Some(descriptor) = selected {
            let bytes = read_limited(
                self.blob(
                    &layout,
                    &mut session,
                    descriptor,
                    PAYLOAD_LIMIT as u64,
                    "manifests",
                )?,
                PAYLOAD_LIMIT as u64,
            )?;
            let manifest: SnapshotManifest =
                serde_json::from_slice(&bytes).context("decode registry snapshot manifest")?;
            manifest.validate()?;
            manifest.config.hash(PAYLOAD_LIMIT as u64)?;
            for (layer, limit) in manifest.blobs() {
                layer.hash(limit)?;
            }
            let config = read_limited(
                self.blob(
                    &layout,
                    &mut session,
                    &manifest.config,
                    PAYLOAD_LIMIT as u64,
                    "blobs",
                )?,
                PAYLOAD_LIMIT as u64,
            )?;
            manifest.validate_config(&config, host)?;
            for (layer, limit) in manifest.blobs() {
                self.blob(&layout, &mut session, layer, limit, "blobs")?;
            }
        }
        ensure!(!self.cancellation.is_cancelled(), "snapshot pull cancelled");
        if !has_marker {
            publish(&layout, "oci-layout", LAYOUT)?;
        }
        if existing_index.is_none() {
            publish(&layout, "index.json", &index)?;
        }
        Ok(Some(PulledLayout {
            path: private.root().join(hash),
            digest: selected.map(|descriptor| descriptor.digest.clone()),
        }))
    }

    fn blob(
        &self,
        layout: &Dir,
        session: &mut Session<'_>,
        descriptor: &OciDescriptor,
        limit: u64,
        kind: &str,
    ) -> Result<std::fs::File> {
        ensure!(!self.cancellation.is_cancelled(), "snapshot pull cancelled");
        let hash = descriptor.hash(limit)?;
        let path = format!("blobs/sha256/{hash}");
        let remember = descriptor.media_type == crate::artifact::MEMORY;
        if let Some(file) = cached(layout, &path, hash, Some(descriptor.size), limit, remember)? {
            return Ok(file);
        }
        stage(layout, &path, |file| {
            session.download(descriptor, kind, file)
        })
    }
}

struct Session<'a> {
    registry: &'a SnapshotRegistry,
    runtime: tokio::runtime::Runtime,
    client: Client,
    auth: Option<RegistryAuth>,
    failures: Failures,
}

/// Paths and statuses (None for transport errors) of failed requests. oci-client drops errors from
/// the authentication it repeats before each request, so the index-404 fallback checks this log.
type Failures = std::sync::Arc<std::sync::Mutex<Vec<(String, Option<u16>)>>>;

/// `blobs` is the configured registry's `HOST[:PORT]` and `/v2/NAME/blobs/` prefix: only digest
/// requests under it stream, so a token endpoint chosen by the registry cannot opt out of the size
/// limit.
fn registry_client(
    config: ClientConfig,
    failures: Failures,
    blobs: (String, String),
) -> Result<Client> {
    let mut builder = reqwest::Client::builder()
        .https_only(matches!(&config.protocol, ClientProtocol::Https))
        .user_agent(config.user_agent)
        .redirect(reqwest::redirect::Policy::custom(|attempt| {
            if safe_url(attempt.url()).is_err()
                || (attempt
                    .previous()
                    .last()
                    .is_some_and(|url| url.scheme() == "https")
                    && attempt.url().scheme() != "https")
            {
                attempt.error("snapshot registry redirect requires HTTPS")
            } else {
                reqwest::redirect::Policy::default().redirect(attempt)
            }
        }));
    if let Some(timeout) = config.connect_timeout {
        builder = builder.connect_timeout(timeout);
    }
    if let Some(timeout) = config.read_timeout {
        builder = builder.read_timeout(timeout);
    }
    for certificate in &config.extra_root_certificates {
        builder = builder.tls_certs_merge([reqwest::Certificate::try_from(certificate)?]);
    }
    let client = builder.build().context("create snapshot HTTPS client")?;
    Ok(Client::new_with_transport(
        config,
        tower::service_fn(move |request: transport::Request| {
            let client = client.clone();
            let failures = failures.clone();
            let (blob_host, blob_prefix) = blobs.clone();
            async move {
                let path = request.uri().path().to_owned();
                // oci-client always adds a `scope` query to token requests and none to blob
                // downloads, so a registry cannot make its token endpoint look like a blob.
                let blob = request.uri().query().is_none()
                    && request
                        .uri()
                        .authority()
                        .map(|authority| authority.as_str())
                        == Some(blob_host.as_str())
                    && path
                        .strip_prefix(&blob_prefix)
                        .and_then(|digest| digest.strip_prefix("sha256:"))
                        .is_some_and(|hash| {
                            hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
                        });
                let record = |status: Option<u16>| {
                    if let Ok(mut failures) = failures.lock() {
                        failures.push((path.clone(), status));
                    }
                };
                let request = reqwest::Request::try_from(request).inspect_err(|_| record(None))?;
                safe_url(request.url()).inspect_err(|_| record(None))?;
                let response = client
                    .execute(request)
                    .await
                    .inspect_err(|_| record(None))?;
                // OCI auth consumes the /v2/ probe's 401 challenge; a 401 anywhere else (such as the
                // token endpoint) is a failure. Other failures must retain their HTTP status.
                if response.status() == reqwest::StatusCode::UNAUTHORIZED && path != "/v2/" {
                    record(Some(401));
                }
                if response.status() != reqwest::StatusCode::UNAUTHORIZED {
                    response
                        .error_for_status_ref()
                        .inspect_err(|_| record(Some(response.status().as_u16())))?;
                }
                if blob && response.status().is_success() {
                    // Blob bodies are streamed and bounded by their descriptor size; a blob's 401
                    // body is buffered by oci-client like any error, so it is bounded below.
                    return Ok::<_, transport::BoxError>(
                        http::Response::from(response).map(transport::Body::wrap),
                    );
                }
                // oci-client buffers manifest, token and error bodies in full, so bound them here.
                let mut builder = http::Response::builder()
                    .status(response.status())
                    .version(response.version());
                if let Some(headers) = builder.headers_mut() {
                    *headers = response.headers().clone();
                }
                let mut response = response;
                let mut body = Vec::new();
                while let Some(chunk) = response.chunk().await.inspect_err(|_| record(None))? {
                    body.extend_from_slice(&chunk);
                    if body.len() > PAYLOAD_LIMIT {
                        record(None);
                        return Err("snapshot registry response exceeds size limit".into());
                    }
                }
                Ok::<_, transport::BoxError>(
                    builder
                        .body(reqwest::Body::from(body))?
                        .map(transport::Body::wrap),
                )
            }
        }),
    ))
}

impl<'a> Session<'a> {
    fn new(registry: &'a SnapshotRegistry) -> Result<Self> {
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            // Another thread may install the same process-wide provider first.
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let config = ClientConfig {
            protocol: if registry.repository.scheme() == "http" {
                ClientProtocol::Http
            } else {
                ClientProtocol::Https
            },
            connect_timeout: Some(Duration::from_secs(10)),
            read_timeout: Some(registry.timeout),
            ..Default::default()
        };
        let failures = Failures::default();
        Ok(Self {
            registry,
            runtime: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("create snapshot registry runtime")?,
            client: registry_client(config, failures.clone(), registry.blob_route())?,
            auth: None,
            failures,
        })
    }

    fn reference(&self, digest: &str) -> Result<Reference> {
        // Parsing "HOST/NAME@DIGEST" applies Docker's rule that a dotless first component is a
        // Docker Hub namespace, which would send this registry's credentials to Docker Hub.
        Ok(Reference::with_digest(
            self.registry.server.clone(),
            self.registry.name.clone(),
            digest.to_owned(),
        ))
    }

    fn authenticate(&mut self, reference: &Reference) -> Result<()> {
        if self.auth.is_some() {
            return Ok(());
        }
        let credentials = credentials(
            self.registry.docker_config.as_deref(),
            &self.registry.server,
            &self.registry.cancellation,
        )?;
        self.authenticate_with(reference, credentials)
    }

    fn authenticate_with(
        &mut self,
        reference: &Reference,
        credentials: Option<Credentials>,
    ) -> Result<()> {
        let mut auth = match credentials {
            None => RegistryAuth::Anonymous,
            Some(credentials) => {
                ensure!(
                    credentials.username.as_str() != "<token>",
                    "Docker identity-token refresh is not supported by the OCI client; run docker login with a username and access token"
                );
                RegistryAuth::Basic(
                    credentials.username.to_string(),
                    credentials.secret.to_string(),
                )
            }
        };
        let result = run(&self.runtime, self.registry, async {
            self.client
                .auth(reference, &auth, oci_client::RegistryOperation::Pull)
                .await
                .map_err(registry_error)?;
            Ok(())
        });
        if let Err(error) = result {
            zeroize_auth(&mut auth);
            return Err(error);
        }
        self.auth = Some(auth);
        Ok(())
    }

    fn manifest(&mut self, digest: &str, media: &str, absent: bool) -> Result<Option<Vec<u8>>> {
        let reference = self.reference(digest)?;
        self.authenticate(&reference)?;
        let manifest_path = format!("/v2/{}/manifests/{digest}", self.registry.name);
        if let Ok(mut failures) = self.failures.lock() {
            failures.clear();
        }
        let result = run(&self.runtime, self.registry, async {
            match self
                .client
                .pull_manifest_raw(
                    &reference,
                    self.auth
                        .as_ref()
                        .context("missing OCI registry authentication")?,
                    &[media],
                )
                .await
            {
                Ok((bytes, _)) => {
                    ensure!(
                        bytes.len() <= PAYLOAD_LIMIT,
                        "snapshot registry manifest exceeds size limit"
                    );
                    Ok(Some(bytes.to_vec()))
                }
                Err(OciDistributionError::ServerError { code: 404, .. }) if absent => Ok(None),
                Err(OciDistributionError::RequestError(error))
                    if absent && error.status() == Some(reqwest::StatusCode::NOT_FOUND) =>
                {
                    Ok(None)
                }
                Err(error) => Err(registry_error(error)),
            }
        })?;
        if result.is_none() {
            // An index 404 permits cold boot only when no other request failed, including the
            // authentication oci-client repeats internally and does not report.
            let failures = self
                .failures
                .lock()
                .map_err(|_| anyhow::anyhow!("snapshot registry request log is unavailable"))?;
            ensure!(
                failures
                    .iter()
                    .all(|(path, status)| *path == manifest_path && *status == Some(404)),
                "snapshot registry authentication or request failed; check registry connectivity and docker login"
            );
        }
        Ok(result)
    }

    fn download(
        &mut self,
        descriptor: &OciDescriptor,
        kind: &str,
        file: &mut cap_std::fs::File,
    ) -> Result<()> {
        if kind == "manifests" {
            let bytes = self
                .manifest(&descriptor.digest, &descriptor.media_type, false)?
                .context("selected snapshot manifest is missing")?;
            ensure!(
                bytes.len() as u64 == descriptor.size,
                "snapshot registry blob size mismatch"
            );
            ensure!(
                hex(&Sha256::digest(&bytes)) == descriptor.hash(PAYLOAD_LIMIT as u64)?,
                "snapshot registry blob checksum mismatch"
            );
            file.write_all(&bytes)?;
            return Ok(());
        }
        let reference = self.reference(&descriptor.digest)?;
        self.authenticate(&reference)?;
        let native = oci_client::manifest::OciDescriptor {
            media_type: descriptor.media_type.clone(),
            digest: descriptor.digest.clone(),
            size: descriptor
                .size
                .try_into()
                .context("invalid snapshot blob size")?,
            ..Default::default()
        };
        // A memory layer can be gigabytes, so bound each wait for data instead of the whole transfer.
        let idle = self.registry.timeout;
        run_with(&self.runtime, self.registry, None, async {
            let mut stream =
                tokio::time::timeout(idle, self.client.pull_blob_stream(&reference, &native))
                    .await
                    .map_err(|_| unavailable("snapshot registry request timed out"))?
                    .map_err(registry_error)?;
            let mut total = 0_u64;
            while let Some(bytes) = tokio::time::timeout(idle, stream.next())
                .await
                .map_err(|_| unavailable("snapshot registry blob download stalled"))?
            {
                let bytes = bytes.map_err(blob_stream_error)?;
                total += bytes.len() as u64;
                ensure!(
                    total <= descriptor.size,
                    "snapshot registry blob exceeds descriptor size"
                );
                file.write_all(&bytes)?;
            }
            ensure!(
                total == descriptor.size,
                "snapshot registry blob size mismatch"
            );
            Ok(())
        })
    }
}

/// Best effort: oci-client keeps its own copies of Basic credentials and the bearer tokens it
/// obtains (its auth store, token cache and request headers), and those are not zeroized.
fn zeroize_auth(auth: &mut RegistryAuth) {
    if let RegistryAuth::Basic(username, secret) = auth {
        username.zeroize();
        secret.zeroize();
    }
}

impl Drop for Session<'_> {
    fn drop(&mut self) {
        if let Some(auth) = self.auth.as_mut() {
            zeroize_auth(auth);
        }
    }
}

fn run<T>(
    runtime: &tokio::runtime::Runtime,
    registry: &SnapshotRegistry,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    run_with(runtime, registry, Some(registry.timeout), future)
}

fn run_with<T>(
    runtime: &tokio::runtime::Runtime,
    registry: &SnapshotRegistry,
    deadline: Option<Duration>,
    future: impl Future<Output = Result<T>>,
) -> Result<T> {
    ensure!(
        !registry.cancellation.is_cancelled(),
        "snapshot pull cancelled"
    );
    let result = runtime.block_on(async {
        tokio::select! {
            result = async {
                match deadline {
                    Some(deadline) => tokio::time::timeout(deadline, future)
                        .await
                        .map_err(|_| unavailable("snapshot registry request timed out"))?,
                    None => future.await,
                }
            } => result,
            _ = async {
                while !registry.cancellation.is_cancelled() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            } => anyhow::bail!("snapshot pull cancelled"),
        }
    });
    ensure!(
        !registry.cancellation.is_cancelled(),
        "snapshot pull cancelled"
    );
    result
}

fn blob_stream_error(error: std::io::Error) -> anyhow::Error {
    if error
        .get_ref()
        .and_then(|source| source.downcast_ref::<oci_client::errors::DigestError>())
        .is_some()
    {
        anyhow::anyhow!("snapshot registry blob checksum mismatch")
    } else {
        unavailable("snapshot registry blob download was interrupted")
    }
}

fn unavailable_status(status: u16) -> bool {
    status == 401 || status == 403 || status == 408 || status == 429 || status >= 500
}

fn status_error(status: u16) -> anyhow::Error {
    let message = format!(
        "snapshot registry returned HTTP {status}; check repository, digest and docker login"
    );
    if unavailable_status(status) {
        unavailable(message)
    } else {
        anyhow::anyhow!(message)
    }
}

fn tls_error(error: &reqwest::Error) -> bool {
    let debug = format!("{error:?}").to_ascii_lowercase();
    if debug.contains("certificate")
        || debug.contains("unknownissuer")
        || debug.contains("unknownca")
        || debug.contains("tls")
        || debug.contains("ssl")
    {
        return true;
    }
    let mut cause = error.source();
    while let Some(current) = cause {
        let message = current.to_string().to_ascii_lowercase();
        if message.contains("certificate") || message.contains("tls") || message.contains("ssl") {
            return true;
        }
        cause = current.source();
    }
    false
}

fn transport_error(error: transport::BoxError) -> anyhow::Error {
    if let Some(request) = error.downcast_ref::<reqwest::Error>() {
        if let Some(status) = request.status() {
            return status_error(status.as_u16());
        }
        if (request.is_connect() || request.is_timeout()) && !tls_error(request) {
            return unavailable(
                "OCI snapshot request failed; check registry connectivity and docker login",
            );
        }
        return anyhow::anyhow!("snapshot registry HTTPS/TLS request was rejected");
    }
    let message = error.to_string();
    if message.contains("HTTPS") {
        anyhow::anyhow!("snapshot registry HTTPS/TLS policy rejected the response")
    } else if message.contains("exceeds size limit") {
        anyhow::anyhow!("snapshot registry response exceeds size limit")
    } else {
        anyhow::anyhow!("snapshot registry returned an invalid response")
    }
}

fn registry_error(error: OciDistributionError) -> anyhow::Error {
    let status = match &error {
        OciDistributionError::ServerError { code, .. } => Some(*code),
        OciDistributionError::RequestError(error) => error.status().map(|status| status.as_u16()),
        _ => None,
    };
    if let Some(status) = status {
        return status_error(status);
    }
    match error {
        OciDistributionError::UnauthorizedError { .. }
        | OciDistributionError::AuthenticationFailure(_) => unavailable(
            "snapshot registry authentication failed; check docker login and pull permissions",
        ),
        OciDistributionError::DigestError(_) => {
            anyhow::anyhow!("snapshot registry checksum mismatch")
        }
        OciDistributionError::RequestError(error)
            if (error.is_connect() || error.is_timeout()) && !tls_error(&error) =>
        {
            unavailable(
                "OCI snapshot request failed (network or HTTPS/TLS); check registry connectivity and docker login",
            )
        }
        OciDistributionError::RequestError(_) => {
            anyhow::anyhow!("snapshot registry HTTPS/TLS request was rejected")
        }
        OciDistributionError::TransportError(error) => transport_error(error),
        OciDistributionError::RegistryTokenDecodeError(_) => anyhow::anyhow!(
            "snapshot registry authentication response is invalid; check docker login"
        ),
        _ => anyhow::anyhow!(
            "snapshot registry returned an invalid OCI response; check repository and digest"
        ),
    }
}

fn real_directory(root: &Dir, path: &str) -> Result<()> {
    match root.create_dir(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).context("create snapshot cache directory"),
    }
    ensure!(
        root.symlink_metadata(path)?.is_dir(),
        "snapshot cache directory must not be a symlink"
    );
    Ok(())
}

fn stage(
    root: &Dir,
    path: &str,
    write: impl FnOnce(&mut cap_std::fs::File) -> Result<()>,
) -> Result<std::fs::File> {
    let mut random = [0; 16];
    getrandom::fill(&mut random)
        .map_err(|e| anyhow::anyhow!("create snapshot cache temporary name: {e}"))?;
    let name = format!(".pull-{}", hex(&random));
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true).write(true).create_new(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = root.open_with(&name, &options)?;
    let result = (|| -> Result<()> {
        write(&mut file)?;
        file.sync_all()?;
        file.rewind()?;
        root.rename(&name, root, path)?;
        Ok(())
    })();
    if let Err(error) = result {
        // Report why the write failed; a leftover file is swept by the next pull.
        let _ = root.remove_file(&name);
        return Err(error);
    }
    Ok(file.into_std())
}

/// Removes staging files left by a pull that was killed before it could clean up.
/// Callers hold the layout lock, so no other pull is writing them.
fn sweep_staging(root: &Dir) -> Result<()> {
    for entry in root.entries().context("list snapshot cache entries")? {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_str().is_some_and(|name| name.starts_with(".pull-"))
            && entry.file_type()?.is_file()
        {
            root.remove_file(&name)
                .context("remove stale snapshot cache staging file")?;
        }
    }
    Ok(())
}

fn publish(root: &Dir, path: &str, bytes: &[u8]) -> Result<()> {
    stage(root, path, |file| {
        file.write_all(bytes)?;
        Ok(())
    })?;
    Ok(())
}

fn cached(
    root: &Dir,
    path: &str,
    hash: &str,
    size: Option<u64>,
    limit: u64,
    remember: bool,
) -> Result<Option<std::fs::File>> {
    let Some(mut file) = regular_file(root, path)? else {
        return Ok(None);
    };
    let metadata = file.metadata()?;
    if remember
        && size.is_none_or(|size| metadata.len() == size)
        && crate::artifact::verified::contains(&metadata, hash)
    {
        return Ok(Some(file));
    }
    let mut input = (&mut file).take(limit + 1);
    let mut digest = Sha256::new();
    let mut total = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        total += count as u64;
        ensure!(total <= limit, "cached snapshot blob exceeds size limit");
        digest.update(&buffer[..count]);
    }
    ensure!(
        size.is_none_or(|size| total == size),
        "cached snapshot blob size mismatch"
    );
    ensure!(
        hex(digest.finalize().as_slice()) == hash,
        "cached snapshot blob checksum mismatch; remove the corrupt cache entry and retry"
    );
    if remember {
        crate::artifact::verified::insert(&metadata, hash);
    }
    file.rewind()?;
    Ok(Some(file))
}

fn regular_file(root: &Dir, path: &str) -> Result<Option<std::fs::File>> {
    match root.symlink_metadata(path) {
        Ok(metadata) => ensure!(
            metadata.is_file(),
            "snapshot cache blob must be a regular file, not a symlink"
        ),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("inspect snapshot cache blob"),
    }
    let mut options = cap_std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = root.open_with(path, &options)?.into_std();
    ensure!(
        file.metadata()?.is_file(),
        "snapshot cache blob must be a regular file, not a symlink"
    );
    Ok(Some(file))
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn read_limited(input: impl Read, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    input
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .context("read snapshot registry response")?;
    ensure!(
        bytes.len() as u64 <= limit,
        "snapshot registry response exceeds size limit"
    );
    Ok(bytes)
}

fn safe_url(url: &Url) -> Result<()> {
    ensure!(
        url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none()
            && (url.scheme() == "https"
                || (url.scheme() == "http"
                    && matches!(url.host_str(), Some("127.0.0.1" | "[::1]" | "::1")))),
        "snapshot registry URLs require HTTPS without embedded credentials (HTTP is allowed only on loopback)"
    );
    Ok(())
}

struct Credentials {
    username: Zeroizing<String>,
    secret: Zeroizing<String>,
}

fn credentials(
    directory: Option<&Path>,
    server: &str,
    cancellation: &Cancellation,
) -> Result<Option<Credentials>> {
    let Some(directory) = directory else {
        return Ok(None);
    };
    let file = match std::fs::File::open(directory.join("config.json")) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).context("read Docker configuration for snapshot registry"),
    };
    let mut bytes = Zeroizing::new(Vec::new());
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 1024 * 1024,
        "Docker configuration exceeds 1 MiB"
    );
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Config {
        #[serde(default)]
        auths: BTreeMap<String, Auth>,
        #[serde(default)]
        cred_helpers: BTreeMap<String, String>,
        creds_store: Option<String>,
    }
    #[derive(Deserialize)]
    struct Auth {
        auth: Option<String>,
        identitytoken: Option<String>,
    }
    impl Drop for Auth {
        fn drop(&mut self) {
            self.auth.zeroize();
            self.identitytoken.zeroize();
        }
    }
    let config: Config = serde_json::from_slice(&bytes).map_err(|_| {
        anyhow::anyhow!("invalid Docker configuration for snapshot registry authentication")
    })?;
    let docker_server = if server == "registry-1.docker.io" {
        "https://index.docker.io/v1/"
    } else {
        server
    };
    let helper = config
        .cred_helpers
        .get(docker_server)
        .or_else(|| config.cred_helpers.get(server))
        .or(config.creds_store.as_ref())
        .filter(|helper| !helper.is_empty());
    if let Some(helper) = helper {
        ensure!(
            helper.len() <= 128
                && helper
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "invalid Docker credential helper name"
        );
        let executable = which::which(format!("docker-credential-{helper}"))
            .context("Docker credential helper is unavailable")?;
        return helper_credentials(&executable, docker_server, cancellation);
    }
    let auth = config
        .auths
        .get(docker_server)
        .or_else(|| config.auths.get(server))
        .or_else(|| config.auths.get(&format!("https://{server}")))
        .or_else(|| config.auths.get(&format!("http://{server}")));
    let Some(auth) = auth else {
        return Ok(None);
    };
    ensure!(
        auth.auth.as_ref().is_none_or(String::is_empty)
            && auth.identitytoken.as_ref().is_none_or(String::is_empty),
        "inline Docker credentials are not supported; configure credHelpers or credsStore and run docker login"
    );
    Ok(None)
}

fn helper_credentials(
    executable: &Path,
    server: &str,
    cancellation: &Cancellation,
) -> Result<Option<Credentials>> {
    let mut input = tempfile::tempfile()?;
    writeln!(input, "{server}")?;
    input.rewind()?;
    let mut command = std::process::Command::new(executable);
    command.arg("get").stdin(input);
    let output = crate::tools::execute(
        &mut command,
        crate::tools::Output::Capture,
        PAYLOAD_LIMIT,
        Some(Instant::now() + Duration::from_secs(15)),
        cancellation,
    )
    .map_err(|_| {
        anyhow::anyhow!("Docker credential helper failed or timed out; check docker login")
    })?;
    let stdout = Zeroizing::new(output.stdout);
    let _stderr = Zeroizing::new(output.stderr);
    if output.exit == 1
        && !output.truncated
        && stdout.trim() == "credentials not found in native keychain"
    {
        return Ok(None);
    }
    ensure!(
        output.exit == 0 && !output.truncated,
        "Docker credential helper failed; run docker login for the snapshot registry"
    );
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Helper {
        username: String,
        secret: String,
    }
    let secret: Helper = serde_json::from_str(&stdout).map_err(|_| {
        anyhow::anyhow!("invalid Docker credential helper response: expected Username and Secret")
    })?;
    Ok(Some(Credentials {
        username: Zeroizing::new(secret.username),
        secret: Zeroizing::new(secret.secret),
    }))
}

#[cfg(test)]
mod tests;
