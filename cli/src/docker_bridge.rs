// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileTypeExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use hyperlight_unikraft::{
    Destination, DirEntry, FileStat, Filter, FsBackend, Mount, NetworkFilter, NetworkPolicy,
    SandboxBuilder,
};
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig, ServerConnection};
use zeroize::Zeroize;

const BUFFER: usize = 64 * 1024;
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: usize = 64;
const MAX_HANDSHAKES: usize = 16;
const CONNECTION_IDLE_TIMEOUT: Duration = Duration::from_secs(300);

struct HandshakeSlot {
    count: Arc<AtomicUsize>,
    active: bool,
}

impl HandshakeSlot {
    fn reserve(count: &Arc<AtomicUsize>) -> Option<Self> {
        let mut current = count.load(Ordering::Acquire);
        loop {
            if current >= MAX_HANDSHAKES {
                return None;
            }
            match count.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    return Some(Self {
                        count: count.clone(),
                        active: true,
                    });
                }
                Err(updated) => current = updated,
            }
        }
    }

    fn release(&mut self) {
        if self.active {
            self.count.fetch_sub(1, Ordering::AcqRel);
            self.active = false;
        }
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        self.release();
    }
}

pub struct DockerBridge {
    endpoint: SocketAddr,
    credentials: Arc<DockerCredentials>,
    stopping: Arc<AtomicBool>,
    worker: Option<JoinHandle<Result<()>>>,
}

impl DockerBridge {
    pub fn start(socket: &Path) -> Result<Self> {
        ensure!(
            std::fs::metadata(socket)
                .context("inspect Docker daemon socket")?
                .file_type()
                .is_socket(),
            "Docker endpoint must be a Unix socket"
        );
        let address = bridge_address()?;
        let listener = TcpListener::bind((address, 0))?;
        listener.set_nonblocking(true)?;
        let endpoint = listener.local_addr()?;
        let (configuration, credentials) = certificates_for(address)?;
        let configuration = Arc::new(configuration);
        let stopping = Arc::new(AtomicBool::new(false));
        let stop = stopping.clone();
        let socket = socket.to_owned();
        let worker = std::thread::spawn(move || serve(listener, &socket, configuration, stop));
        Ok(Self {
            endpoint,
            credentials,
            stopping,
            worker: Some(worker),
        })
    }

    pub fn from_environment() -> Result<Self> {
        let socket =
            match std::env::var("DOCKER_HOST") {
                Ok(value) => PathBuf::from(value.strip_prefix("unix://").context(
                    "Docker guest access requires DOCKER_HOST=unix:///path/to/docker.sock",
                )?),
                Err(std::env::VarError::NotPresent) => PathBuf::from("/var/run/docker.sock"),
                Err(error) => return Err(error).context("read DOCKER_HOST"),
            };
        Self::start(&socket)
    }

    pub fn configure(&self, builder: SandboxBuilder) -> SandboxBuilder {
        builder
            .mount(Mount::backend(self.credentials.clone(), "/docker", true))
            .env("DOCKER_HOST", format!("tcp://{}", self.endpoint))
            .env("DOCKER_TLS_VERIFY", "1")
            .env("DOCKER_CERT_PATH", "/docker")
            .network(self.network_policy())
    }

    pub fn network_policy(&self) -> NetworkPolicy {
        NetworkPolicy::Filter(Filter::new(DockerOnly(self.endpoint)))
    }
}

impl Drop for DockerBridge {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("Docker bridge stopped with an error: {error:#}"),
                Err(_) => eprintln!("Docker bridge worker panicked"),
            }
        }
    }
}

struct DockerOnly(SocketAddr);

impl NetworkFilter for DockerOnly {
    fn allows_name(&self, _name: &str) -> bool {
        false
    }

    fn allows(&self, destination: &Destination<'_>) -> bool {
        !destination.udp && destination.addr == self.0
    }
}

struct DockerCredentials {
    files: BTreeMap<String, Vec<u8>>,
}

impl DockerCredentials {
    fn new(files: impl IntoIterator<Item = (&'static str, String)>) -> Arc<Self> {
        Arc::new(Self {
            files: files
                .into_iter()
                .map(|(name, content)| (name.to_owned(), content.into_bytes()))
                .collect(),
        })
    }

    fn file(&self, path: &str) -> io::Result<&[u8]> {
        self.files
            .get(path)
            .map(Vec::as_slice)
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }
}

impl Drop for DockerCredentials {
    fn drop(&mut self) {
        for contents in self.files.values_mut() {
            contents.zeroize();
        }
    }
}

impl FsBackend for DockerCredentials {
    fn stat(&self, path: &str) -> io::Result<FileStat> {
        if path.is_empty() {
            return Ok(FileStat::dir(0o500));
        }
        Ok(FileStat::file(self.file(path)?.len() as u64, 0o400))
    }

    fn read(&self, path: &str, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        let contents = self.file(path)?;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(contents.len());
        let count = buffer.len().min(contents.len() - start);
        buffer[..count].copy_from_slice(&contents[start..start + count]);
        Ok(count)
    }

    fn write(&self, _path: &str, _offset: Option<u64>, _data: &[u8]) -> io::Result<()> {
        Err(io::ErrorKind::PermissionDenied.into())
    }

    fn create_dir(&self, _path: &str) -> io::Result<()> {
        Err(io::ErrorKind::PermissionDenied.into())
    }

    fn remove(&self, _path: &str) -> io::Result<()> {
        Err(io::ErrorKind::PermissionDenied.into())
    }

    fn truncate(&self, _path: &str, _len: u64) -> io::Result<()> {
        Err(io::ErrorKind::PermissionDenied.into())
    }

    fn list(&self, path: &str) -> io::Result<Vec<DirEntry>> {
        if !path.is_empty() {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(self
            .files
            .keys()
            .map(|name| DirEntry::new(name, false))
            .collect())
    }

    fn rename(&self, _from: &str, _to: &str) -> io::Result<()> {
        Err(io::ErrorKind::PermissionDenied.into())
    }
}

fn certificates_for(address: IpAddr) -> Result<(ServerConfig, Arc<DockerCredentials>)> {
    let mut authority = CertificateParams::new(Vec::<String>::new())?;
    authority.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let authority_key = KeyPair::generate()?;
    let authority_certificate = authority.self_signed(&authority_key)?;
    let issuer = Issuer::new(authority, authority_key);

    let mut server = CertificateParams::new(vec![address.to_string()])?;
    server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let server_key = KeyPair::generate()?;
    let server = server.signed_by(&server_key, &issuer)?;
    let mut client = CertificateParams::new(Vec::<String>::new())?;
    client.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let client_key = KeyPair::generate()?;
    let client = client.signed_by(&client_key, &issuer)?;
    let credentials = DockerCredentials::new([
        ("ca.pem", authority_certificate.pem()),
        ("cert.pem", client.pem()),
        ("key.pem", client_key.serialize_pem()),
    ]);

    let mut roots = RootCertStore::empty();
    roots.add(authority_certificate.der().clone())?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let verifier =
        WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone()).build()?;
    let configuration = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()?
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![server.der().clone(), authority_certificate.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(server_key.serialize_der()).into(),
        )?;
    Ok((configuration, credentials))
}

fn serve(
    listener: TcpListener,
    socket: &Path,
    configuration: Arc<ServerConfig>,
    stopping: Arc<AtomicBool>,
) -> Result<()> {
    let local_address = listener.local_addr()?.ip();
    let mut clients: Vec<JoinHandle<()>> = Vec::new();
    let handshakes = Arc::new(AtomicUsize::new(0));
    while !stopping.load(Ordering::Acquire) {
        let mut pending = Vec::new();
        for client in clients.drain(..) {
            if client.is_finished() {
                client
                    .join()
                    .map_err(|_| anyhow::anyhow!("Docker bridge connection worker panicked"))?;
            } else {
                pending.push(client);
            }
        }
        clients = pending;
        match listener.accept() {
            Ok((stream, peer)) if peer.ip() != local_address => {
                drop(stream);
            }
            Ok((stream, address)) if clients.len() < MAX_CONNECTIONS => {
                let Some(handshake) = HandshakeSlot::reserve(&handshakes) else {
                    drop(stream);
                    eprintln!("Docker bridge refused excess unauthenticated connections");
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                let socket = socket.to_owned();
                let configuration = configuration.clone();
                let stopping = stopping.clone();
                clients.push(std::thread::spawn(move || {
                    if let Err(error) = forward(
                        stream,
                        address,
                        &socket,
                        configuration,
                        &stopping,
                        handshake,
                        TLS_HANDSHAKE_TIMEOUT,
                    ) {
                        eprintln!("Docker bridge connection failed: {error:#}");
                    }
                }));
            }
            Ok(_) => {
                eprintln!("Docker bridge refused excess concurrent connections");
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error).context("accept Docker bridge connection"),
        }
    }
    for client in clients {
        client
            .join()
            .map_err(|_| anyhow::anyhow!("Docker bridge connection worker panicked"))?;
    }
    Ok(())
}

fn forward(
    peer: TcpStream,
    address: SocketAddr,
    socket: &Path,
    configuration: Arc<ServerConfig>,
    stopping: &AtomicBool,
    handshake: HandshakeSlot,
    handshake_timeout: Duration,
) -> Result<()> {
    let mut phase = "first-byte";
    forward_connection(
        peer,
        socket,
        configuration,
        stopping,
        handshake,
        handshake_timeout,
        &mut phase,
    )
    .with_context(|| format!("peer={address} phase={phase}"))
}

fn forward_connection(
    mut peer: TcpStream,
    socket: &Path,
    configuration: Arc<ServerConfig>,
    stopping: &AtomicBool,
    mut handshake: HandshakeSlot,
    handshake_timeout: Duration,
    phase: &mut &'static str,
) -> Result<()> {
    peer.set_nonblocking(true)?;
    peer.set_nodelay(true)?;
    let mut tls = ServerConnection::new(configuration)?;
    tls.set_buffer_limit(Some(BUFFER));
    let handshake_started = Instant::now();
    let first_byte_deadline = handshake_started + handshake_timeout;
    let mut handshake_deadline = None;
    let mut tls_read_bytes = 0;
    let mut tls_written_bytes = 0;
    let mut last_tls_read = handshake_started;
    let mut last_tls_write = handshake_started;
    let mut daemon = None;
    let mut incoming = VecDeque::new();
    let mut outgoing = VecDeque::new();
    let mut buffer = [0; 16 * 1024];
    let mut daemon_closed = false;
    let mut transport_closed = false;
    let mut peer_closed = false;
    let mut write_closed = false;
    let mut notified = false;
    let mut last_activity = Instant::now();
    while !stopping.load(Ordering::Acquire) {
        let mut progress = false;
        if !transport_closed && incoming.len() < BUFFER && tls.wants_read() {
            match tls.read_tls(&mut peer) {
                Ok(0) => {
                    ensure!(
                        !tls.is_handshaking(),
                        "Docker bridge TCP EOF before TLS authentication completed"
                    );
                    transport_closed = true;
                    progress = true;
                }
                Ok(count) => {
                    if tls.is_handshaking() {
                        tls_read_bytes += count;
                        last_tls_read = Instant::now();
                    }
                    if count > 0 && handshake_deadline.is_none() {
                        handshake_deadline = Some(Instant::now() + handshake_timeout);
                        *phase = "tls-handshake";
                    }
                    // Only plaintext EOF closes daemon input; close_notify may precede buffered data.
                    tls.process_new_packets()
                        .context("process Docker bridge TLS packets")?;
                    progress = true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error).context("read Docker TLS transport"),
            }
        }
        if !tls.is_handshaking() {
            handshake.release();
            let daemon = match &mut daemon {
                Some(daemon) => daemon,
                target @ None => {
                    *phase = "daemon-connect";
                    let stream = connect_daemon(socket, stopping, Duration::from_secs(10))?;
                    *phase = "authenticated-forwarding";
                    target.insert(stream)
                }
            };
            if incoming.len() < BUFFER && !write_closed {
                let available = buffer.len().min(BUFFER - incoming.len());
                match tls.reader().read(&mut buffer[..available]) {
                    Ok(0) => peer_closed = true,
                    Ok(count) => {
                        incoming.extend(&buffer[..count]);
                        progress = true;
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock && transport_closed =>
                    {
                        peer_closed = true;
                    }
                    Err(error)
                        if error.kind() == std::io::ErrorKind::UnexpectedEof
                            && transport_closed =>
                    {
                        peer_closed = true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error).context("read Docker client request"),
                }
            }
            progress |= drain(daemon, &mut incoming).context("write Docker daemon request")?;
            if peer_closed && incoming.is_empty() && !write_closed {
                daemon
                    .shutdown(std::net::Shutdown::Write)
                    .context("close Docker daemon request")?;
                write_closed = true;
            }
            if outgoing.len() < BUFFER && !daemon_closed {
                let available = buffer.len().min(BUFFER - outgoing.len());
                match daemon.read(&mut buffer[..available]) {
                    Ok(0) => daemon_closed = true,
                    Ok(count) => {
                        outgoing.extend(&buffer[..count]);
                        progress = true;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error).context("read Docker daemon response"),
                }
            }
            // Rustls reports a full plaintext buffer as a zero-length write.
            let count = tls
                .writer()
                .write(outgoing.as_slices().0)
                .context("buffer Docker daemon response")?;
            outgoing.drain(..count);
            progress |= count > 0;
            if daemon_closed && outgoing.is_empty() && !notified {
                tls.send_close_notify();
                notified = true;
            }
        }
        if tls.wants_write() {
            match tls.write_tls(&mut peer) {
                Ok(count) => {
                    if count > 0 && tls.is_handshaking() {
                        tls_written_bytes += count;
                        last_tls_write = Instant::now();
                    }
                    progress |= count > 0;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(error) => return Err(error).context("write Docker TLS transport"),
            }
        }
        if tls.is_handshaking() {
            let now = Instant::now();
            if now >= handshake_deadline.unwrap_or(first_byte_deadline) {
                let reason = if handshake_deadline.is_some() {
                    "Docker bridge TLS handshake timed out"
                } else {
                    "Docker bridge timed out waiting for first TLS byte"
                };
                bail!(
                    "{reason}: tls_read_bytes={tls_read_bytes} tls_written_bytes={tls_written_bytes} \
                     timeout_ms={} elapsed_ms={} last_tls_read_ms_ago={} last_tls_write_ms_ago={} \
                     protocol={:?} client_certificates={} wants_read={} wants_write={}",
                    handshake_timeout.as_millis(),
                    now.duration_since(handshake_started).as_millis(),
                    now.duration_since(last_tls_read).as_millis(),
                    now.duration_since(last_tls_write).as_millis(),
                    tls.protocol_version(),
                    tls.peer_certificates()
                        .map_or(0, |certificates| certificates.len()),
                    tls.wants_read(),
                    tls.wants_write(),
                );
            }
        }
        if notified && !tls.wants_write() {
            return Ok(());
        }
        if progress {
            last_activity = Instant::now();
        } else {
            if !tls.is_handshaking() {
                ensure!(
                    last_activity.elapsed() < CONNECTION_IDLE_TIMEOUT,
                    "Docker bridge authenticated connection timed out while idle"
                );
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    Ok(())
}

fn connect_daemon(socket: &Path, stopping: &AtomicBool, timeout: Duration) -> Result<UnixStream> {
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    address.sun_family = libc::AF_UNIX as _;
    let path = socket.as_os_str().as_bytes();
    ensure!(
        !path.contains(&0) && path.len() < address.sun_path.len(),
        "invalid Docker daemon socket path"
    );
    for (target, byte) in address.sun_path.iter_mut().zip(path) {
        *target = *byte as _;
    }
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    ensure!(
        fd >= 0,
        "create Docker socket: {}",
        std::io::Error::last_os_error()
    );
    // Own the descriptor before any fallible operation, and never enter a blocking connect.
    let stream = unsafe { UnixStream::from_raw_fd(fd) };
    stream.set_nonblocking(true)?;
    ensure!(
        unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } >= 0,
        "set Docker socket close-on-exec: {}",
        std::io::Error::last_os_error()
    );
    let deadline = Instant::now() + timeout;
    loop {
        ensure!(
            !stopping.load(Ordering::Acquire),
            "Docker daemon connection cancelled"
        );
        ensure!(
            Instant::now() < deadline,
            "Docker daemon connection timed out"
        );
        let connected = unsafe {
            libc::connect(
                stream.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                std::mem::size_of_val(&address) as _,
            )
        };
        if connected == 0 {
            return Ok(stream);
        }
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EINPROGRESS | libc::EALREADY) => loop {
                ensure!(
                    !stopping.load(Ordering::Acquire),
                    "Docker daemon connection cancelled"
                );
                ensure!(
                    Instant::now() < deadline,
                    "Docker daemon connection timed out"
                );
                let mut descriptor = libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut descriptor, 1, 10) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(error).context("poll Docker daemon connection");
                }
                if ready > 0 {
                    if let Some(error) = stream.take_error()? {
                        return Err(error).context("connect Docker daemon");
                    }
                    return Ok(stream);
                }
            },
            // Unix sockets report a full accept backlog as EAGAIN, not EINPROGRESS.
            Some(libc::EAGAIN | libc::EINTR) => {
                std::thread::sleep(Duration::from_millis(5));
            }
            _ => return Err(error).context("connect Docker daemon"),
        }
    }
}

fn select_bridge_address(preferred: Option<IpAddr>, addresses: &[IpAddr]) -> Result<IpAddr> {
    let usable = |address: &IpAddr| {
        address.is_ipv4()
            && !address.is_loopback()
            && !address.is_unspecified()
            && !address.is_multicast()
    };
    preferred
        .filter(|address| usable(address) && addresses.contains(address))
        .or_else(|| addresses.iter().copied().find(usable))
        .context("Docker bridge requires an active non-loopback IPv4 interface")
}

fn bridge_address() -> Result<IpAddr> {
    let mut interfaces = std::ptr::null_mut();
    ensure!(
        unsafe { libc::getifaddrs(&mut interfaces) } == 0,
        "enumerate Docker bridge interfaces: {}",
        std::io::Error::last_os_error()
    );
    struct Interfaces(*mut libc::ifaddrs);
    impl Drop for Interfaces {
        fn drop(&mut self) {
            unsafe { libc::freeifaddrs(self.0) };
        }
    }
    let interfaces = Interfaces(interfaces);
    let mut entry = interfaces.0;
    let mut addresses = Vec::new();
    while !entry.is_null() {
        let interface = unsafe { &*entry };
        if interface.ifa_flags & libc::IFF_UP as u32 != 0
            && !interface.ifa_addr.is_null()
            && unsafe { (*interface.ifa_addr).sa_family } as i32 == libc::AF_INET
        {
            let address = unsafe { &*interface.ifa_addr.cast::<libc::sockaddr_in>() };
            addresses.push(IpAddr::V4(address.sin_addr.s_addr.to_ne_bytes().into()));
        }
        entry = interface.ifa_next;
    }
    // This route lookup sends no packet and is only a preference, never a prerequisite.
    let preferred = UdpSocket::bind("0.0.0.0:0").ok().and_then(|route| {
        route.connect("192.0.2.1:9").ok()?;
        Some(route.local_addr().ok()?.ip())
    });
    select_bridge_address(preferred, &addresses)
}

fn drain(writer: &mut impl Write, pending: &mut VecDeque<u8>) -> Result<bool> {
    if pending.is_empty() {
        return Ok(false);
    }
    match writer.write(pending.as_slices().0) {
        Ok(0) => bail!("Docker bridge stream stopped accepting data"),
        Ok(count) => {
            pending.drain(..count);
            Ok(true)
        }
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => Ok(false),
        Err(error) => Err(error).context("forward Docker API bytes"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, pem::PemObject};
    use rustls::{ClientConfig, ClientConnection, StreamOwned};
    use std::os::unix::net::UnixListener;

    fn client(
        bridge: &DockerBridge,
        authenticated: bool,
    ) -> Result<StreamOwned<ClientConnection, TcpStream>> {
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from_pem_slice(
            bridge.credentials.file("ca.pem")?,
        )?)?;
        let builder =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots);
        let configuration = if authenticated {
            builder.with_client_auth_cert(
                vec![CertificateDer::from_pem_slice(
                    bridge.credentials.file("cert.pem")?,
                )?],
                PrivateKeyDer::from_pem_slice(bridge.credentials.file("key.pem")?)?,
            )?
        } else {
            builder.with_no_client_auth()
        };
        let stream = TcpStream::connect(bridge.endpoint)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        stream.set_write_timeout(Some(Duration::from_secs(5)))?;
        Ok(StreamOwned::new(
            ClientConnection::new(
                Arc::new(configuration),
                ServerName::IpAddress(bridge.endpoint.ip().into()),
            )?,
            stream,
        ))
    }

    #[test]
    fn unauthenticated_clients_cannot_reach_the_docker_socket() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("docker.sock");
        let daemon = UnixListener::bind(&socket)?;
        daemon.set_nonblocking(true)?;
        let bridge = DockerBridge::start(&socket)?;
        let mut peer = client(&bridge, false)?;
        let result = peer
            .write_all(b"not authorized")
            .and_then(|_| peer.read_exact(&mut [0]));
        ensure!(
            result.is_err(),
            "bridge accepted a client without its per-operation certificate"
        );
        ensure!(
            matches!(daemon.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock),
            "unauthenticated client reached the daemon"
        );
        Ok(())
    }

    #[test]
    fn authenticated_streams_handle_backpressure_and_drop_removes_access() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("docker.sock");
        let daemon = UnixListener::bind(&socket)?;
        let payload = vec![b'x'; BUFFER * 64 + 19];
        let count = payload.len();
        let echo = std::thread::spawn(move || -> std::io::Result<()> {
            let (mut connection, _) = daemon.accept()?;
            connection.set_read_timeout(Some(Duration::from_secs(5)))?;
            connection.set_write_timeout(Some(Duration::from_secs(5)))?;
            let mut bytes = vec![0; count];
            connection.read_exact(&mut bytes)?;
            bytes[0] = b'y';
            connection.write_all(&bytes)
        });
        let bridge = DockerBridge::start(&socket)?;
        let endpoint = bridge.endpoint;
        let credentials = Arc::downgrade(&bridge.credentials);
        let mut peer = client(&bridge, true)?;
        peer.write_all(&payload)?;
        std::thread::sleep(Duration::from_millis(100));
        let mut received = vec![0; count];
        peer.read_exact(&mut received)?;
        ensure!(
            received[0] == b'y' && received[1..] == payload[1..],
            "Docker stream was truncated"
        );
        echo.join().unwrap()?;
        drop(bridge);
        ensure!(
            credentials.upgrade().is_none(),
            "Docker credentials survived bridge shutdown"
        );
        ensure!(
            TcpStream::connect_timeout(&endpoint, Duration::from_secs(1)).is_err(),
            "Docker listener survived its provider operation"
        );
        Ok(())
    }

    #[test]
    fn docker_credentials_are_read_only_and_exist_only_in_memory() -> Result<()> {
        let (_, credentials) = certificates_for("192.0.2.1".parse()?)?;
        assert_eq!(
            credentials.list("")?,
            vec![
                DirEntry::new("ca.pem", false),
                DirEntry::new("cert.pem", false),
                DirEntry::new("key.pem", false),
            ]
        );
        assert_eq!(credentials.stat("key.pem")?.mode & 0o777, 0o400);
        let mut bytes = vec![0; credentials.stat("key.pem")?.size as usize];
        let count = credentials.read("key.pem", 0, &mut bytes)?;
        assert_eq!(count, bytes.len());
        assert!(bytes.starts_with(b"-----BEGIN PRIVATE KEY-----"));
        assert_eq!(
            credentials
                .write("key.pem", Some(0), b"replacement")
                .unwrap_err()
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        Ok(())
    }

    #[test]
    fn docker_network_grant_is_exact_and_tcp_only() {
        let endpoint = "192.0.2.2:32123".parse().unwrap();
        let policy = DockerOnly(endpoint);
        assert!(policy.allows(&Destination::new(endpoint, false, &[])));
        assert!(!policy.allows(&Destination::new(endpoint, true, &[])));
        assert!(!policy.allows(&Destination::new(
            "192.0.2.2:22".parse().unwrap(),
            false,
            &[]
        )));
        assert!(!policy.allows(&Destination::new(
            "192.0.2.3:32123".parse().unwrap(),
            false,
            &[]
        )));
        assert!(!policy.allows_name("example.com"));
    }

    #[test]
    fn authenticated_half_close_drains_plaintext_before_closing_daemon_input() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("docker.sock");
        let daemon = UnixListener::bind(&socket)?;
        let payload = vec![b'x'; 16 * 1024 + 139];
        let receiver = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            let (mut connection, _) = daemon.accept()?;
            connection.set_read_timeout(Some(Duration::from_secs(5)))?;
            connection.set_write_timeout(Some(Duration::from_secs(5)))?;
            let mut received = Vec::new();
            connection.read_to_end(&mut received)?;
            connection.write_all(b"done")?;
            Ok(received)
        });
        let bridge = DockerBridge::start(&socket)?;
        let mut peer = client(&bridge, true)?;
        peer.conn.complete_io(&mut peer.sock)?;
        peer.conn.set_buffer_limit(None);
        peer.conn.writer().write_all(&payload)?;
        peer.conn.send_close_notify();
        let mut encrypted = Vec::new();
        while peer.conn.wants_write() {
            peer.conn.write_tls(&mut encrypted)?;
        }
        peer.sock.write_all(&encrypted)?;
        let mut response = [0; 4];
        peer.read_exact(&mut response)?;
        assert_eq!(&response, b"done");
        let received = receiver.join().expect("daemon receiver panicked")?;
        assert_eq!(
            received.len(),
            payload.len(),
            "TLS half-close truncated the Docker upload"
        );
        assert_eq!(received, payload);
        Ok(())
    }

    #[test]
    fn tcp_half_close_still_returns_the_daemon_response() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("docker.sock");
        let daemon = UnixListener::bind(&socket)?;
        let receiver = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
            let (mut connection, _) = daemon.accept()?;
            connection.set_read_timeout(Some(Duration::from_secs(5)))?;
            connection.set_write_timeout(Some(Duration::from_secs(5)))?;
            let mut received = Vec::new();
            connection.read_to_end(&mut received)?;
            connection.write_all(b"done")?;
            Ok(received)
        });
        let bridge = DockerBridge::start(&socket)?;
        let mut peer = client(&bridge, true)?;
        peer.conn.complete_io(&mut peer.sock)?;
        peer.conn.writer().write_all(b"request")?;
        while peer.conn.wants_write() {
            peer.conn.write_tls(&mut peer.sock)?;
        }
        peer.sock.shutdown(std::net::Shutdown::Write)?;
        let mut response = [0; 4];
        peer.read_exact(&mut response)?;
        assert_eq!(&response, b"done");
        assert_eq!(
            receiver.join().expect("daemon receiver panicked")?,
            b"request"
        );
        Ok(())
    }

    #[test]
    fn bridge_refuses_regular_files_in_place_of_daemon_sockets() {
        let file = tempfile::NamedTempFile::new().unwrap();
        assert!(
            DockerBridge::start(file.path())
                .err()
                .unwrap()
                .to_string()
                .contains("must be a Unix socket")
        );
    }

    #[test]
    fn handshake_slots_are_bounded_and_released() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut slots: Vec<_> = (0..MAX_HANDSHAKES)
            .map(|_| HandshakeSlot::reserve(&count).unwrap())
            .collect();
        assert!(HandshakeSlot::reserve(&count).is_none());
        slots.pop().unwrap().release();
        assert!(HandshakeSlot::reserve(&count).is_some());
    }

    #[test]
    fn first_byte_timeout_is_bounded_and_configurable() -> Result<()> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let peer = TcpStream::connect(listener.local_addr()?)?;
        let (server, address) = listener.accept()?;
        let (configuration, _) = certificates_for(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))?;
        let count = Arc::new(AtomicUsize::new(0));
        let handshake = HandshakeSlot::reserve(&count).unwrap();
        let stopping = AtomicBool::new(false);
        let started = Instant::now();
        let error = forward(
            server,
            address,
            Path::new("/unused"),
            Arc::new(configuration),
            &stopping,
            handshake,
            Duration::from_millis(25),
        )
        .unwrap_err();
        drop(peer);
        assert!(
            format!("{error:#}").contains("timed out waiting for first TLS byte"),
            "{error:#}"
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        Ok(())
    }

    fn controlled_handshake_error(
        input: &[u8],
        close_input: bool,
    ) -> Result<(String, std::net::SocketAddr)> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let mut peer = TcpStream::connect(listener.local_addr()?)?;
        let (server, address) = listener.accept()?;
        peer.write_all(input)?;
        if close_input {
            peer.shutdown(std::net::Shutdown::Write)?;
        }
        let (configuration, _) = certificates_for(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))?;
        let count = Arc::new(AtomicUsize::new(0));
        let handshake = HandshakeSlot::reserve(&count).unwrap();
        let error = forward(
            server,
            address,
            Path::new("unused-docker-bridge-diagnostic.sock"),
            Arc::new(configuration),
            &AtomicBool::new(false),
            handshake,
            Duration::from_millis(50),
        )
        .expect_err("an incomplete handshake must not succeed");
        assert_eq!(count.load(Ordering::Acquire), 0);
        drop(peer);
        Ok((format!("{error:#}"), address))
    }

    #[test]
    fn diagnostics_distinguish_eof_before_tls_from_timeout() -> Result<()> {
        let (diagnostic, peer) = controlled_handshake_error(&[], true)?;
        assert!(
            diagnostic.to_lowercase().contains("eof") && !diagnostic.contains("timed out"),
            "a closed transport is not a timeout: {diagnostic}"
        );
        assert!(diagnostic.contains("phase=first-byte"), "{diagnostic}");
        assert!(diagnostic.contains(&format!("peer={peer}")), "{diagnostic}");
        Ok(())
    }

    #[test]
    fn diagnostics_preserve_accepted_peer_after_reset_before_forwarding() -> Result<()> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let peer = TcpStream::connect(listener.local_addr()?)?;
        let (server, address) = listener.accept()?;
        let linger = libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        ensure!(
            unsafe {
                libc::setsockopt(
                    peer.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_LINGER,
                    (&linger as *const libc::linger).cast(),
                    std::mem::size_of_val(&linger) as _,
                )
            } == 0,
            "set reset-on-close: {}",
            io::Error::last_os_error()
        );
        drop(peer);
        let mut descriptor = libc::pollfd {
            fd: server.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        ensure!(
            unsafe { libc::poll(&mut descriptor, 1, 1000) } > 0
                && descriptor.revents & (libc::POLLIN | libc::POLLERR | libc::POLLHUP) != 0,
            "reset did not arrive before forwarding"
        );
        let (configuration, _) = certificates_for(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))?;
        let count = Arc::new(AtomicUsize::new(0));
        let handshake = HandshakeSlot::reserve(&count).unwrap();
        let error = forward(
            server,
            address,
            Path::new("unused-docker-bridge-diagnostic.sock"),
            Arc::new(configuration),
            &AtomicBool::new(false),
            handshake,
            Duration::from_secs(1),
        )
        .expect_err("reset transport must not authenticate");
        let diagnostic = format!("{error:#}");
        assert_eq!(count.load(Ordering::Acquire), 0);
        assert!(
            diagnostic.contains(&format!("peer={address}")),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("phase=first-byte"), "{diagnostic}");
        assert!(!diagnostic.contains("timed out"), "{diagnostic}");
        Ok(())
    }

    #[test]
    fn diagnostics_distinguish_eof_during_tls_from_timeout() -> Result<()> {
        // An incomplete TLS record header starts the handshake without a server response.
        let (diagnostic, peer) = controlled_handshake_error(&[0x16, 0x03], true)?;
        assert!(
            diagnostic.to_lowercase().contains("eof") && !diagnostic.contains("timed out"),
            "a truncated handshake is not a timeout: {diagnostic}"
        );
        assert!(diagnostic.contains("phase=tls-handshake"), "{diagnostic}");
        assert!(diagnostic.contains(&format!("peer={peer}")), "{diagnostic}");
        Ok(())
    }

    #[test]
    fn diagnostics_locate_first_byte_timeout_and_peer() -> Result<()> {
        let (diagnostic, peer) = controlled_handshake_error(&[], false)?;
        assert!(diagnostic.contains("timed out"), "{diagnostic}");
        assert!(diagnostic.contains("phase=first-byte"), "{diagnostic}");
        assert!(diagnostic.contains(&format!("peer={peer}")), "{diagnostic}");
        Ok(())
    }

    #[test]
    fn diagnostics_locate_partial_tls_timeout_and_peer() -> Result<()> {
        let (diagnostic, peer) = controlled_handshake_error(&[0x16, 0x03], false)?;
        assert!(diagnostic.contains("timed out"), "{diagnostic}");
        assert!(diagnostic.contains("phase=tls-handshake"), "{diagnostic}");
        assert!(diagnostic.contains(&format!("peer={peer}")), "{diagnostic}");
        Ok(())
    }

    #[test]
    fn diagnostics_identify_client_stalled_after_server_handshake_flight() -> Result<()> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let (configuration, credentials) =
            certificates_for(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))?;
        let bridge = DockerBridge {
            endpoint: listener.local_addr()?,
            credentials,
            stopping: Arc::new(AtomicBool::new(false)),
            worker: None,
        };
        let mut peer = client(&bridge, true)?;
        let (server, address) = listener.accept()?;
        let count = Arc::new(AtomicUsize::new(0));
        let handshake = HandshakeSlot::reserve(&count).unwrap();
        let worker = std::thread::spawn(move || {
            forward(
                server,
                address,
                Path::new("unused-docker-bridge-diagnostic.sock"),
                Arc::new(configuration),
                &AtomicBool::new(false),
                handshake,
                Duration::from_millis(100),
            )
        });
        let mut sent = 0;
        while peer.conn.wants_write() {
            sent += peer.conn.write_tls(&mut peer.sock)?;
        }
        assert!(peer.conn.read_tls(&mut peer.sock)? > 0);
        let result = worker.join().expect("controlled handshake worker panicked");
        let diagnostic = format!(
            "{:#}",
            result.expect_err("client did not finish authentication")
        );
        assert_eq!(count.load(Ordering::Acquire), 0);
        assert!(diagnostic.contains("timed out"), "{diagnostic}");
        assert!(
            diagnostic.contains(&format!("peer={address}")),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("phase=tls-handshake"), "{diagnostic}");
        assert!(
            diagnostic.contains(&format!("tls_read_bytes={sent} ")),
            "{diagnostic}"
        );
        let written = diagnostic
            .split_whitespace()
            .find_map(|field| field.strip_prefix("tls_written_bytes="))
            .context("missing TLS write progress")?
            .parse::<usize>()?;
        assert!(written > 0, "{diagnostic}");
        assert!(
            diagnostic.contains("protocol=Some(TLSv1_3)"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("client_certificates=0"), "{diagnostic}");
        Ok(())
    }

    #[test]
    fn diagnostics_locate_daemon_connect_failure_after_authentication() -> Result<()> {
        let directory = tempfile::tempdir_in(".")?;
        let socket = Path::new(".")
            .join(directory.path().file_name().unwrap())
            .join("docker.sock");
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let (configuration, credentials) =
            certificates_for(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))?;
        let bridge = DockerBridge {
            endpoint: listener.local_addr()?,
            credentials,
            stopping: Arc::new(AtomicBool::new(false)),
            worker: None,
        };
        let mut peer = client(&bridge, true)?;
        let (server, address) = listener.accept()?;
        let count = Arc::new(AtomicUsize::new(0));
        let handshake = HandshakeSlot::reserve(&count).unwrap();
        let stopping = bridge.stopping.clone();
        let worker = std::thread::spawn(move || {
            forward(
                server,
                address,
                &socket,
                Arc::new(configuration),
                &stopping,
                handshake,
                Duration::from_secs(1),
            )
        });
        let probe = (|| -> Result<()> {
            peer.conn.complete_io(&mut peer.sock)?;
            let deadline = Instant::now() + Duration::from_secs(2);
            while !worker.is_finished() {
                ensure!(
                    Instant::now() < deadline,
                    "controlled forwarding did not stop"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        })();
        bridge.stopping.store(true, Ordering::Release);
        let result = worker.join().expect("controlled bridge worker panicked");
        probe?;
        assert_eq!(count.load(Ordering::Acquire), 0);
        let diagnostic = format!("{:#}", result.expect_err("expected transport failure"));
        assert!(diagnostic.contains("connect Docker daemon"), "{diagnostic}");
        assert!(diagnostic.contains("phase=daemon-connect"), "{diagnostic}");
        assert!(
            diagnostic.contains(&format!("peer={address}")),
            "{diagnostic}"
        );
        Ok(())
    }

    #[test]
    fn partial_handshake_input_does_not_extend_deadline() -> Result<()> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let mut peer = TcpStream::connect(listener.local_addr()?)?;
        let (server, address) = listener.accept()?;
        let (configuration, credentials) =
            certificates_for(IpAddr::V4(std::net::Ipv4Addr::LOCALHOST))?;
        let mut roots = RootCertStore::empty();
        roots.add(CertificateDer::from_pem_slice(credentials.file("ca.pem")?)?)?;
        let client_configuration =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .with_root_certificates(roots)
                .with_client_auth_cert(
                    vec![CertificateDer::from_pem_slice(
                        credentials.file("cert.pem")?,
                    )?],
                    PrivateKeyDer::from_pem_slice(credentials.file("key.pem")?)?,
                )?;
        let mut client = ClientConnection::new(
            Arc::new(client_configuration),
            ServerName::IpAddress(std::net::Ipv4Addr::LOCALHOST.into()),
        )?;
        let mut hello = Vec::new();
        client.write_tls(&mut hello)?;
        let sender = std::thread::spawn(move || {
            for byte in hello.into_iter().take(10) {
                if peer.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let count = Arc::new(AtomicUsize::new(0));
        let handshake = HandshakeSlot::reserve(&count).unwrap();
        let stopping = AtomicBool::new(false);
        let started = Instant::now();
        let error = forward(
            server,
            address,
            Path::new("/unused"),
            Arc::new(configuration),
            &stopping,
            handshake,
            Duration::from_millis(25),
        )
        .unwrap_err();
        sender.join().expect("handshake sender panicked");
        assert!(
            format!("{error:#}").contains("TLS handshake timed out"),
            "{error:#}"
        );
        assert!(started.elapsed() < Duration::from_millis(80));
        Ok(())
    }

    #[test]
    fn offline_interface_selection_needs_no_default_route() -> Result<()> {
        let address: IpAddr = "172.20.0.1".parse()?;
        assert_eq!(
            select_bridge_address(None, &["127.0.0.1".parse()?, address])?,
            address
        );
        assert_eq!(
            select_bridge_address(Some("192.0.2.1".parse()?), &[address])?,
            address
        );
        assert!(select_bridge_address(None, &["127.0.0.1".parse()?, "0.0.0.0".parse()?]).is_err());
        Ok(())
    }

    #[test]
    fn saturated_daemon_connect_is_bounded_and_bridge_drop_cancels_it() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("docker.sock");
        let daemon = UnixListener::bind(&socket)?;
        ensure!(
            unsafe { libc::listen(daemon.as_raw_fd(), 0) } == 0,
            "set daemon backlog"
        );
        let stopping = AtomicBool::new(false);
        let queued = connect_daemon(&socket, &stopping, Duration::from_secs(1))?;
        let started = Instant::now();
        let error = connect_daemon(&socket, &stopping, Duration::from_millis(50)).unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
        let bridge = DockerBridge::start(&socket)?;
        let mut peer = client(&bridge, true)?;
        peer.write_all(b"request")?;
        peer.flush()?;
        std::thread::sleep(Duration::from_millis(50));
        let started = Instant::now();
        drop(bridge);
        assert!(started.elapsed() < Duration::from_secs(1));
        drop(queued);
        Ok(())
    }

    #[test]
    fn foreign_source_connections_do_not_consume_worker_slots() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("docker.sock");
        let daemon = UnixListener::bind(&socket)?;
        let bridge = DockerBridge::start(&socket)?;
        let mut foreign = Vec::new();
        for _ in 0..MAX_CONNECTIONS {
            let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
            ensure!(fd >= 0, "create foreign-source socket");
            let stream = unsafe { TcpStream::from_raw_fd(fd) };
            let mut source: libc::sockaddr_in = unsafe { std::mem::zeroed() };
            source.sin_family = libc::AF_INET as _;
            source.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
            ensure!(
                unsafe {
                    libc::bind(
                        fd,
                        (&source as *const libc::sockaddr_in).cast(),
                        std::mem::size_of_val(&source) as _,
                    )
                } == 0,
                "bind foreign-source socket"
            );
            let mut destination = source;
            let IpAddr::V4(address) = bridge.endpoint.ip() else {
                unreachable!()
            };
            destination.sin_addr.s_addr = u32::from_ne_bytes(address.octets());
            destination.sin_port = bridge.endpoint.port().to_be();
            ensure!(
                unsafe {
                    libc::connect(
                        fd,
                        (&destination as *const libc::sockaddr_in).cast(),
                        std::mem::size_of_val(&destination) as _,
                    )
                } == 0,
                "connect foreign-source socket"
            );
            foreign.push(stream);
        }
        let mut peer = client(&bridge, true)?;
        peer.write_all(b"authorized")?;
        peer.flush()?;
        daemon.set_nonblocking(true)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match daemon.accept() {
                Ok(_) => break,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    ensure!(
                        Instant::now() < deadline,
                        "foreign clients exhausted bridge workers"
                    );
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
}
