// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! The TCP sockets listening on one port, as the kernel's sock_diag
//! netlink interface reports them.
//!
//! /proc/net/tcp lists every TCP socket of the network namespace, and in a
//! namespace without its own established table (net.ipv4.
//! tcp_child_ehash_entries=0, the default) reading it walks the host's
//! whole table, so its cost grows with every connection on the node. A
//! sock_diag dump asked for listeners alone walks the listening hash and
//! never the established one (inet_diag_dump_icsk). That hash is shared by
//! every network namespace on the host, and the walk still visits each
//! listener in it to compare its port, so its cost grows with the host's
//! listeners, a fiber's among them, though not with connections; only the
//! port's listeners are sent back. It needs no capability: only the extra
//! detail CAP_NET_ADMIN unlocks, which is not asked for.

use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::os::fd::OwnedFd;

use rustix::net::{
    AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType, netlink, recvfrom, send,
    socket_with,
};

// From linux/netlink.h, linux/sock_diag.h and linux/inet_diag.h.
const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_DUMP: u16 = 0x300;
/// Set on a dump whose tables changed between its datagrams, by the dumps
/// that track it (netlink_callback.seq). inet_diag does not, so a split
/// dump is caught by counting its datagrams instead (Dump::take); this is
/// only for a kernel that starts to.
const NLM_F_DUMP_INTR: u16 = 0x10;
const SOCK_DIAG_BY_FAMILY: u16 = 20;
const IPPROTO_TCP: u8 = 6;
const TCP_LISTEN: u8 = 10;
/// An attribute of a u8, whether an AF_INET6 socket is IPV6_V6ONLY.
/// inet_diag_msg_attrs_fill puts it on every AF_INET6 socket in LISTEN
/// (Linux 4.2 on) unasked: like INET_DIAG_PROTOCOL (10) before it, it
/// has no bit in idiag_ext, which only the first eight attributes do.
const INET_DIAG_SKV6ONLY: u16 = 11;
/// What an attribute's type is with NLA_F_NESTED and NLA_F_NET_BYTEORDER
/// taken off.
const NLA_TYPE_MASK: u16 = 0x3fff;
const NLA_HDRLEN: usize = 4;

const NLMSG_HDRLEN: usize = 16;
/// struct inet_diag_req_v2.
const REQ_LEN: usize = 56;
/// struct inet_diag_msg, up to and including idiag_inode.
const MSG_LEN: usize = 72;

/// The longest datagram sock_diag sends. The kernel fills one up to the
/// size the socket's last recv asked for, at most 32 KiB of messages
/// (netlink_recvmsg's cap on max_recvmsg_len), so a buffer this long
/// holds any of them. One cut short would lose listeners, so TRUNC has
/// recv say how long it was.
const DATAGRAM_MAX: usize = 32 * 1024;

/// A TCP socket listening on the port asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Listener {
    /// idiag_cookie, which is the socket's SO_COOKIE wherever SO_COOKIE
    /// can be read (Linux 4.12 on): 64 bits, never reused while the host
    /// is up, though before Linux 5.3 each network namespace counts its
    /// own, so a socket of another one can have the same.
    pub cookie: u64,
    /// idiag_inode, the socket's sockfs inode number.
    pub inode: u64,
    pub addr: IpAddr,
    /// idiag_if, the index of the device it is bound to (SO_BINDTODEVICE),
    /// or 0 for none.
    pub device: u32,
    /// INET_DIAG_SKV6ONLY: whether an IPv6 listener is IPV6_V6ONLY, so
    /// takes no IPv4 connection. For an MPTCP listener it is its subflow's,
    /// the TCP socket in the listening hash, which is the one that takes
    /// connections. None for an IPv4 listener, or where the kernel did not
    /// say.
    pub v6only: Option<bool>,
}

/// What a dump fails with when its sockets came in more than one datagram.
/// The kernel takes up again where it left off by counting, so a socket
/// that came or went in between can shift another out of the answer or
/// into it twice, and nothing marks it (see NLM_F_DUMP_INTR). A whole
/// datagram holds about 260 listeners, and a port with more is refused
/// rather than read another way: the /proc tables are read in pieces too.
#[derive(Debug)]
pub struct Split;

impl fmt::Display for Split {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("sock_diag answered in more than one datagram, so may have missed a listener")
    }
}

impl std::error::Error for Split {}

/// Whether `e` is a dump that came in pieces (Split).
pub fn is_split(e: &io::Error) -> bool {
    e.get_ref().is_some_and(|e| e.is::<Split>())
}

/// Every TCP socket listening on `port`, IPv4 and IPv6, in this process's
/// network namespace. An error when the kernel cannot be asked or answers
/// one; then nothing is known.
pub fn tcp_listeners(port: u16) -> io::Result<Vec<Listener>> {
    let sock = socket_with(
        AddressFamily::NETLINK,
        SocketType::DGRAM,
        SocketFlags::CLOEXEC,
        Some(netlink::SOCK_DIAG),
    )?;
    // On the stack, as every birth asks, and every dump uses this one.
    let mut buf = [0u8; DATAGRAM_MAX];
    let mut found = Vec::new();
    // One socket asks every dump: a dump is over once its NLMSG_DONE is
    // read, and the sequence numbers tell the answers apart.
    let mut seq = 0;
    for family in [AddressFamily::INET, AddressFamily::INET6] {
        let before = found.len();
        seq += 1;
        match dump(&sock, &mut buf, seq, family, port, &mut found) {
            // A dump's first datagram is filled as send() asks for it, and
            // on a socket that has not received yet that is only a page,
            // at most 8 KiB (NLMSG_GOODSIZE): 33 IPv4 listeners on the
            // port split the first dump with 4 KiB pages, about 70 with
            // larger ones. Asked again after its recvs, the dump starts
            // with a whole 32 KiB datagram.
            Err(e) if is_split(&e) => {
                found.truncate(before);
                seq += 1;
                dump(&sock, &mut buf, seq, family, port, &mut found)?;
            }
            done => done?,
        }
    }
    Ok(found)
}

/// `call`, again for as long as a signal interrupts it: a signal whose
/// handler was installed without SA_RESTART ends a blocking call with
/// EINTR, which says nothing about the dump.
fn uninterrupted<T>(mut call: impl FnMut() -> rustix::io::Result<T>) -> io::Result<T> {
    loop {
        match call() {
            Err(rustix::io::Errno::INTR) => {}
            done => return done.map_err(io::Error::from),
        }
    }
}

fn dump(
    sock: &OwnedFd,
    buf: &mut [u8],
    seq: u32,
    family: AddressFamily,
    port: u16,
    found: &mut Vec<Listener>,
) -> io::Result<()> {
    let mut req = [0u8; NLMSG_HDRLEN + REQ_LEN];
    req[0..4].copy_from_slice(&((NLMSG_HDRLEN + REQ_LEN) as u32).to_ne_bytes());
    req[4..6].copy_from_slice(&SOCK_DIAG_BY_FAMILY.to_ne_bytes());
    req[6..8].copy_from_slice(&(NLM_F_REQUEST | NLM_F_DUMP).to_ne_bytes());
    req[8..12].copy_from_slice(&seq.to_ne_bytes());
    let body = &mut req[NLMSG_HDRLEN..];
    body[0] = family.as_raw() as u8;
    body[1] = IPPROTO_TCP;
    body[4..8].copy_from_slice(&(1u32 << TCP_LISTEN).to_ne_bytes());
    // id.idiag_sport, in network order.
    body[8..10].copy_from_slice(&port.to_be_bytes());
    uninterrupted(|| send(sock, &req, SendFlags::empty()))?;

    let mut dump = Dump::new(seq, port, found);
    loop {
        let (n, whole, from) = uninterrupted(|| recvfrom(sock, &mut *buf, RecvFlags::TRUNC))?;
        // Only the kernel sends as port 0. A process with CAP_NET_ADMIN in
        // the namespace can send to this socket too, and so end the dump
        // early with a DONE of its own: its datagrams are passed over.
        let from = from
            .and_then(|a| netlink::SocketAddrNetlink::try_from(a).ok())
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "sock_diag's answer came from no netlink address",
                )
            })?;
        if from.pid() != 0 {
            continue;
        }
        if whole > n {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("sock_diag sent a datagram of {whole} bytes, over {n}"),
            ));
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sock_diag ended its dump without NLMSG_DONE",
            ));
        }
        if dump.take(&buf[..n])? {
            return Ok(());
        }
    }
}

/// One dump's answer, read a datagram at a time.
struct Dump<'a> {
    seq: u32,
    port: u16,
    found: &'a mut Vec<Listener>,
    /// Whether an earlier datagram held sockets. The last holds only
    /// NLMSG_DONE when the dump had any (netlink_dump sends what a dump
    /// call made before asking it again), so it is the second datagram of
    /// sockets that makes a dump split.
    had_sockets: bool,
    /// Whether it is split. Its datagrams are still read to its end, so
    /// the socket is left ready to ask it again.
    split: bool,
}

impl<'a> Dump<'a> {
    fn new(seq: u32, port: u16, found: &'a mut Vec<Listener>) -> Self {
        Dump {
            seq,
            port,
            found,
            had_sockets: false,
            split: false,
        }
    }

    /// Take the listeners in one datagram of the dump; true at its end.
    fn take(&mut self, mut data: &[u8]) -> io::Result<bool> {
        let bad =
            |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("sock_diag {what}"));
        let i32_at = |d: &[u8]| {
            d.get(0..4)
                .map(|e| i32::from_ne_bytes(e.try_into().unwrap()))
        };
        let mut sockets = false;
        while data.len() >= NLMSG_HDRLEN {
            let u16_at = |d: &[u8], i: usize| u16::from_ne_bytes([d[i], d[i + 1]]);
            let u32_at = |d: &[u8], i: usize| u32::from_ne_bytes(d[i..i + 4].try_into().unwrap());
            let len = u32_at(data, 0) as usize;
            if len < NLMSG_HDRLEN || len > data.len() {
                return Err(bad("sent a message of a bad length"));
            }
            let (kind, flags) = (u16_at(data, 4), u16_at(data, 6));
            if u32_at(data, 8) != self.seq {
                return Err(bad("answered another request"));
            }
            if flags & NLM_F_DUMP_INTR != 0 {
                return Err(bad("dump was interrupted"));
            }
            let msg = &data[NLMSG_HDRLEN..len];
            match kind {
                // The dump's own error, such as ENOENT from a kernel
                // without tcp_diag, comes in its NLMSG_DONE, which always
                // has room for one (netlink_dump_done).
                NLMSG_DONE => {
                    return match i32_at(msg) {
                        Some(e) if e < 0 => Err(io::Error::from_raw_os_error(e.saturating_neg())),
                        Some(_) if self.split => Err(io::Error::other(Split)),
                        Some(_) => Ok(true),
                        None => Err(bad("sent a short NLMSG_DONE")),
                    };
                }
                NLMSG_ERROR => match i32_at(msg) {
                    // An acknowledgement, which a dump does not ask for,
                    // and which ends nothing.
                    Some(0) => {}
                    Some(e) if e < 0 => {
                        return Err(io::Error::from_raw_os_error(e.saturating_neg()));
                    }
                    _ => return Err(bad("sent an error that is not one")),
                },
                SOCK_DIAG_BY_FAMILY if msg.len() >= MSG_LEN => {
                    if !sockets && self.had_sockets {
                        self.split = true;
                    }
                    sockets = true;
                    // The kernel filtered on both already; asked again, so
                    // a kernel that ignores the port filter is not trusted
                    // on it.
                    let sport = u16::from_be_bytes([msg[4], msg[5]]);
                    if msg[1] == TCP_LISTEN && sport == self.port {
                        let src = &msg[8..24];
                        let addr = match msg[0] as u32 {
                            f if f == AddressFamily::INET.as_raw() as u32 => {
                                IpAddr::V4(Ipv4Addr::new(src[0], src[1], src[2], src[3]))
                            }
                            f if f == AddressFamily::INET6.as_raw() as u32 => {
                                IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(src).unwrap()))
                            }
                            _ => return Err(bad("sent a socket of another family")),
                        };
                        let v6only = match attribute(&msg[MSG_LEN..], INET_DIAG_SKV6ONLY)
                            .map_err(|()| bad("sent an attribute of a bad length"))?
                        {
                            Some(&[only]) => Some(only != 0),
                            Some(_) => return Err(bad("sent an INET_DIAG_SKV6ONLY not of a u8")),
                            None => None,
                        };
                        self.found.push(Listener {
                            // idiag_cookie[0] is the low half
                            // (sock_diag_save_cookie).
                            cookie: u64::from(u32_at(msg, 44)) | u64::from(u32_at(msg, 48)) << 32,
                            // idiag_inode is 32 bits; so are sockfs inode
                            // numbers.
                            inode: u64::from(u32_at(msg, 68)),
                            addr,
                            device: u32_at(msg, 40),
                            v6only,
                        });
                    }
                }
                SOCK_DIAG_BY_FAMILY => return Err(bad("sent a short socket")),
                _ => {}
            }
            // Messages are padded to four bytes.
            data = data.get(len.next_multiple_of(4)..).unwrap_or_default();
        }
        self.had_sockets |= sockets;
        Ok(false)
    }
}

/// The payload of the attribute `kind` among a message's `attrs`, if it
/// has one; Err when one of them is cut short. Each is an nlattr, padded to
/// four bytes, as is the inet_diag_msg before them (72 bytes).
fn attribute(mut attrs: &[u8], kind: u16) -> Result<Option<&[u8]>, ()> {
    while attrs.len() >= NLA_HDRLEN {
        let len = usize::from(u16::from_ne_bytes([attrs[0], attrs[1]]));
        let ty = u16::from_ne_bytes([attrs[2], attrs[3]]) & NLA_TYPE_MASK;
        let payload = attrs.get(NLA_HDRLEN..len).ok_or(())?;
        if ty == kind {
            return Ok(Some(payload));
        }
        attrs = attrs.get(len.next_multiple_of(4)..).unwrap_or_default();
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    // Ports of the default range, not ephemeral ones: these tests assert on
    // everything listening there, and a test beside them could take any
    // ephemeral port.
    use crate::ports::test_port;
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::os::fd::AsFd;

    fn listener(s: impl AsFd, addr: IpAddr) -> Listener {
        Listener {
            cookie: rustix::net::sockopt::socket_cookie(&s).unwrap(),
            inode: rustix::fs::fstat(&s).unwrap().st_ino,
            addr,
            device: 0,
            v6only: addr
                .is_ipv6()
                .then(|| rustix::net::sockopt::ipv6_v6only(&s).unwrap()),
        }
    }

    #[test]
    fn listeners_on_a_port_are_found_in_both_families_and_nothing_else() {
        let claim = test_port();
        let port = claim.port;
        let v4 = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        let v6 = TcpListener::bind((Ipv6Addr::LOCALHOST, port)).ok();
        // A connection on the port is established, not listening.
        let _client = TcpStream::connect(SocketAddr::from((Ipv4Addr::LOCALHOST, port))).unwrap();
        let _other = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();

        let mut found = tcp_listeners(port).unwrap();
        found.sort();
        let mut want = vec![listener(&v4, IpAddr::V4(Ipv4Addr::LOCALHOST))];
        if let Some(v6) = &v6 {
            want.push(listener(v6, IpAddr::V6(Ipv6Addr::LOCALHOST)));
        }
        want.sort();
        assert_eq!(found, want);
        drop(v4);
        drop(v6);
        assert!(tcp_listeners(port).unwrap().is_empty());
    }

    /// More IPv4 listeners than the first datagram of a fresh socket's
    /// first dump holds (32 with 4 KiB pages, about 70 with larger ones),
    /// but fewer than a whole one does, on addresses of their own, so none
    /// needs SO_REUSEPORT: the dump is split, and asked again whole.
    #[test]
    fn a_port_with_more_listeners_than_a_first_datagram_holds_is_found_whole() {
        let claim = test_port();
        let port = claim.port;
        let held: Vec<_> = (2..102)
            .map(|i| TcpListener::bind((Ipv4Addr::new(127, 0, 0, i), port)).unwrap())
            .collect();
        // The first dump does split, or the retry goes untested.
        let fresh = socket_with(
            AddressFamily::NETLINK,
            SocketType::DGRAM,
            SocketFlags::CLOEXEC,
            Some(netlink::SOCK_DIAG),
        )
        .unwrap();
        let mut buf = [0u8; DATAGRAM_MAX];
        let first = dump(
            &fresh,
            &mut buf,
            1,
            AddressFamily::INET,
            port,
            &mut Vec::new(),
        );
        assert!(
            first.as_ref().is_err_and(is_split),
            "the first dump was not split: {first:?}"
        );
        let mut found = tcp_listeners(port).unwrap();
        found.sort();
        let mut want: Vec<_> = held
            .iter()
            .map(|s| listener(s, s.local_addr().unwrap().ip()))
            .collect();
        want.sort();
        assert_eq!(found, want);
    }

    /// A netlink message of `kind` with `body`, padded as the kernel pads.
    fn message(kind: u16, body: &[u8]) -> Vec<u8> {
        let mut m = Vec::new();
        m.extend_from_slice(&((NLMSG_HDRLEN + body.len()) as u32).to_ne_bytes());
        m.extend_from_slice(&kind.to_ne_bytes());
        m.extend_from_slice(&0u16.to_ne_bytes());
        m.extend_from_slice(&1u32.to_ne_bytes());
        m.extend_from_slice(&0u32.to_ne_bytes());
        m.extend_from_slice(body);
        m.resize(m.len().next_multiple_of(4), 0);
        m
    }

    /// A listener on 127.0.0.1:`port` as inet_diag reports it.
    fn socket(port: u16) -> Vec<u8> {
        let mut body = [0u8; MSG_LEN];
        body[0] = AddressFamily::INET.as_raw() as u8;
        body[1] = TCP_LISTEN;
        body[4..6].copy_from_slice(&port.to_be_bytes());
        body[8..12].copy_from_slice(&[127, 0, 0, 1]);
        message(SOCK_DIAG_BY_FAMILY, &body)
    }

    /// A listener on [::]:`port` as inet_diag reports it, with `attrs`
    /// after it, each a (type, payload).
    fn socket_v6(port: u16, attrs: &[(u16, &[u8])]) -> Vec<u8> {
        let mut body = vec![0u8; MSG_LEN];
        body[0] = AddressFamily::INET6.as_raw() as u8;
        body[1] = TCP_LISTEN;
        body[4..6].copy_from_slice(&port.to_be_bytes());
        for (kind, payload) in attrs {
            body.extend_from_slice(&((NLA_HDRLEN + payload.len()) as u16).to_ne_bytes());
            body.extend_from_slice(&kind.to_ne_bytes());
            body.extend_from_slice(payload);
            body.resize(body.len().next_multiple_of(4), 0);
        }
        message(SOCK_DIAG_BY_FAMILY, &body)
    }

    /// An IPv6 listener's IPV6_V6ONLY is the one INET_DIAG_SKV6ONLY says,
    /// found among its other attributes, and unknown where it has none.
    #[test]
    fn a_listener_s_v6only_is_read_from_its_attribute() {
        // What Linux 6.17 sends about a TCP listener: INET_DIAG_SHUTDOWN
        // before it, and INET_DIAG_CGROUP_ID (a u64) and
        // INET_DIAG_SOCKOPT (a u16, so padded) after it.
        let shutdown = (8, &[0u8][..]);
        let (cgroup, sockopt) = ((21, &[0u8; 8][..]), (22, &[0u8; 2][..]));
        let (no, yes) = (
            (INET_DIAG_SKV6ONLY, &[0][..]),
            (INET_DIAG_SKV6ONLY, &[1][..]),
        );
        for (attrs, want) in [
            (vec![shutdown, no, cgroup, sockopt], Some(false)),
            (vec![shutdown, yes, cgroup, sockopt], Some(true)),
            (vec![shutdown, cgroup, sockopt], None),
            (vec![], None),
        ] {
            let found = socket_dump(&socket_v6(80, &attrs));
            assert_eq!(found.len(), 1);
            assert_eq!(found[0].v6only, want, "{attrs:?}");
        }
        assert_eq!(socket_dump(&socket(80))[0].v6only, None);
        // One longer than its message, or not of a u8, is bad data.
        let mut short = socket_v6(80, &[(INET_DIAG_SKV6ONLY, &[0])]);
        short[NLMSG_HDRLEN + MSG_LEN..][..2].copy_from_slice(&64u16.to_ne_bytes());
        // So is one too short for its own header, which would otherwise
        // never move the walk on.
        let too_short = |len: u16| {
            let mut m = socket_v6(80, &[shutdown, (INET_DIAG_SKV6ONLY, &[0])]);
            m[NLMSG_HDRLEN + MSG_LEN..][..2].copy_from_slice(&len.to_ne_bytes());
            m
        };
        for bad in [
            short,
            socket_v6(80, &[(INET_DIAG_SKV6ONLY, &[0, 0])]),
            too_short(0),
            too_short(3),
        ] {
            let err = Dump::new(1, 80, &mut Vec::new()).take(&bad).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        }
    }

    /// The listeners in one datagram of sockets, then NLMSG_DONE.
    fn socket_dump(sockets: &[u8]) -> Vec<Listener> {
        let mut found = Vec::new();
        let mut dump = Dump::new(1, 80, &mut found);
        assert!(!dump.take(sockets).unwrap());
        assert!(dump.take(&done(0)).unwrap());
        found
    }

    /// A dual-stack listener on `::` is reported as one, and a v6-only one
    /// as v6-only, by the kernel itself.
    #[test]
    fn a_dual_stack_listener_is_reported_as_one() {
        for only in [false, true] {
            let s = rustix::net::socket(AddressFamily::INET6, SocketType::STREAM, None).unwrap();
            rustix::net::sockopt::set_ipv6_v6only(&s, only).unwrap();
            // Not an ephemeral port: a v6-only one leaves 127.0.0.1 free
            // on it, for another test's bind(0) to share it and find this
            // listener among its own.
            let claim = test_port();
            let any = std::net::SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, claim.port, 0, 0);
            rustix::net::bind(&s, &any).unwrap();
            rustix::net::listen(&s, 1).unwrap();
            let cookie = rustix::net::sockopt::socket_cookie(&s).unwrap();
            let found = tcp_listeners(claim.port).unwrap();
            let ours = found.iter().find(|l| l.cookie == cookie).unwrap();
            assert_eq!(ours.v6only, Some(only));
        }
    }

    fn done(errno: i32) -> Vec<u8> {
        message(NLMSG_DONE, &(-errno).to_ne_bytes())
    }

    fn error(errno: i32) -> Vec<u8> {
        message(NLMSG_ERROR, &(-errno).to_ne_bytes())
    }

    #[test]
    fn an_error_in_nlmsg_done_fails_the_dump() {
        let mut found = Vec::new();
        let err = Dump::new(1, 80, &mut found)
            .take(&done(rustix::io::Errno::NOENT.raw_os_error()))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound, "{err}");
        assert!(Dump::new(1, 80, &mut found).take(&done(0)).unwrap());
        // One without room for its error says nothing for the dump.
        let err = Dump::new(1, 80, &mut found)
            .take(&message(NLMSG_DONE, &[]))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
    }

    /// An acknowledgement (an NLMSG_ERROR of 0) is skipped, and a short
    /// NLMSG_ERROR or one of no error is bad data, not "os error 0".
    #[test]
    fn only_a_real_error_in_nlmsg_error_fails_the_dump() {
        let mut found = Vec::new();
        let mut dump = Dump::new(1, 80, &mut found);
        assert!(!dump.take(&[error(0), socket(80)].concat()).unwrap());
        assert!(dump.take(&done(0)).unwrap());
        assert_eq!(found.len(), 1);

        let err = Dump::new(1, 80, &mut found)
            .take(&error(rustix::io::Errno::PERM.raw_os_error()))
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied, "{err}");
        for bad in [message(NLMSG_ERROR, &[]), error(-1)] {
            let err = Dump::new(1, 80, &mut found).take(&bad).unwrap_err();
            assert_eq!(err.kind(), io::ErrorKind::InvalidData, "{err}");
        }
    }

    /// Sockets, then NLMSG_DONE alone, is one whole answer; sockets in two
    /// datagrams are a split one, which is read to its end before it fails.
    #[test]
    fn a_dump_with_sockets_in_two_datagrams_is_split() {
        let mut found = Vec::new();
        let mut dump = Dump::new(1, 80, &mut found);
        assert!(!dump.take(&[socket(80), socket(80)].concat()).unwrap());
        assert!(dump.take(&done(0)).unwrap());
        assert_eq!(found.len(), 2);

        let mut found = Vec::new();
        let mut dump = Dump::new(1, 80, &mut found);
        assert!(!dump.take(&socket(80)).unwrap());
        assert!(!dump.take(&socket(80)).unwrap());
        assert!(is_split(&dump.take(&done(0)).unwrap_err()));
    }
}
