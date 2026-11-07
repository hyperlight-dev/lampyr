// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! A fiber's endpoint. fiberd hands every fiber a unix socket path, and a
//! hyperlight-unikraft guest serves on host TCP sockets, so the bridge
//! listens on the path and splices each connection to the guest's loopback
//! port.
//!
//! A spliced connection costs two threads and four descriptors for as long
//! as it is open, so a fiber splices at most MAX_SPLICES at once and closes
//! the connections beyond that as it accepts them. An established splice
//! has no idle timeout: streams and WebSockets may be quiet for as long as
//! the app likes, and fiber-router owns request timeouts.

use std::io;
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

/// How many connections one fiber splices at once. fiber-router sends a
/// Fiber one request at a time (containerConcurrency 1), so a fiber's
/// connections are that request, any stream or WebSocket it became, and
/// the router's idle keep-alives: a handful. This leaves room for many
/// times that, and bounds a flood at 128 threads per fiber.
pub const MAX_SPLICES: usize = 64;

/// How long the accept loop waits after accept fails, as when the process
/// is out of descriptors, so it does not spin while they are.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(10);

/// A bound endpoint. Dropping it stops taking connections and removes the
/// socket, so nothing new reaches the fiber; connections already spliced
/// end when either side closes. A fiber drops its bridge before it releases
/// its port, so a connection still being made when it does is dropped, not
/// spliced to whichever fiber takes the port next.
pub struct Bridge {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    /// The accept loop's listener, to shut down and so wake it.
    listener: UnixListener,
}

/// A fiber's open splices, counted across every bridge it binds: a sync
/// park that fails binds a new one while the old one's splices still run.
#[derive(Clone, Default)]
pub struct Splices(Arc<AtomicUsize>);

impl Bridge {
    /// Listen on `path` and splice each connection to `guest`, once
    /// `verify()` says the connection reached the guest's own listener
    /// (see `splice`),
    /// while the fiber has fewer than MAX_SPLICES `splices` open.
    pub fn bind(
        path: &Path,
        guest: SocketAddr,
        splices: &Splices,
        verify: impl Fn() -> Option<u64> + Send + Sync + 'static,
    ) -> io::Result<Self> {
        Self::bind_capped(path, guest, splices, MAX_SPLICES, verify)
    }

    fn bind_capped(
        path: &Path,
        guest: SocketAddr,
        splices: &Splices,
        cap: usize,
        verify: impl Fn() -> Option<u64> + Send + Sync + 'static,
    ) -> io::Result<Self> {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path)?;
        let ours = match listener.try_clone() {
            Ok(ours) => ours,
            Err(e) => {
                let _ = std::fs::remove_file(path);
                return Err(e);
            }
        };
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let verify: Verify = Arc::new(verify);
        let active = splices.0.clone();
        let accepting = thread::Builder::new().spawn(move || {
            // Whether the last connection was refused, so a flood is logged
            // once, not once per connection.
            let mut refusing = false;
            for conn in listener.incoming() {
                if stopped.load(Ordering::Acquire) {
                    break;
                }
                let conn = match conn {
                    Ok(conn) => conn,
                    Err(_) => {
                        thread::sleep(ACCEPT_BACKOFF);
                        continue;
                    }
                };
                // Explicitly shut down a refused connection so its client
                // observes EOF promptly, then drop it.
                let Some(slot) = Slot::take(&active, cap) else {
                    let _ = conn.shutdown(Shutdown::Both);
                    if !refusing {
                        eprintln!(
                            "helper: closing connections to port {}: {cap} are open",
                            guest.port()
                        );
                    }
                    refusing = true;
                    continue;
                };
                refusing = false;
                let verify = verify.clone();
                let stopped = stopped.clone();
                // Out of threads, the connection is closed and the loop
                // carries on: the next may find one.
                if let Err(e) = thread::Builder::new().spawn(move || {
                    let _slot = slot;
                    splice(conn, guest, &*verify, &stopped)
                }) {
                    eprintln!("helper: closing a connection to port {}: {e}", guest.port());
                }
            }
        });
        if let Err(e) = accepting {
            let _ = std::fs::remove_file(path);
            return Err(e);
        }
        Ok(Bridge {
            path: path.to_path_buf(),
            stop,
            listener: ours,
        })
    }
}

/// One of a fiber's MAX_SPLICES, held for as long as its connection is.
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(active: &Arc<AtomicUsize>, cap: usize) -> Option<Self> {
        active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < cap).then_some(n + 1)
            })
            .ok()
            .map(|_| Slot(active.clone()))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake the accept loop so it sees the flag: on Linux, shutting a
        // listening socket down fails a blocked accept. A connection wakes
        // it too, should that not, though one needs a free descriptor.
        let _ = rustix::net::shutdown(&self.listener, rustix::net::Shutdown::Read);
        let _ = UnixStream::connect(&self.path);
        let _ = std::fs::remove_file(&self.path);
    }
}

/// The record of the guest's listeners, as a generation that changes when
/// the record does: None when they are not the guest's alone.
type Verify = Arc<dyn Fn() -> Option<u64> + Send + Sync>;

fn splice(
    client: UnixStream,
    guest: SocketAddr,
    verify: &(dyn Fn() -> Option<u64> + Send + Sync),
    stopped: &AtomicBool,
) {
    // The listeners are checked before the connection is made and after,
    // and it is spliced only if the same ones were open both times. Then
    // the port was never free in between, and the connection reached one
    // of them. After alone is not enough: while the guest restarts its
    // listener, another process could take the port, accept and close the
    // connection, and the guest listen again before the check, which
    // would then approve the new listener for a connection made elsewhere.
    let dropped = || {
        eprintln!(
            "helper: dropped a connection: port {} is not this fiber's guest alone",
            guest.port()
        )
    };
    let Some(before) = verify() else {
        return dropped();
    };
    let Ok(upstream) = TcpStream::connect(guest) else {
        return;
    };
    if verify() != Some(before) {
        return dropped();
    }
    // Also after the connection is made: the fiber still held the port when
    // the bridge was still up, so a connection made before that is to this
    // fiber's guest. One made later may be to the port's next fiber, which
    // passes the check above as this process's own.
    if stopped.load(Ordering::Acquire) {
        return;
    }
    let _ = upstream.set_nodelay(true);
    let (Ok(mut client_r), Ok(mut upstream_w)) = (client.try_clone(), upstream.try_clone()) else {
        return;
    };
    let Ok(to_guest) = thread::Builder::new().spawn(move || {
        let _ = io::copy(&mut client_r, &mut upstream_w);
        let _ = upstream_w.shutdown(Shutdown::Write);
    }) else {
        eprintln!(
            "helper: closing a connection to port {}: no thread for it",
            guest.port()
        );
        return;
    };
    let (mut upstream_r, mut client_w) = (upstream, client);
    let _ = io::copy(&mut upstream_r, &mut client_w);
    // The guest's side is done, or gone with its fiber: close the client
    // both ways, so the copy toward the guest stops waiting on a client
    // that would otherwise hold the connection and its thread open.
    let _ = client_w.shutdown(Shutdown::Both);
    let _ = upstream_r.shutdown(Shutdown::Both);
    let _ = to_guest.join();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn read_to_eof(stream: &mut UnixStream, timeout: Duration) -> io::Result<Vec<u8>> {
        stream.set_nonblocking(true)?;
        let deadline = std::time::Instant::now() + timeout;
        let mut got = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => return Ok(got),
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if std::time::Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "connection did not close before the deadline",
                        ));
                    }
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[test]
    fn a_connection_reaches_the_guest_port_both_ways() {
        let guest = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = guest.local_addr().unwrap();
        thread::spawn(move || {
            let (mut s, _) = guest.accept().unwrap();
            let mut buf = [0u8; 4];
            s.read_exact(&mut buf).unwrap();
            s.write_all(&buf.map(|b| b.to_ascii_uppercase())).unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ep");
        let bridge = Bridge::bind(&path, addr, &Splices::default(), || Some(0)).unwrap();
        let mut c = UnixStream::connect(&path).unwrap();
        c.write_all(b"ping").unwrap();
        c.shutdown(Shutdown::Write).unwrap();
        let mut got = String::new();
        c.read_to_string(&mut got).unwrap();
        assert_eq!(got, "PING");
        drop(bridge);
        assert!(!path.exists());
        assert!(UnixStream::connect(&path).is_err());
    }

    #[test]
    fn a_connection_made_as_the_bridge_goes_away_is_dropped() {
        let guest = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = guest.local_addr().unwrap();
        // The port's next listener, which a stale connection must not reach.
        thread::spawn(move || {
            for s in guest.incoming().flatten() {
                let mut s = s;
                let _ = s.write_all(b"next");
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ep");
        let (entered, at_check) = std::sync::mpsc::channel();
        let (go, proceed) = std::sync::mpsc::channel::<()>();
        let proceed = std::sync::Mutex::new(proceed);
        // The check passes, as it would for another fiber of this helper,
        // but only once the bridge is gone.
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let bridge = Bridge::bind(&path, addr, &Splices::default(), move || {
            // The check after the connection is made.
            if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                let _ = entered.send(());
                let _ = proceed.lock().unwrap().recv();
            }
            Some(0)
        })
        .unwrap();
        let mut c = UnixStream::connect(&path).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        at_check.recv().unwrap();
        drop(bridge);
        go.send(()).unwrap();
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        assert!(
            got.is_empty(),
            "a stale connection reached the next listener"
        );
    }

    /// Connections past the cap are closed unspliced, and one is spliced
    /// again once an open one ends.
    #[test]
    fn a_fiber_splices_at_most_its_cap_at_once() {
        const CAP: usize = 3;
        let guest = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = guest.local_addr().unwrap();
        let (accepted, held) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for s in guest.incoming().flatten() {
                let _ = accepted.send(s);
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ep");
        let _bridge =
            Bridge::bind_capped(&path, addr, &Splices::default(), CAP, || Some(0)).unwrap();
        let wait = std::time::Duration::from_secs(5);
        let open = |path: &Path| {
            let c = UnixStream::connect(path).unwrap();
            c.set_read_timeout(Some(wait)).unwrap();
            c
        };
        let mut clients = Vec::new();
        let mut guests = Vec::new();
        for _ in 0..CAP {
            clients.push(open(&path));
            guests.push(held.recv_timeout(wait).unwrap());
        }
        // Past the cap: closed, and never reaches the guest.
        let mut refused = open(&path);
        let mut got = Vec::new();
        refused.read_to_end(&mut got).unwrap();
        assert!(got.is_empty());
        assert!(
            held.recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "a connection past the cap reached the guest"
        );
        // The guest ends one, which frees its slot.
        drop(guests.remove(0));
        let mut first = clients.remove(0);
        first.read_to_end(&mut got).unwrap();
        // Its client sees the end a moment before the slot is free.
        let until = std::time::Instant::now() + wait;
        let (mut again, mut guest_side) = loop {
            assert!(std::time::Instant::now() < until, "no slot came free");
            let again = open(&path);
            if let Ok(s) = held.recv_timeout(std::time::Duration::from_millis(50)) {
                break (again, s);
            }
        };
        guest_side.write_all(b"back").unwrap();
        let mut buf = [0u8; 4];
        again.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"back");
    }

    /// A fiber's next bridge, as a failed sync park binds, counts the
    /// splices its last one left running.
    #[test]
    fn a_fiber_s_cap_holds_across_its_bridges() {
        const CAP: usize = 2;
        let guest = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = guest.local_addr().unwrap();
        let (accepted, held) = std::sync::mpsc::channel();
        thread::spawn(move || {
            for s in guest.incoming().flatten() {
                let _ = accepted.send(s);
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ep");
        let wait = std::time::Duration::from_secs(5);
        let splices = Splices::default();
        let (entered, at_check) = std::sync::mpsc::channel();
        let (go, proceed) = std::sync::mpsc::channel::<()>();
        let proceed = std::sync::Mutex::new(proceed);
        let calls = AtomicUsize::new(0);
        let old = Bridge::bind_capped(&path, addr, &splices, CAP, move || {
            if calls.fetch_add(1, Ordering::SeqCst) == 1 {
                entered.send(()).unwrap();
                proceed.lock().unwrap().recv().unwrap();
            }
            Some(0)
        })
        .unwrap();
        let mut open = Vec::new();
        for i in 0..CAP {
            let c = UnixStream::connect(&path).unwrap();
            c.set_read_timeout(Some(wait)).unwrap();
            open.push((c, held.recv_timeout(wait).unwrap()));
            if i == 0 {
                // TCP accept can precede the bridge's final listener check.
                at_check.recv_timeout(wait).unwrap();
            }
        }
        go.send(()).unwrap();
        // Guest accept alone does not establish a splice: dropping the
        // bridge before its final check can still close it and free a slot.
        for (client, guest_side) in &mut open {
            guest_side.write_all(b"ready").unwrap();
            let mut ready = [0u8; 5];
            client.read_exact(&mut ready).unwrap();
            assert_eq!(&ready, b"ready");
        }
        drop(old);
        let _new = Bridge::bind_capped(&path, addr, &splices, CAP, || Some(0)).unwrap();
        let mut refused = UnixStream::connect(&path).unwrap();
        let got = read_to_eof(&mut refused, Duration::from_secs(30)).unwrap();
        assert!(got.is_empty());
        assert!(
            held.recv_timeout(std::time::Duration::from_millis(100))
                .is_err(),
            "the new bridge spliced past the fiber's cap"
        );
        // The old bridge's splices still carry their connections.
        let (mut client, mut guest_side) = open.remove(0);
        guest_side.write_all(b"live").unwrap();
        let mut buf = [0u8; 4];
        client.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"live");
    }

    /// The accept loop ends with its bridge even when no connection can
    /// wake it, as when the process is out of descriptors.
    #[test]
    fn dropping_a_bridge_ends_its_accept_loop_without_a_connection() {
        let guest = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = guest.local_addr().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ep");
        // The loop owns the check, so holds this until it ends.
        let token = Arc::new(());
        let held = token.clone();
        let bridge = Bridge::bind(&path, addr, &Splices::default(), move || {
            let _ = &held;
            Some(0)
        })
        .unwrap();
        // The path gone, the bridge's own connection cannot reach it.
        std::fs::remove_file(&path).unwrap();
        drop(bridge);
        let until = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while Arc::strong_count(&token) > 1 {
            assert!(
                std::time::Instant::now() < until,
                "the accept loop is still running"
            );
            thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    /// The guest's listeners changed while the connection was made: it may
    /// have reached another process's, and is dropped. Unchanged, it passes.
    #[test]
    fn a_connection_is_spliced_only_if_the_listeners_held_across_it() {
        let guest = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = guest.local_addr().unwrap();
        thread::spawn(move || {
            for s in guest.incoming().flatten() {
                let mut s = s;
                let _ = s.write_all(b"ours");
            }
        });
        let dir = tempfile::tempdir().unwrap();
        for (changed, want) in [(true, &b""[..]), (false, &b"ours"[..])] {
            let path = dir.path().join(format!("ep-{changed}"));
            let generation = std::sync::atomic::AtomicU64::new(1);
            let _bridge = Bridge::bind(&path, addr, &Splices::default(), move || {
                Some(if changed {
                    generation.fetch_add(1, Ordering::SeqCst)
                } else {
                    generation.load(Ordering::SeqCst)
                })
            })
            .unwrap();
            let mut c = UnixStream::connect(&path).unwrap();
            c.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut got = Vec::new();
            c.read_to_end(&mut got).unwrap();
            assert_eq!(got, want, "listeners changed: {changed}");
        }
    }

    #[test]
    fn a_connection_to_a_listener_that_fails_the_check_is_dropped() {
        let guest = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = guest.local_addr().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ep");
        let _bridge = Bridge::bind(&path, addr, &Splices::default(), || None).unwrap();
        let mut c = UnixStream::connect(&path).unwrap();
        c.set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut got = Vec::new();
        c.read_to_end(&mut got).unwrap();
        assert!(
            got.is_empty(),
            "the client got bytes from an unverified listener"
        );
    }
}
