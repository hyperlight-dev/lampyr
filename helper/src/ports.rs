// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! The TCP ports this helper's guests listen on.
//!
//! A guest's listening socket is a host socket at the address the app asks
//! for, and a resumed guest re-binds the port it had when it was parked. So
//! a port belongs to one fiber at a time, it comes from a range the kernel
//! never hands out itself (no guest's bind to port 0, and no outgoing
//! connection, can land on it), and before a fiber is reported serving,
//! every listener on its port must be this process's. With all three, the
//! listener a fiber's endpoint reaches is that fiber's guest, or the fiber
//! fails to start.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::hash::{BuildHasher, RandomState};
use std::io::{BufRead, BufReader, ErrorKind};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
use std::ops::RangeInclusive;
use std::os::fd::{BorrowedFd, RawFd};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use rustix::fs::FileType;
use rustix::net::{AddressFamily, Protocol, SocketFlags, SocketType, ipproto};

use crate::sock_diag;

/// Where guests listen unless `--port-range` says otherwise: below Linux's
/// default ephemeral range (32768-60999) and the ranges fiberd relays
/// endpoints on (30000-32767 in fiberd, 20000-29999 in Lampyr's own build).
pub const DEFAULT_RANGE: RangeInclusive<u16> = 10000..=19999;

/// The ports held by this helper's live fibers, and the range they come from.
#[derive(Clone)]
pub struct Ports {
    held: Arc<Mutex<HashSet<u16>>>,
    range: RangeInclusive<u16>,
}

/// A port held for one fiber, released when it is dropped.
pub struct Claim {
    pub port: u16,
    ports: Ports,
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Ok(mut held) = self.ports.held.lock() {
            held.remove(&self.port);
        }
    }
}

impl Ports {
    /// Ports from `range`, which must not overlap the range the kernel
    /// picks ephemeral ports from.
    pub fn new(range: RangeInclusive<u16>) -> Result<Self> {
        if range.is_empty() {
            bail!("the port range is empty");
        }
        if let Some(ephemeral) = ephemeral_range()?
            && range.start() <= ephemeral.end()
            && ephemeral.start() <= range.end()
        {
            bail!(
                "the port range {}-{} overlaps the kernel's ephemeral ports {}-{} (net.ipv4.ip_local_port_range)",
                range.start(),
                range.end(),
                ephemeral.start(),
                ephemeral.end()
            );
        }
        Ok(Ports {
            held: Arc::default(),
            range,
        })
    }

    /// A port of the range no fiber holds and nothing on this host listens
    /// on. The search starts at a random place, so fibers do not crowd the
    /// bottom of the range and a resume rarely finds its port taken.
    pub fn claim_fresh(&self) -> Result<Claim> {
        let (start, end) = (*self.range.start() as u64, *self.range.end() as u64);
        let len = end - start + 1;
        let offset = RandomState::new().hash_one(()) % len;
        for i in 0..len {
            let port = (start + (offset + i) % len) as u16;
            if !self.insert(port) {
                continue;
            }
            if unbound(port) {
                return Ok(Claim {
                    port,
                    ports: self.clone(),
                });
            }
            self.remove(port);
        }
        bail!(
            "no free port in {}-{}",
            self.range.start(),
            self.range.end()
        )
    }

    /// `port`, for a resumed guest that re-binds it. Refused when it is
    /// outside the range, as a park made with another `--port-range` can
    /// be, since only the range keeps guests' ephemeral binds off it; when
    /// another fiber holds it, which is a park resumed twice or one resumed
    /// while its parked original is still alive; or when another process
    /// does.
    pub fn claim(&self, port: u16) -> Result<Claim> {
        if !self.range.contains(&port) {
            bail!(
                "port {port}, which the parked guest listens on, is outside this helper's port range {}-{}",
                self.range.start(),
                self.range.end()
            );
        }
        if !self.insert(port) {
            bail!(
                "port {port} belongs to another fiber of this helper: is this park already resumed?"
            );
        }
        let claim = Claim {
            port,
            ports: self.clone(),
        };
        if !unbound(port) {
            bail!("port {port}, which the parked guest listens on, is taken on this host");
        }
        Ok(claim)
    }

    fn insert(&self, port: u16) -> bool {
        self.held
            .lock()
            .map(|mut held| held.insert(port))
            .unwrap_or(false)
    }

    fn remove(&self, port: u16) {
        if let Ok(mut held) = self.held.lock() {
            held.remove(&port);
        }
    }
}

/// The ports every test listens on, from one Ports of the default range,
/// so tests on parallel threads are never handed the same one.
#[cfg(test)]
pub fn test_ports() -> &'static Ports {
    static PORTS: OnceLock<Ports> = OnceLock::new();
    PORTS.get_or_init(|| Ports::new(DEFAULT_RANGE).unwrap())
}

/// A port of the default range nothing listens on, for a test to listen
/// on: bind(0) never gives one out, so no other test listens there by
/// chance. Held, against every other test, until the claim drops.
#[cfg(test)]
pub fn test_port() -> Claim {
    test_ports().claim_fresh().unwrap()
}

/// Whether nothing on this host listens on TCP `port`, in either family:
/// the guest's listener must bind it, and a listener of another process in
/// either would fail the ownership check (`Listeners::of`) later on. One
/// family at a time, since a dual-stack `[::]` conflicts with `0.0.0.0`.
/// A host without IPv6 has nothing listening there.
fn unbound(port: u16) -> bool {
    if TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).is_err() {
        return false;
    }
    match TcpListener::bind((Ipv6Addr::UNSPECIFIED, port)) {
        Ok(_) => true,
        Err(e) => {
            matches!(
                e.kind(),
                ErrorKind::AddrNotAvailable | ErrorKind::Unsupported
            ) || e.raw_os_error() == Some(EAFNOSUPPORT)
        }
    }
}

/// What socket(2) says on a host built without IPv6.
const EAFNOSUPPORT: i32 = 97;

/// Parse `--port-range`'s `LOW-HIGH`.
pub fn parse_range(s: &str) -> Result<RangeInclusive<u16>> {
    let (low, high) = s
        .split_once('-')
        .with_context(|| format!("port range {s:?} is not LOW-HIGH"))?;
    let (low, high): (u16, u16) = (
        low.parse().with_context(|| format!("port {low:?}"))?,
        high.parse().with_context(|| format!("port {high:?}"))?,
    );
    if low == 0 || low > high {
        bail!("port range {s:?} is not LOW-HIGH with 0 < LOW <= HIGH");
    }
    Ok(low..=high)
}

/// The kernel's ephemeral port range, when this host says.
fn ephemeral_range() -> Result<Option<RangeInclusive<u16>>> {
    let Ok(text) = std::fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range") else {
        return Ok(None);
    };
    let mut f = text.split_whitespace().map(str::parse::<u16>);
    match (f.next(), f.next()) {
        (Some(Ok(low)), Some(Ok(high))) => Ok(Some(low..=high)),
        _ => bail!("cannot read net.ipv4.ip_local_port_range {text:?}"),
    }
}

/// This process's listening sockets on a port, by descriptor and
/// identity, taken when every listener on the port was this process's.
///
/// Taking them asks the kernel for the port's listeners and looks for each
/// among this process's descriptors. Checking that each is still the same
/// socket and still listening costs two system calls, or three where
/// sockets are told apart by inode, and one more for each that takes the
/// endpoint's connections, whose device is read again: Linux lets
/// SO_BINDTODEVICE change on a socket that already listens, and one moved
/// off lo leaves 127.0.0.1 on its port free for a socket bound to lo. Its
/// address cannot change once bound, nor can IPV6_V6ONLY. That is enough
/// between takings: while they listen, Linux lets no other socket bind
/// the port on an address whose connections they take, short of
/// `SO_REUSEPORT` set by both the guest and a process of this helper's
/// own user, which could then take every connection. Another address on
/// the port (127.0.0.2, `::1`) can still be bound, but the endpoint's
/// connections to 127.0.0.1 never reach it.
pub struct Listeners {
    sockets: Vec<Held>,
}

/// One of the listeners, as it was taken.
#[derive(Clone, Copy)]
struct Held {
    fd: RawFd,
    id: SocketId,
    /// Whether it took the endpoint's connections, bound to no device or
    /// to lo, so it must stay so bound.
    takes: bool,
}

/// What tells one socket from another among this process's descriptors.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SocketId {
    /// SO_COOKIE, which sock_diag reports too: 64 bits, never reused while
    /// the host is up, and read from a descriptor with one getsockopt,
    /// which fails at once on anything but a socket. Before Linux 5.3
    /// each network namespace counts its own, so a socket from another
    /// can have a listener's cookie and even listen on its address and
    /// port there: the match relies on the helper holding no socket from
    /// another namespace.
    Cookie(u64),
    /// The sockfs inode number, where there is no cookie to match (before
    /// Linux 4.12, or from /proc/net/tcp), or to find an MPTCP listener,
    /// whose cookie sock_diag reports is its subflow's (see
    /// Listeners::of). A file of another filesystem can have the same
    /// number, and so, once the 32-bit count comes round, can another
    /// socket that is still open.
    Inode(u64),
}

impl SocketId {
    /// The identity of the socket under `fd`, of the same kind as `like`;
    /// None for a descriptor that is closed or not a socket.
    fn of(fd: BorrowedFd<'_>, like: SocketId) -> Option<Self> {
        if let SocketId::Cookie(_) = like {
            return rustix::net::sockopt::socket_cookie(fd)
                .ok()
                .map(SocketId::Cookie);
        }
        // SO_TYPE fails at once on what is not a socket, so a file the
        // guest or the helper has open is never stat'ed: on FUSE, NFS or
        // CIFS, a stat can wait on the file's server.
        rustix::net::sockopt::socket_type(fd).ok()?;
        Self::inode(fd)
    }

    /// The inode of the socket under `fd`, which a getsockopt has just
    /// shown to be a socket, so this stat cannot wait on a file's server.
    fn inode(fd: BorrowedFd<'_>) -> Option<Self> {
        let st = rustix::fs::fstat(fd).ok()?;
        // A descriptor swapped for a file since that getsockopt.
        (FileType::from_raw_mode(st.st_mode) == FileType::Socket)
            .then_some(SocketId::Inode(st.st_ino))
    }
}

/// Whether SO_COOKIE can be read on this kernel (Linux 4.12 on). Where it
/// can, sock_diag's idiag_cookie is the same number; before 4.1 that was
/// the socket's address instead, but those kernels have no SO_COOKIE.
fn cookies_readable() -> bool {
    static READABLE: OnceLock<bool> = OnceLock::new();
    *READABLE.get_or_init(|| {
        rustix::net::socket_with(
            AddressFamily::UNIX,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC,
            None,
        )
        .is_ok_and(|s| rustix::net::sockopt::socket_cookie(&s).is_ok())
    })
}

impl Listeners {
    /// None when `port` has no listener, or one that is not this process's,
    /// or when none of them takes the connections the endpoint makes to
    /// 127.0.0.1. The kernel names each listener on the port, and this
    /// process's descriptors name its own. A listener of another process
    /// on any address of the port could take the endpoint's connections,
    /// so one is enough to refuse. And if this process's listeners are all
    /// on other addresses (127.0.0.2, say), 127.0.0.1 on the port is free
    /// for any process to bind, so they are not enough to approve.
    ///
    /// Every fiber of the helper lives in this one process, so neither
    /// step may cost much more with each fiber: the kernel is asked for
    /// this port's listeners alone (sock_diag.rs), and the descriptors are
    /// looked through only until every listener is found.
    pub fn of(port: u16) -> Result<Option<Self>> {
        Self::named(port, &port_listeners(port)?)
    }

    /// Listeners::of, for the listeners the kernel named on `port`.
    fn named(port: u16, found: &[Found]) -> Result<Option<Self>> {
        if found.is_empty() {
            return Ok(None);
        }
        let by_cookie = cookies_readable() && found.iter().all(|l| l.cookie.is_some());
        // An MPTCP listener is not in the TCP tables: the kernel names its
        // first subflow, a TCP socket with a cookie of its own, grafted
        // onto the MPTCP socket's inode from Linux 5.7 on. (5.6, the first
        // with MPTCP, gives the subflow an inode of its own, so a listener
        // there is refused, which fails its birth but approves nothing.)
        // Go asks for MPTCP by default since 1.24, and hyperlight-unikraft
        // makes the host socket as the guest asks, so where listeners are
        // told apart by cookie, one whose cookie no descriptor has is
        // looked for by inode among the MPTCP sockets, and from then on is
        // checked by the MPTCP socket's own cookie.
        let ids = |l: &Found| match l.cookie {
            Some(cookie) if by_cookie => [
                (SocketId::Cookie(cookie), TCP),
                (SocketId::Inode(l.inode), MPTCP),
            ],
            // No cookies, so the inode is matched alone, and is a TCP
            // socket's or an MPTCP subflow's.
            _ => [(SocketId::Inode(l.inode), TCP_OR_MPTCP); 2],
        };
        // After the kernel answered, so the descriptors listed include
        // each listener it named that is still open.
        let mine = own_sockets(found.iter().map(|l| ids(l).map(|(id, _)| id)))?;
        let mut sockets: Vec<Held> = Vec::with_capacity(found.len());
        let mut covered = false;
        for l in found {
            let Some((Mine { fd, held }, protocols)) = ids(l)
                .into_iter()
                .find_map(|(id, protocols)| Some((*mine.get(&id)?, protocols)))
            else {
                return Ok(None);
            };
            // Each socket is one listener (a dual-stack one is named once,
            // in the IPv6 dump). Two named by one inode are a number come
            // round again, and one of them is another socket's.
            if sockets.iter().any(|h| h.fd == fd) {
                return Ok(None);
            }
            // An identity alone can match a socket that is not this
            // listener: a cookie from another network namespace's count,
            // or an inode number come round again. The listener's own
            // address and protocol make that socket this listener.
            if !bound_as(fd, l.addr, port, protocols) {
                return Ok(None);
            }
            let takes = takes_loopback(fd, l.addr, l.v6only) && on_loopback(fd, l.device);
            covered |= takes;
            sockets.push(Held {
                fd,
                id: held,
                takes,
            });
        }
        let listeners = Listeners { sockets };
        // Each is asked whether it still listens, and for its device where
        // that counted, then for its identity again: one the guest shut
        // down or moved off lo after the kernel answered leaves its port
        // free to bind, and a descriptor swapped meanwhile could have
        // answered the questions above for another socket.
        Ok((covered && listeners.still_open()).then_some(listeners))
    }

    /// Whether every one of them is still the same socket under the same
    /// descriptor, and still listening, and each that took the endpoint's
    /// connections is still bound to no device or to lo. A guest can shut
    /// a listener down without closing it, and then another process may
    /// bind its port where the endpoint's connections reach. It cannot
    /// move one to another device, as hyperlight-unikraft's setsockopt
    /// refuses SO_BINDTODEVICE, so the device is read again only in
    /// defence in depth: against the helper process itself moving it.
    pub fn still_open(&self) -> bool {
        self.sockets.iter().all(|&Held { fd, id, takes }| {
            // SAFETY: the descriptor is only borrowed for a few system
            // calls. If the guest has closed it they fail, and if the
            // number now names something else its identity differs;
            // either way, false.
            let fd = unsafe { BorrowedFd::borrow_raw(fd) };
            // Listening and the device first, then the identity: a
            // descriptor swapped between the calls then fails the
            // identity check.
            rustix::net::sockopt::socket_acceptconn(fd).unwrap_or(false)
                && (!takes || bound_to_loopback_or_none(fd))
                && SocketId::of(fd, id) == Some(id)
        })
    }
}

/// A socket listening on the port asked for.
#[derive(Clone, Copy)]
struct Found {
    /// Its SO_COOKIE, which only sock_diag reports.
    cookie: Option<u64>,
    inode: u64,
    addr: IpAddr,
    /// The index of the device it is bound to, or 0 for none, which only
    /// sock_diag reports.
    device: Option<u32>,
    /// Whether it is IPV6_V6ONLY, which only sock_diag reports; only an
    /// IPv6 listener's is read.
    v6only: Option<bool>,
}

/// The sockets listening on TCP `port`, from sock_diag, or from
/// /proc/net/tcp{,6} where it cannot be asked or fails (a seccomp profile
/// that refuses netlink sockets, or a kernel without tcp_diag, say).
fn port_listeners(port: u16) -> Result<Vec<Found>> {
    let err = match sock_diag::tcp_listeners(port) {
        Ok(found) => {
            return Ok(found
                .into_iter()
                .map(|l| Found {
                    cookie: Some(l.cookie),
                    inode: l.inode,
                    addr: l.addr,
                    device: Some(l.device),
                    // The kernel says for every IPv6 listener from Linux
                    // 4.2 on. One it did not is asked, as one from the
                    // /proc tables is (takes_loopback): an MPTCP socket
                    // that cannot answer, before Linux 6.15, then does not
                    // count, but a plain TCP one on `::` still can.
                    v6only: l.v6only,
                })
                .collect());
        }
        // The /proc tables are read in pieces too, so they would be no
        // surer: the search fails, and with it the birth.
        Err(e) if sock_diag::is_split(&e) => {
            return Err(e).with_context(|| format!("find the listeners on port {port}"));
        }
        Err(e) => e,
    };
    // Every search asks sock_diag again, so one failure does not keep the
    // slower tables for good. Each kind of failure is told once, not on
    // every birth, but one unlike those before is still told.
    static SAID: Mutex<Vec<(ErrorKind, Option<i32>)>> = Mutex::new(Vec::new());
    let kind = (err.kind(), err.raw_os_error());
    let mut said = SAID.lock().unwrap_or_else(|e| e.into_inner());
    if !said.contains(&kind) {
        said.push(kind);
        eprintln!("helper: sock_diag: {err}; reading /proc/net/tcp for listeners this time");
    }
    drop(said);
    Ok(proc_listeners(port)?
        .into_iter()
        .map(|(inode, addr)| Found {
            cookie: None,
            inode,
            addr,
            device: None,
            v6only: None,
        })
        .collect())
}

/// The inodes and addresses of the sockets listening on TCP `port`, from
/// /proc/net/tcp{,6}.
fn proc_listeners(port: u16) -> Result<Vec<(u64, IpAddr)>> {
    let mut found = Vec::new();
    for table in ["/proc/self/net/tcp", "/proc/self/net/tcp6"] {
        // A host without IPv6 has no tcp6.
        let Ok(file) = File::open(table) else {
            continue;
        };
        // A table read in part could miss another process's listener, so
        // a failed read fails the search.
        let mut failed = None;
        let lines = BufReader::with_capacity(64 * 1024, file)
            .lines()
            .map_while(|line| line.map_err(|e| failed = Some(e)).ok());
        found.extend(listeners(lines, port));
        if let Some(e) = failed {
            return Err(e).with_context(|| format!("read {table}"));
        }
    }
    // The kernel writes a table a page at a time and finds its place again
    // by counting, so a socket that comes or goes between pages can repeat
    // a listener, dropped here, or hide one, which nothing here can see.
    // sock_diag answers a port in one datagram, which is why it is asked
    // first.
    found.sort_unstable();
    found.dedup();
    Ok(found)
}

/// A descriptor of this process found for a listener, and the identity
/// its record checks it by from then on: the one it was found by, or an
/// MPTCP socket's own cookie where it was found by inode, as a cookie is
/// never reused and an inode number can be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Mine {
    fd: RawFd,
    held: SocketId,
}

/// This process's descriptors for the listeners `ids`, by identity. Each
/// listener is named by a pair of identities, either of which is enough:
/// its cookie and then its inode, or its inode twice. Where listeners are
/// named by cookie, every descriptor is asked for its cookie until each
/// listener is found, and only when one is left unmatched, as an MPTCP
/// listener is, are they searched again for the inode of each MPTCP
/// socket among them. So a plain TCP listener costs one getsockopt a
/// descriptor, and a TCP socket is never matched by an inode number come
/// round to a listener's.
///
/// Nothing maps a socket to its descriptor, and hyperlight-unikraft keeps
/// the guest's to itself, so the open descriptors are searched; but each
/// is asked with a getsockopt or two rather than a path lookup and
/// readlink in /proc/self/fd, and the search stops once every socket is
/// found. It goes from the top: Linux gives out the lowest free number,
/// so under a burst the newest fiber's listener is among the highest.
fn own_sockets(ids: impl IntoIterator<Item = [SocketId; 2]>) -> Result<HashMap<SocketId, Mine>> {
    let listeners: Vec<[SocketId; 2]> = ids.into_iter().collect();
    let Some(&[like, _]) = listeners.first() else {
        return Ok(HashMap::new());
    };
    let wanted: HashSet<SocketId> = listeners.iter().flatten().copied().collect();
    let mut mine = HashMap::with_capacity(wanted.len());
    let unmatched = |mine: &HashMap<SocketId, Mine>| {
        listeners
            .iter()
            .any(|pair| pair.iter().all(|id| !mine.contains_key(id)))
    };
    let fds = open_descriptors()?;
    for &fd in fds.iter().rev() {
        if !unmatched(&mine) {
            break;
        }
        // SAFETY: borrowed for the calls SocketId::of makes. A descriptor
        // closed meanwhile fails them; one reused names another socket,
        // or, if the number now holds a listener the kernel named, holds
        // that socket.
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        if let Some(id) = SocketId::of(borrowed, like)
            && wanted.contains(&id)
        {
            mine.insert(id, Mine { fd, held: id });
        }
    }
    if let SocketId::Cookie(_) = like {
        for &fd in fds.iter().rev() {
            if !unmatched(&mine) {
                break;
            }
            // SAFETY: as above.
            let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
            // SO_PROTOCOL fails at once on what is not a socket, as
            // SO_TYPE does, so only an MPTCP socket is stat'ed.
            if rustix::net::sockopt::socket_protocol(borrowed)
                .is_ok_and(|p| p == Some(ipproto::MPTCP))
                && let Some(inode) = SocketId::inode(borrowed)
                && wanted.contains(&inode)
                && let Ok(cookie) = rustix::net::sockopt::socket_cookie(borrowed)
            {
                mine.insert(
                    inode,
                    Mine {
                        fd,
                        held: SocketId::Cookie(cookie),
                    },
                );
            }
        }
    }
    Ok(mine)
}

/// This process's open descriptors, lowest first, from the names in
/// /proc/self/fd. Only the names are read, never a link, so a listing
/// costs no system call per descriptor. The table's size would not do: it
/// keeps the size a burst grew it to, so asking every slot below it would
/// make each birth after the burst pay for the empty ones.
///
/// A listing taken after the kernel named the listeners holds each one
/// still open: the kernel lists every descriptor open throughout the
/// read. One opened after the kernel answered can be missed, but it is
/// not among the listeners it named either. One closed meanwhile is gone
/// from the search, as it would have failed it; a number reused meanwhile
/// is asked as it is now, which the identity checks then judge.
fn open_descriptors() -> Result<Vec<RawFd>> {
    const DIR: &str = "/proc/self/fd";
    let mut fds = Vec::new();
    for entry in std::fs::read_dir(DIR).with_context(|| format!("list {DIR}"))? {
        // A listing read in part could miss a listener, so an error fails
        // the search.
        let entry = entry.with_context(|| format!("list {DIR}"))?;
        if let Some(fd) = entry.file_name().to_str().and_then(|n| n.parse().ok()) {
            fds.push(fd);
        }
    }
    // The kernel lists them in order, but nothing promises it.
    fds.sort_unstable();
    Ok(fds)
}

/// A listener's protocols, as a descriptor matched to it may have them: a
/// TCP socket, matched by its cookie, an MPTCP one, matched by the inode
/// its TCP subflow shares, or either where only inodes are matched.
const TCP: &[Protocol] = &[ipproto::TCP];
const MPTCP: &[Protocol] = &[ipproto::MPTCP];
const TCP_OR_MPTCP: &[Protocol] = &[ipproto::TCP, ipproto::MPTCP];

/// Whether the socket under `fd` is of one of `protocols` and bound to
/// `addr` and `port`, as the listener the kernel named is; false when it
/// cannot say.
fn bound_as(fd: RawFd, addr: IpAddr, port: u16, protocols: &[Protocol]) -> bool {
    // SAFETY: borrowed for two system calls; a descriptor the guest has
    // closed or reused fails them or answers for another socket, which
    // the identity check `of` makes after it then refuses.
    let fd = unsafe { BorrowedFd::borrow_raw(fd) };
    rustix::net::sockopt::socket_protocol(fd)
        .is_ok_and(|p| p.is_some_and(|p| protocols.contains(&p)))
        && rustix::net::getsockname(fd)
            .ok()
            .and_then(|a| SocketAddr::try_from(a).ok())
            .is_some_and(|a| a.ip() == addr && a.port() == port)
}

/// Whether a listener of this process on `addr` takes connections to
/// 127.0.0.1: one on 127.0.0.1 or 0.0.0.0, or on ::ffff:127.0.0.1 or
/// ::ffff:0.0.0.0, or on `::` without IPV6_V6ONLY, as sock_diag says
/// (`v6only`). The /proc tables do not show it, nor sock_diag before
/// Linux 4.2, so for a listener it is not known of (None) the socket is
/// asked, and one that cannot be asked does not count. The socket is not asked where sock_diag said: an MPTCP
/// socket, as Go makes, answers getsockopt(IPV6_V6ONLY) only from Linux
/// 6.15, and EOPNOTSUPP before. Its setsockopt works from 5.9; before
/// that, Go ignores the error, and the socket keeps net.ipv6.bindv6only,
/// which sock_diag reports too. A v4-mapped address needs no asking:
/// Linux refuses to bind one with IPV6_V6ONLY set, or to set it once
/// bound.
fn takes_loopback(fd: RawFd, addr: IpAddr, v6only: Option<bool>) -> bool {
    let v4_takes = |v4: Ipv4Addr| v4 == Ipv4Addr::LOCALHOST || v4.is_unspecified();
    match addr {
        IpAddr::V4(v4) => v4_takes(v4),
        IpAddr::V6(v6) if v6.to_ipv4_mapped().is_some_and(v4_takes) => true,
        IpAddr::V6(v6) if v6.is_unspecified() => match v6only {
            Some(only) => !only,
            None => {
                // SAFETY: borrowed for one system call; a descriptor the
                // guest has closed or reused fails it or answers for
                // another socket, which the identity check `of` makes
                // after it then refuses.
                let fd = unsafe { BorrowedFd::borrow_raw(fd) };
                rustix::net::sockopt::ipv6_v6only(fd).is_ok_and(|only| !only)
            }
        },
        IpAddr::V6(_) => false,
    }
}

/// Whether a listener of this process bound to `device` takes connections
/// that come in on lo, as the endpoint's to 127.0.0.1 do: one bound to no
/// device, or to lo. One bound to another device leaves 127.0.0.1 on its
/// port free for a socket bound to lo, which takes them. None is a
/// listener from the /proc tables, which do not show the device, so the
/// socket is asked; one that cannot be asked does not count.
fn on_loopback(fd: RawFd, device: Option<u32>) -> bool {
    match device {
        Some(device) => device == 0 || device == LOOPBACK_INDEX,
        None => {
            // SAFETY: borrowed for one system call; a descriptor the guest
            // has closed or reused fails it or answers for another socket,
            // which the identity check `of` makes after it then refuses.
            let fd = unsafe { BorrowedFd::borrow_raw(fd) };
            bound_to_loopback_or_none(fd)
        }
    }
}

/// lo's index in every network namespace: Linux registers lo first in
/// each and checks it gets this index (LOOPBACK_IFINDEX, since 3.7). A
/// constant rather than asking, since an answer asked through a socket the
/// guest may have closed could fail, and a failure kept would refuse every
/// lo-bound listener from then on. On a kernel where lo had another index
/// such a listener is refused, which fails the birth but approves nothing.
const LOOPBACK_INDEX: u32 = 1;

/// Whether the socket under `fd` is bound to no device, or to lo, by its
/// SO_BINDTODEVICE, which rustix does not read; false when it cannot say.
fn bound_to_loopback_or_none(fd: BorrowedFd<'_>) -> bool {
    use std::os::fd::AsRawFd;
    let mut name = [0u8; libc::IFNAMSIZ];
    let mut len = name.len() as libc::socklen_t;
    // SAFETY: the kernel writes at most `len` bytes of the name to it, and
    // says in `len` how many.
    let got = unsafe {
        libc::getsockopt(
            fd.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_BINDTODEVICE,
            name.as_mut_ptr().cast(),
            &mut len,
        )
    };
    // No device is an empty name; a name is counted with its NUL. A split
    // always yields a first piece, empty for an empty name.
    let name = &name[..(len as usize).min(name.len())];
    got == 0 && matches!(name.split(|&b| b == 0).next(), Some(b"" | b"lo"))
}

/// The inodes and addresses of the sockets listening on `port` in the
/// lines of a /proc/net/tcp or tcp6 table, header first.
///
/// The kernel writes every listener before any other socket (tcp_get_idx
/// and tcp_seq_next walk the listening hash, then the established one),
/// and the listening hash holds only sockets in LISTEN: one is hashed
/// there after its state is set and unhashed before it changes, under the
/// bucket lock the walk holds (inet_csk_listen_start, tcp_set_state).
/// Before Linux 4.4 the walk also wrote each listener's half-open
/// connections (SYN_RECV, 03) right after it, among the listeners. So the
/// first line in neither state ends the listeners, and the reading stops
/// there instead of going through every connection on the host.
fn listeners<S: AsRef<str>>(
    lines: impl IntoIterator<Item = S>,
    port: u16,
) -> impl Iterator<Item = (u64, IpAddr)> {
    let suffix = format!(":{port:04X}");
    lines
        .into_iter()
        .skip(1)
        .map_while(move |line| {
            let f: Vec<&str> = line.as_ref().split_whitespace().collect();
            // local_address, st (0A is LISTEN), inode.
            if f.len() <= 9 {
                return None;
            }
            match f[3] {
                "0A" => {}
                "03" => return Some(None),
                _ => return None,
            }
            let on_port = |addr: &str| Some((f[9].parse().ok()?, proc_addr(addr)?));
            Some(f[1].strip_suffix(&suffix).and_then(on_port))
        })
        .flatten()
}

/// An address as /proc/net/tcp{,6} writes it: 32-bit words in hex, each in
/// the host's byte order.
fn proc_addr(hex: &str) -> Option<IpAddr> {
    let word = |i: usize| -> Option<[u8; 4]> {
        Some(
            u32::from_str_radix(hex.get(i * 8..i * 8 + 8)?, 16)
                .ok()?
                .to_ne_bytes(),
        )
    };
    match hex.len() {
        8 => Some(IpAddr::V4(Ipv4Addr::from(word(0)?))),
        32 => {
            let mut bytes = [0u8; 16];
            for i in 0..4 {
                bytes[i * 4..i * 4 + 4].copy_from_slice(&word(i)?);
            }
            Some(IpAddr::V6(Ipv6Addr::from(bytes)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ports() -> Ports {
        Ports::new(DEFAULT_RANGE).unwrap()
    }

    /// Whether TCP `port` has listeners and every one of them is this
    /// process's.
    fn only_this_process_listens(port: u16) -> Result<bool> {
        Ok(Listeners::of(port)?.is_some())
    }

    #[test]
    fn a_port_belongs_to_one_fiber_until_released() {
        // A port held against every other test, so none binds it between
        // the release and the claim again, in a set of that port alone.
        let held = test_port();
        let ports = Ports {
            held: Arc::default(),
            range: held.port..=held.port,
        };
        let first = ports.claim_fresh().unwrap();
        assert!(DEFAULT_RANGE.contains(&first.port));
        let err = ports.claim(first.port).err().unwrap().to_string();
        assert!(err.contains("another fiber"), "{err}");
        let port = first.port;
        drop(first);
        assert_eq!(ports.claim(port).unwrap().port, port);
    }

    #[test]
    fn a_port_another_process_listens_on_is_refused() {
        let ports = ports();
        let claim = test_port();
        let port = claim.port;
        let taken = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).unwrap();
        let err = ports.claim(port).err().unwrap().to_string();
        assert!(err.contains("taken on this host"), "{err}");
        // A refused claim holds nothing.
        drop(taken);
        assert!(ports.claim(port).is_ok());
    }

    /// A listener on IPv6 alone is not seen by an IPv4 bind.
    #[test]
    fn a_port_another_process_listens_on_over_ipv6_is_refused() {
        let ports = ports();
        let claim = test_port();
        let port = claim.port;
        let taken = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).unwrap();
        let err = ports.claim(port).err().unwrap().to_string();
        assert!(err.contains("taken on this host"), "{err}");
        // A fresh claim passes it over: a range of that port alone is full.
        let only = Ports {
            held: Arc::default(),
            range: port..=port,
        };
        assert!(only.claim_fresh().is_err());
        drop(taken);
        assert!(only.claim_fresh().is_ok());
    }

    #[test]
    fn a_port_outside_the_range_is_refused() {
        let ports = ports();
        let port = DEFAULT_RANGE.end() + 1;
        let err = ports.claim(port).err().unwrap().to_string();
        assert!(err.contains("outside this helper's port range"), "{err}");
        assert!(ports.insert(port), "a refused claim holds nothing");
    }

    /// A parked guest that closed a connection first leaves it in TIME_WAIT
    /// on its port, which a listener with SO_REUSEADDR, as the guest's is,
    /// binds over. The claim's probe must too.
    #[test]
    fn a_port_left_in_time_wait_can_be_claimed() {
        use std::io::Read;
        use std::net::{Shutdown, TcpStream};
        let ports = ports();
        let claim = test_port();
        let port = claim.port;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        let mut client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        let (server, _) = listener.accept().unwrap();
        // The listening side closes first, so its end is the one in TIME_WAIT.
        server.shutdown(Shutdown::Both).unwrap();
        let _ = client.read(&mut [0u8; 1]);
        drop(client);
        drop(server);
        drop(listener);
        let table = std::fs::read_to_string("/proc/self/net/tcp").unwrap();
        let time_wait = table.lines().skip(1).any(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            f.len() > 3 && f[1].ends_with(&format!(":{port:04X}")) && f[3] == "06"
        });
        assert!(time_wait, "port {port} is not in TIME_WAIT");
        assert!(ports.claim(port).is_ok());
    }

    #[test]
    fn fresh_claims_skip_taken_ports_and_stop_when_the_range_is_full() {
        // Ports of the default range, not ephemeral ones: the kernel gives
        // bind(0) odd ports and connect() the even ones beside them, so an
        // ephemeral port's neighbour is often a connection of another test.
        // The neighbour is claimed too, so no other test listens there.
        let (_a, _claims, taken) = loop {
            let claim = test_port();
            let port = claim.port;
            if port < *DEFAULT_RANGE.end()
                && let Ok(next) = test_ports().claim(port + 1)
            {
                break (
                    TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).unwrap(),
                    (claim, next),
                    port,
                );
            }
        };
        // A two-port range around a taken port has one port to give.
        let ports = Ports {
            held: Arc::default(),
            range: taken..=taken + 1,
        };
        let only = ports.claim_fresh().unwrap();
        assert_eq!(only.port, taken + 1);
        assert!(ports.claim_fresh().is_err());
    }

    #[test]
    fn a_range_over_the_ephemeral_ports_is_refused() {
        let Some(ephemeral) = ephemeral_range().unwrap() else {
            return;
        };
        let err = Ports::new(*ephemeral.start()..=*ephemeral.start())
            .err()
            .unwrap()
            .to_string();
        assert!(err.contains("ephemeral"), "{err}");
    }

    #[test]
    fn port_ranges_parse_as_low_dash_high() {
        assert_eq!(parse_range("10000-19999").unwrap(), 10000..=19999);
        for bad in ["", "10000", "0-5", "9-8", "a-b", "1-70000"] {
            assert!(parse_range(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_port_is_ours_only_when_every_listener_on_it_is() {
        // A port of the default range: another test can listen on an
        // ephemeral one as soon as it is free.
        let claim = test_port();
        let port = claim.port;
        let ours = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        assert!(only_this_process_listens(port).unwrap());
        drop(ours);
        assert!(!only_this_process_listens(port).unwrap());
    }

    #[test]
    fn taken_listeners_stay_open_until_their_socket_closes() {
        use std::os::fd::OwnedFd;
        // Ports of the default range: a listener of another test on an
        // ephemeral one would join the record, and leave it when dropped.
        let claim = test_port();
        let port = claim.port;
        let mut ours = OwnedFd::from(TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap());
        let known = Listeners::of(port).unwrap().unwrap();
        assert!(known.still_open());
        // A new listener under the reused descriptor is not the old one.
        // dup2 closes the old socket and puts the new one under its number
        // at once, so no other test's thread can take the number between.
        let other_port = test_port();
        let other = TcpListener::bind((Ipv4Addr::LOCALHOST, other_port.port)).unwrap();
        rustix::io::dup2(&other, &mut ours).unwrap();
        assert!(
            !known.still_open(),
            "a listener under a reused descriptor counts"
        );
        drop(ours);
        assert!(!known.still_open());
    }

    #[test]
    fn a_listener_shut_down_but_not_closed_is_not_open() {
        let claim = test_port();
        let port = claim.port;
        let ours = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        let known = Listeners::of(port).unwrap().unwrap();
        assert!(known.still_open());
        rustix::net::shutdown(&ours, rustix::net::Shutdown::Read).unwrap();
        assert!(!known.still_open(), "a shut-down listener still counts");
    }

    #[test]
    fn proc_net_tcp6_addresses_are_read_in_host_order() {
        assert_eq!(
            proc_addr("00000000000000000000000000000000"),
            Some(IpAddr::V6(Ipv6Addr::UNSPECIFIED))
        );
        assert_eq!(
            proc_addr("0000000000000000FFFF00000100007F"),
            Some(IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped()))
        );
        assert_eq!(
            proc_addr("00000000000000000000000001000000"),
            Some(IpAddr::V6(Ipv6Addr::LOCALHOST))
        );
        assert_eq!(
            proc_addr("0200007F"),
            Some(IpAddr::V4([127, 0, 0, 2].into()))
        );
    }

    /// A guest listening on two addresses closes its 127.0.0.1 one: what is
    /// left does not take the endpoint's connections, and 127.0.0.1 on the
    /// port is free to bind, so the record no longer approves.
    #[test]
    fn listeners_must_take_the_endpoint_s_loopback_connections() {
        // A port of the default range: on an ephemeral one, another test's
        // connection can hold 127.0.0.1's end of it.
        let claim = test_port();
        let port = claim.port;
        let _other = TcpListener::bind((Ipv4Addr::new(127, 0, 0, 2), port)).unwrap();
        let loopback = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        let known = Listeners::of(port).unwrap().expect("127.0.0.1 is covered");
        drop(loopback);
        assert!(!known.still_open());
        assert!(
            Listeners::of(port).unwrap().is_none(),
            "127.0.0.2 alone approved"
        );
        assert!(!only_this_process_listens(port).unwrap());
    }

    #[test]
    fn ipv6_listeners_take_loopback_only_when_dual_stack_or_mapped() {
        use std::os::fd::AsRawFd;
        let v6 = |addr: Ipv6Addr, only: bool, port: u16| {
            let s = rustix::net::socket(
                rustix::net::AddressFamily::INET6,
                rustix::net::SocketType::STREAM,
                None,
            )
            .unwrap();
            rustix::net::sockopt::set_ipv6_v6only(&s, only).unwrap();
            rustix::net::bind(&s, &std::net::SocketAddrV6::new(addr, port, 0, 0)).unwrap();
            rustix::net::listen(&s, 1).unwrap();
            s
        };
        // Ports of the default range: a v6-only socket or one on ::1
        // leaves the port free on 127.0.0.1, so on an ephemeral one another
        // test's listener could share it, and join or leave its record.
        let any = IpAddr::V6(Ipv6Addr::UNSPECIFIED);
        let dual_port = test_port();
        let dual = v6(Ipv6Addr::UNSPECIFIED, false, dual_port.port);
        // A listener from the /proc tables (None) is asked.
        assert!(takes_loopback(dual.as_raw_fd(), any, None));
        let claim = test_port();
        let port = claim.port;
        let only = v6(Ipv6Addr::UNSPECIFIED, true, port);
        assert!(!takes_loopback(only.as_raw_fd(), any, None));
        // One sock_diag reported is not: its word is taken over the
        // socket's, which an MPTCP socket cannot give before Linux 6.15.
        assert!(takes_loopback(only.as_raw_fd(), any, Some(false)));
        assert!(!takes_loopback(dual.as_raw_fd(), any, Some(true)));
        let lo_port = test_port();
        let lo = v6(Ipv6Addr::LOCALHOST, false, lo_port.port);
        for v6only in [None, Some(false)] {
            assert!(!takes_loopback(
                lo.as_raw_fd(),
                IpAddr::V6(Ipv6Addr::LOCALHOST),
                v6only
            ));
        }
        let mapped = IpAddr::V6(Ipv4Addr::LOCALHOST.to_ipv6_mapped());
        assert!(takes_loopback(lo.as_raw_fd(), mapped, None));
        // ::ffff:0.0.0.0 takes every IPv4 address, 127.0.0.1 among them.
        let mapped_any = IpAddr::V6(Ipv4Addr::UNSPECIFIED.to_ipv6_mapped());
        assert!(takes_loopback(lo.as_raw_fd(), mapped_any, None));
        let mapped_other = IpAddr::V6(Ipv4Addr::new(127, 0, 0, 2).to_ipv6_mapped());
        assert!(!takes_loopback(lo.as_raw_fd(), mapped_other, None));
        // And a v6-only listener alone is no record.
        assert!(Listeners::of(port).unwrap().is_none());
    }

    /// A listener on ::ffff:127.0.0.1, as the kernel reports it, takes the
    /// endpoint's connections.
    #[test]
    fn a_v4_mapped_listener_is_a_record() {
        let claim = test_port();
        let port = claim.port;
        let s = rustix::net::socket(AddressFamily::INET6, SocketType::STREAM, None).unwrap();
        rustix::net::sockopt::set_ipv6_v6only(&s, false).unwrap();
        let mapped = Ipv4Addr::LOCALHOST.to_ipv6_mapped();
        if rustix::net::bind(&s, &std::net::SocketAddrV6::new(mapped, port, 0, 0)).is_err() {
            // A host without IPv6.
            return;
        }
        rustix::net::listen(&s, 1).unwrap();
        let found = sock_diag::tcp_listeners(port).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].addr, IpAddr::V6(mapped));
        assert!(Listeners::of(port).unwrap().is_some());
    }

    /// An MPTCP stream socket of `family`. Where it cannot be made (a
    /// kernel without MPTCP, or net.mptcp.enabled=0) the test fails,
    /// unless LAMPYR_HELPER_NO_MPTCP=1 says the host has none,
    /// and then there is nothing to test: Go listens over TCP there.
    fn mptcp_socket(family: AddressFamily) -> Option<rustix::fd::OwnedFd> {
        match rustix::net::socket(family, SocketType::STREAM, Some(ipproto::MPTCP)) {
            Ok(s) => Some(s),
            Err(_) if std::env::var_os("LAMPYR_HELPER_NO_MPTCP").is_some_and(|v| v == "1") => None,
            Err(e) => panic!(
                "cannot make an MPTCP socket of {family:?} ({e}); set \
                 LAMPYR_HELPER_NO_MPTCP=1 on a host without MPTCP"
            ),
        }
    }

    /// The identity `known` checks its one listener by.
    fn held_id(known: &Listeners) -> SocketId {
        assert_eq!(known.sockets.len(), 1);
        known.sockets[0].id
    }

    /// What Go's net.Listen("tcp", ":PORT") makes, which
    /// hyperlight-unikraft's hostnet makes on the host as asked: one
    /// AF_INET6 socket on `::` with IPV6_V6ONLY off, of IPPROTO_MPTCP,
    /// which Go asks for by default since 1.24. The kernel names its TCP
    /// subflow, which has a cookie of its own and the socket's inode, and
    /// says it is dual-stack: the socket itself cannot before Linux 6.15.
    #[test]
    fn a_go_style_dual_stack_mptcp_listener_is_a_record() {
        let claim = test_port();
        let port = claim.port;
        let Some(s) = mptcp_socket(AddressFamily::INET6) else {
            return;
        };
        // Before Linux 5.9 an MPTCP socket refuses this, and Go ignores
        // the error: the socket keeps net.ipv6.bindv6only, 0 by default.
        let _ = rustix::net::sockopt::set_ipv6_v6only(&s, false);
        rustix::net::bind(
            &s,
            &std::net::SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, port, 0, 0),
        )
        .unwrap();
        rustix::net::listen(&s, 1).unwrap();
        let found = sock_diag::tcp_listeners(port).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].addr, IpAddr::V6(Ipv6Addr::UNSPECIFIED));
        assert_eq!(found[0].v6only, Some(false));
        let known = Listeners::of(port).unwrap().expect("Go's listener refused");
        // Checked from then on by the MPTCP socket's own cookie, which is
        // never reused, not by the inode it was found by.
        let cookie = rustix::net::sockopt::socket_cookie(&s).unwrap();
        assert_eq!(held_id(&known), SocketId::Cookie(cookie));
        assert!(known.still_open());
        drop(s);
        assert!(!known.still_open());
    }

    /// An IPv4 MPTCP listener on 127.0.0.1 is found by its inode too, and
    /// held by its own cookie.
    #[test]
    fn an_ipv4_mptcp_listener_is_a_record() {
        let claim = test_port();
        let port = claim.port;
        let Some(s) = mptcp_socket(AddressFamily::INET) else {
            return;
        };
        rustix::net::bind(&s, &std::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).unwrap();
        rustix::net::listen(&s, 1).unwrap();
        let found = sock_diag::tcp_listeners(port).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].addr, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(
            found[0].cookie,
            rustix::net::sockopt::socket_cookie(&s).unwrap(),
            "the kernel named the MPTCP socket, not its subflow"
        );
        let known = Listeners::of(port)
            .unwrap()
            .expect("an MPTCP listener refused");
        let cookie = rustix::net::sockopt::socket_cookie(&s).unwrap();
        assert_eq!(held_id(&known), SocketId::Cookie(cookie));
        assert!(known.still_open());
        drop(s);
        assert!(!known.still_open());
    }

    /// A listener as sock_diag reports the TCP socket `s`.
    fn reported(s: &TcpListener) -> Found {
        let addr = s.local_addr().unwrap().ip();
        Found {
            cookie: Some(rustix::net::sockopt::socket_cookie(s).unwrap()),
            inode: rustix::fs::fstat(s).unwrap().st_ino,
            addr,
            device: Some(0),
            v6only: addr.is_ipv6().then_some(false),
        }
    }

    /// Where listeners are told apart by cookie, a TCP socket whose inode
    /// is a listener's but whose cookie is not is never taken for it: only
    /// an MPTCP socket is matched by inode, as an inode number comes round
    /// again and a cookie does not.
    #[test]
    fn a_tcp_socket_is_not_a_listener_by_its_inode_alone() {
        // An ephemeral port will do: the listeners are named here, not
        // asked of the kernel.
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = tcp.local_addr().unwrap().port();
        assert!(Listeners::named(port, &[reported(&tcp)]).unwrap().is_some());
        let other_cookie = Found {
            cookie: Some(u64::MAX),
            ..reported(&tcp)
        };
        assert!(
            Listeners::named(port, &[other_cookie]).unwrap().is_none(),
            "a TCP socket matched by its inode"
        );
    }

    /// Two listeners the kernel named that are one descriptor of this
    /// process, as two sockets whose inode numbers met would be, are
    /// refused: one of them is a socket this process does not have.
    #[test]
    fn two_listeners_on_one_descriptor_are_refused() {
        let tcp = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = tcp.local_addr().unwrap().port();
        let one = reported(&tcp);
        assert!(Listeners::named(port, &[one]).unwrap().is_some());
        // As from the /proc tables, by inode alone.
        let by_inode = Found {
            cookie: None,
            v6only: None,
            device: None,
            ..one
        };
        assert!(Listeners::named(port, &[by_inode]).unwrap().is_some());
        assert!(
            Listeners::named(port, &[one, one]).unwrap().is_none(),
            "two by one cookie"
        );
        assert!(
            Listeners::named(port, &[by_inode, by_inode])
                .unwrap()
                .is_none(),
            "two by one inode"
        );
    }

    /// Bind `s` to the device `name` (SO_BINDTODEVICE).
    fn bind_to_device(s: &impl std::os::fd::AsRawFd, name: &str) -> std::io::Result<()> {
        // SAFETY: the name is a valid buffer of its length.
        let set = unsafe {
            libc::setsockopt(
                s.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                name.as_ptr().cast(),
                name.len() as libc::socklen_t,
            )
        };
        if set == 0 {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    /// A descriptor matches a listener only on the listener's own TCP
    /// address and port, so a socket that only shares its identity is
    /// never taken for it.
    #[test]
    fn a_socket_is_a_listener_only_at_its_tcp_address() {
        use std::os::fd::AsRawFd;
        // Ephemeral ports will do: nothing binds them again, and nothing
        // here asks what else listens there.
        let localhost = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let tcp = TcpListener::bind((localhost, 0)).unwrap();
        let (fd, port) = (tcp.as_raw_fd(), tcp.local_addr().unwrap().port());
        assert!(bound_as(fd, localhost, port, TCP));
        assert!(bound_as(fd, localhost, port, TCP_OR_MPTCP));
        // A TCP socket is never taken for an MPTCP listener.
        assert!(!bound_as(fd, localhost, port, MPTCP));
        assert!(!bound_as(fd, IpAddr::V4(Ipv4Addr::UNSPECIFIED), port, TCP));
        assert!(!bound_as(fd, IpAddr::V6(Ipv6Addr::LOCALHOST), port, TCP));
        assert!(!bound_as(fd, localhost, port.wrapping_add(1), TCP));
        let udp = std::net::UdpSocket::bind((localhost, 0)).unwrap();
        let udp_port = udp.local_addr().unwrap().port();
        assert!(!bound_as(
            udp.as_raw_fd(),
            localhost,
            udp_port,
            TCP_OR_MPTCP
        ));
    }

    /// A network device of this host other than lo, if it has one. Only
    /// entries with an ifindex are devices: /sys/class/net also holds
    /// files, such as bonding_masters with the bonding module loaded, that
    /// a socket cannot be bound to, which would skip the tests' checks.
    fn other_device() -> Option<String> {
        let net = std::path::Path::new("/sys/class/net");
        std::fs::read_dir(net)
            .unwrap()
            .filter_map(|d| d.ok()?.file_name().into_string().ok())
            .find(|name| name != "lo" && net.join(name).join("ifindex").exists())
    }

    /// A listener on 0.0.0.0 bound to lo takes the endpoint's connections,
    /// and one bound to another device does not: 127.0.0.1 on its port is
    /// free for a socket bound to lo. The /proc tables' listeners, which
    /// carry no device, are asked for theirs.
    #[test]
    fn a_listener_bound_to_another_device_is_no_record() {
        use std::os::fd::AsRawFd;
        let listen_on = |device: Option<&str>| {
            let claim = test_port();
            let port = claim.port;
            let s = rustix::net::socket(AddressFamily::INET, SocketType::STREAM, None).unwrap();
            if let Some(device) = device {
                // Unprivileged from Linux 5.7.
                bind_to_device(&s, device).ok()?;
            }
            let any = std::net::SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port);
            rustix::net::bind(&s, &any).unwrap();
            rustix::net::listen(&s, 1).unwrap();
            Some((s, port, claim))
        };
        let (none, port, _claim) = listen_on(None).unwrap();
        assert!(Listeners::of(port).unwrap().is_some());
        assert!(on_loopback(none.as_raw_fd(), None));
        if let Some((lo, port, _claim)) = listen_on(Some("lo")) {
            assert!(Listeners::of(port).unwrap().is_some());
            assert!(on_loopback(lo.as_raw_fd(), None));
        }
        if let Some((elsewhere, port, _claim)) =
            other_device().and_then(|name| listen_on(Some(&name)))
        {
            assert!(
                Listeners::of(port).unwrap().is_none(),
                "a listener bound to another device approved"
            );
            assert!(!on_loopback(elsewhere.as_raw_fd(), None));
        }
    }

    /// A listener moved to another device after it was taken no longer
    /// takes the endpoint's connections, so the record no longer holds.
    #[test]
    fn a_listener_moved_to_another_device_is_not_open() {
        let Some(other) = other_device() else {
            return;
        };
        // Each from no device: unprivileged, Linux 5.7 on lets a socket be
        // bound to a device once, and moving it again needs CAP_NET_RAW.
        let moved_to = |device: &str| {
            let claim = test_port();
            let port = claim.port;
            let ours = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).unwrap();
            let known = Listeners::of(port).unwrap().unwrap();
            assert!(known.still_open());
            bind_to_device(&ours, device)
                .ok()
                .map(|()| known.still_open())
        };
        if let Some(open) = moved_to("lo") {
            assert!(open, "a listener moved to lo is refused");
        }
        if let Some(open) = moved_to(&other) {
            assert!(!open, "a listener moved off lo still counts");
        }
    }

    #[test]
    fn proc_net_tcp_listeners_are_read_by_port_and_state() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   \
            0: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1\n   \
            1: 0100007F:1F90 0100007F:D431 01 00000000:00000000 00:00000000 00000000  1000        0 4343 1\n";
        assert_eq!(
            listeners(table.lines(), 8080).collect::<Vec<_>>(),
            vec![(4242, IpAddr::V4(Ipv4Addr::LOCALHOST))]
        );
        assert!(listeners(table.lines(), 8081).next().is_none());
    }

    /// Listeners come first, with only a pre-4.4 kernel's half-open
    /// connections among them, so the first line in another state ends
    /// them, and nothing after it is read.
    #[test]
    fn proc_net_tcp_listeners_end_at_the_first_other_state() {
        let table = "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n   \
            0: 0100007F:1F91 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4141 1\n   \
            1: 0100007F:1F91 0100007F:D431 03 00000000:00000000 00:00000000 00000000  1000        0 0 1\n   \
            2: 0100007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4242 1\n   \
            3: 0100007F:1F90 0100007F:D432 01 00000000:00000000 00:00000000 00000000  1000        0 4343 1\n   \
            4: 0200007F:1F90 00000000:0000 0A 00000000:00000000 00:00000000 00000000  1000        0 4444 1\n";
        let read = std::cell::Cell::new(0);
        let lines = table.lines().inspect(|_| read.set(read.get() + 1));
        assert_eq!(
            listeners(lines, 8080).collect::<Vec<_>>(),
            vec![(4242, IpAddr::V4(Ipv4Addr::LOCALHOST))]
        );
        assert_eq!(read.get(), 5, "lines after the listeners were read");
    }

    /// The /proc tables, read only as far as their listeners, find what
    /// sock_diag does, where both can be asked.
    #[test]
    fn proc_net_tcp_and_sock_diag_agree() {
        use std::net::TcpStream;
        let claim = test_port();
        let port = claim.port;
        let _v4 = TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).unwrap();
        let _v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port));
        let _client = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        let mut proc = proc_listeners(port).unwrap();
        let mut diag: Vec<_> = sock_diag::tcp_listeners(port)
            .unwrap()
            .into_iter()
            .map(|l| (l.inode, l.addr))
            .collect();
        proc.sort();
        diag.sort();
        assert!(!proc.is_empty());
        assert_eq!(proc, diag);
    }

    /// Sockets are found among this process's descriptors by cookie or by
    /// inode, and only sockets.
    #[test]
    fn own_sockets_are_found_by_cookie_or_inode() {
        use std::os::fd::AsRawFd;
        let a = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let b = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let cookie: fn(&TcpListener) -> SocketId =
            |s| SocketId::Cookie(rustix::net::sockopt::socket_cookie(s).unwrap());
        let ino: fn(&TcpListener) -> SocketId =
            |s| SocketId::Inode(rustix::fs::fstat(s).unwrap().st_ino);
        let fd_of = |mine: &HashMap<SocketId, Mine>, id| mine.get(&id).map(|m| m.fd);
        for id in [cookie, ino] {
            let mine = own_sockets([[id(&a); 2], [id(&b); 2]]).unwrap();
            assert_eq!(fd_of(&mine, id(&a)), Some(a.as_raw_fd()));
            assert_eq!(fd_of(&mine, id(&b)), Some(b.as_raw_fd()));
            assert_eq!(mine[&id(&a)].held, id(&a));
        }
        // A cookie no descriptor has, as an MPTCP listener's is, and then
        // the inode, which a TCP socket's never matches.
        let mine = own_sockets([[SocketId::Cookie(u64::MAX), ino(&a)]]).unwrap();
        assert_eq!(fd_of(&mine, ino(&a)), None);
        // An MPTCP socket's does, and it is held by its own cookie.
        if let Some(m) = mptcp_socket(AddressFamily::INET) {
            let m_ino = SocketId::Inode(rustix::fs::fstat(&m).unwrap().st_ino);
            let m_cookie = SocketId::Cookie(rustix::net::sockopt::socket_cookie(&m).unwrap());
            let mine = own_sockets([[SocketId::Cookie(u64::MAX), m_ino]]).unwrap();
            assert_eq!(
                mine.get(&m_ino),
                Some(&Mine {
                    fd: m.as_raw_fd(),
                    held: m_cookie
                })
            );
        }
        // Ones no descriptor of this process names.
        assert!(
            own_sockets([[SocketId::Cookie(u64::MAX); 2]])
                .unwrap()
                .is_empty()
        );
        assert!(
            own_sockets([[SocketId::Inode(u64::MAX); 2]])
                .unwrap()
                .is_empty()
        );
        // A file's inode number is another filesystem's: it never counts.
        let file = tempfile::tempfile().unwrap();
        let file_ino = SocketId::Inode(rustix::fs::fstat(&file).unwrap().st_ino);
        assert_ne!(
            fd_of(&own_sockets([[file_ino; 2]]).unwrap(), file_ino),
            Some(file.as_raw_fd())
        );
    }

    /// Held by the tests that fill the descriptor table, so neither times
    /// its search through the other's descriptors.
    static FULL_TABLE: Mutex<()> = Mutex::new(());

    /// The descriptor table keeps the size a burst grew it to after the
    /// burst's fibers end, and a search must not pay for its empty slots:
    /// only the open descriptors are asked. A listener with tens of
    /// thousands of descriptors opened above it and closed again is found
    /// in well under the time asking every slot of the table takes, which
    /// is measured beside it, so a slow host slows both.
    #[test]
    fn listeners_are_found_past_a_drained_burst() {
        use std::hint::black_box;
        use std::time::{Duration, Instant};
        const BURST: u64 = 40_000;
        let _full = FULL_TABLE.lock().unwrap_or_else(|e| e.into_inner());
        // The burst's descriptors on top of what the other tests hold at
        // once, which a process may raise its soft limit to itself. The
        // test that fills the table with connections holds FULL_TABLE too,
        // so the rest hold few. Where the hard limit is lower, the burst is
        // as big as it allows, and the test says so.
        let spare = 1024;
        let mut nofile = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        let allowed = nofile
            .maximum
            .map_or(BURST + spare, |hard| hard.min(BURST + spare));
        if nofile.current.is_some_and(|soft| soft < allowed) {
            nofile.current = Some(allowed);
            rustix::process::setrlimit(rustix::process::Resource::Nofile, nofile)
                .unwrap_or_else(|e| panic!("raise RLIMIT_NOFILE to {allowed}: {e}"));
        }
        let burst = allowed.saturating_sub(spare).min(BURST);
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let null = File::open("/dev/null").unwrap();
        let held: Vec<_> = (0..burst)
            .map(|_| rustix::io::dup(&null).unwrap())
            .collect();
        drop(held);
        let table: RawFd = std::fs::read_to_string("/proc/self/status")
            .unwrap()
            .lines()
            .find_map(|line| line.strip_prefix("FDSize:"))
            .and_then(|n| n.trim().parse().ok())
            .unwrap();
        fn median(mut call: impl FnMut()) -> Duration {
            let mut took: Vec<Duration> = (0..9)
                .map(|_| {
                    let start = Instant::now();
                    call();
                    start.elapsed()
                })
                .collect();
            took.sort();
            took[took.len() / 2]
        }
        let search = median(|| assert!(Listeners::of(port).unwrap().is_some()));
        // What a search that asks every slot pays for the slots alone.
        let slots = median(|| {
            for fd in (0..table).rev() {
                // SAFETY: borrowed for one getsockopt, which a closed slot
                // fails.
                let fd = unsafe { BorrowedFd::borrow_raw(fd) };
                black_box(SocketId::of(fd, SocketId::Cookie(0)));
            }
        });
        let open = open_descriptors().unwrap().len();
        let limited = if burst < BURST {
            format!(
                " (RLIMIT_NOFILE's hard limit allows {burst} of the {BURST}{})",
                if burst < MIN_BURST {
                    ", too few to time"
                } else {
                    ""
                }
            )
        } else {
            String::new()
        };
        let report = format!(
            "Listeners::of: {search:?} with {open} descriptors open in a table \
             of {table} after a burst of {burst}{limited}; asking every slot \
             takes {slots:?}"
        );
        eprintln!("{report}");
        // A table too small to dwarf the open descriptors cannot tell a
        // search of them from one of every slot, so only the finding above
        // is checked under so low a hard limit.
        if burst >= MIN_BURST {
            assert!(
                search < slots / 4,
                "the search asks the table's empty slots: {report}"
            );
        }
    }

    /// The smallest drained burst whose empty slots outnumber the open
    /// descriptors enough for the timing to tell the two searches apart.
    const MIN_BURST: u64 = 4096;

    /// How long finding a fiber's listeners takes in a helper as busy as
    /// one with a thousand fibers: thousands of descriptors, and thousands
    /// of established connections in the socket tables. Finding the newest
    /// listener must take well under the 35 ms a call that reading every
    /// descriptor and all of /proc/net/tcp took, as it does in about 2 ms,
    /// most of it listing the descriptors. So must finding the newest
    /// MPTCP listener, as Go makes, though it takes both searches of the
    /// descriptors (own_sockets). The numbers are printed: `cargo test
    /// ports::tests::listeners_of_under_load -- --nocapture`.
    #[test]
    fn listeners_of_under_load() {
        use std::net::TcpStream;
        use std::time::{Duration, Instant};
        const PAIRS: u64 = 2500;
        const ROUNDS: u32 = 9;
        let _full = FULL_TABLE.lock().unwrap_or_else(|e| e.into_inner());
        // The pairs need more descriptors than the soft limit many hosts
        // start a shell with (1024), which a process may raise up to the
        // hard limit itself. Where the hard limit is lower, the test holds
        // as many pairs as it allows, and says so.
        let spare = 256;
        let need = 2 * PAIRS + spare;
        let mut nofile = rustix::process::getrlimit(rustix::process::Resource::Nofile);
        let allowed = nofile.maximum.map_or(need, |hard| hard.min(need));
        if nofile.current.is_some_and(|soft| soft < allowed) {
            nofile.current = Some(allowed);
            rustix::process::setrlimit(rustix::process::Resource::Nofile, nofile)
                .unwrap_or_else(|e| panic!("raise RLIMIT_NOFILE to {allowed}: {e}"));
        }
        let pairs = (allowed.saturating_sub(spare) / 2) as usize;
        let limited = if pairs < PAIRS as usize {
            format!(
                " (RLIMIT_NOFILE's hard limit, {allowed}, allows {pairs} \
                 of the {PAIRS} pairs)"
            )
        } else {
            String::new()
        };
        // The median of several calls, so one slowed by a busy host does
        // not decide it.
        fn median(mut call: impl FnMut()) -> Duration {
            let mut took: Vec<Duration> = (0..ROUNDS)
                .map(|_| {
                    let start = Instant::now();
                    call();
                    start.elapsed()
                })
                .collect();
            took.sort();
            took[ROUNDS as usize / 2]
        }
        let time = |port: u16| median(|| assert!(Listeners::of(port).unwrap().is_some()));
        // An MPTCP listener, as Go's is, on 127.0.0.1; None where the host
        // has no MPTCP and LAMPYR_HELPER_NO_MPTCP=1 says so.
        let mptcp = || {
            let s = mptcp_socket(AddressFamily::INET)?;
            rustix::net::bind(&s, &std::net::SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0)).unwrap();
            rustix::net::listen(&s, 1).unwrap();
            let port = SocketAddr::try_from(rustix::net::getsockname(&s).unwrap())
                .unwrap()
                .port();
            Some((s, port))
        };
        let ours = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = ours.local_addr().unwrap().port();
        let ours_mptcp = mptcp();
        let quiet = time(port);
        // Each pair is two descriptors and two established sockets.
        let hub = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let held: Vec<_> = (0..pairs)
            .map(|_| {
                let client = TcpStream::connect(hub.local_addr().unwrap()).unwrap();
                (client, hub.accept().unwrap().0)
            })
            .collect();
        // The descriptors are searched from the top, so the listener opened
        // before the pairs is the slowest to find, and one opened after
        // them the quickest.
        let busy = time(port);
        let last = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let busy_last = time(last.local_addr().unwrap().port());
        let last_mptcp = mptcp();
        let mptcp_times = ours_mptcp
            .as_ref()
            .zip(last_mptcp.as_ref())
            .map(|(first, last)| (time(first.1), time(last.1)));
        let fallback = median(|| assert!(!proc_listeners(port).unwrap().is_empty()));
        let fds = std::fs::read_dir("/proc/self/fd").unwrap().count();
        let established = std::fs::read_to_string("/proc/self/net/tcp")
            .unwrap()
            .lines()
            .count();
        let report = format!(
            "Listeners::of: {quiet:?} quiet; with {fds} descriptors and \
             {established} lines in /proc/net/tcp{limited}, {busy:?} for the \
             first descriptor and {busy_last:?} for the last; the /proc \
             fallback's search {fallback:?}{}",
            mptcp_times.map_or(String::new(), |(first, last)| format!(
                "; an MPTCP listener {first:?} for the first descriptor and \
                 {last:?} for the last"
            ))
        );
        eprintln!("{report}");
        assert!(
            busy_last < Duration::from_millis(20),
            "finding the newest listener is as slow as reading every socket: {report}"
        );
        if let Some((_, last)) = mptcp_times {
            assert!(
                last < Duration::from_millis(20),
                "finding the newest MPTCP listener is as slow as reading every socket: {report}"
            );
        }
        drop(held);
    }
}
