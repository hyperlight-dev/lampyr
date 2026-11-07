// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! Waking a fiber's thread out of the wait in a guest step, so a KILL is
//! taken as soon as it arrives rather than when the step's timeout runs
//! out.
//!
//! hyperlight-unikraft's step waits in poll(2) on the guest's sockets and
//! has no way to end that wait early. poll is never restarted after a
//! signal handler, whatever SA_RESTART says, so a signal sent to the
//! waiting thread ends it, and the step returns the guest's last Blocked
//! without entering, as when its timeout runs out. One that lands while
//! the vCPU runs makes KVM_RUN return EINTR, which hyperlight takes for a
//! cancel nobody asked for, and it runs the vCPU again. The handler does
//! nothing and is installed with SA_RESTART, so most other system calls
//! carry on; but not every one. The guest's net_poll host call is a
//! poll(2) too, and one the signal lands in fails with EINTR, which the
//! guest's socket layer is handed and does not expect.
//!
//! So only KILL nudges, as it ends the guest: whatever the guest makes of
//! an EINTR is never run. A PARK keeps the guest, and the park would keep
//! that EINTR, so it is not nudged and waits out the step instead: one
//! IDLE_STEP at most, well inside PARK_REACH. Nor is one taken once a KILL
//! has come, even if it was queued first.
//!
//! A nudge sent just before the thread enters poll is lost, so KILL sends
//! one again and again until the fiber ends. When the guest is not parked
//! on a socket, or has no network, the step waits in a sleep, which a
//! signal does not end, so only the step's timeout bounds it.

use std::sync::{Mutex, OnceLock};

/// A thread that can be woken while it waits in a step.
#[derive(Default)]
pub struct Nudge {
    /// The thread, while it is in a step. It clears this before it leaves
    /// the step, and a nudge is sent under the lock, so the thread a nudge
    /// is sent to has not exited and its pthread_t is still its own.
    waiting: Mutex<Option<libc::pthread_t>>,
}

impl Nudge {
    /// Let a nudge wake the calling thread until the guard drops. Without
    /// the signal, nothing does, and the thread waits out its step.
    pub fn waiting(&self) -> Waiting<'_> {
        if signal().is_some() {
            // SAFETY: pthread_self has no preconditions.
            let me = unsafe { libc::pthread_self() };
            *self.lock() = Some(me);
        }
        Waiting(self)
    }

    /// Wake the thread, if it is waiting in a step.
    pub fn nudge(&self) {
        let Some(signal) = signal() else {
            return;
        };
        let waiting = self.lock();
        if let Some(thread) = *waiting {
            // SAFETY: the thread is alive while it is `waiting` (see the
            // field), and the signal's handler is installed.
            unsafe { libc::pthread_kill(thread, signal) };
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<libc::pthread_t>> {
        self.waiting.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// A thread waiting in a step, until it drops.
pub struct Waiting<'a>(&'a Nudge);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        *self.0.lock() = None;
    }
}

/// The signal a nudge sends: SIGRTMAX, as far from hyperlight's own
/// SIGRTMIN as there is. Set by `install`, once.
static SIGNAL: OnceLock<Option<libc::c_int>> = OnceLock::new();

/// Install the nudge's handler, unless something else in the process
/// handles the signal already, and unblock the signal; says once on
/// stderr when nudging is off, as every KILL then waits out a step.
/// Called from main before any thread starts, so it sees the disposition
/// the helper was started with rather than racing whatever a thread sets
/// up, no fiber's thread has to install it as it first waits, and every
/// thread inherits the unblocked mask.
pub fn install() {
    SIGNAL.get_or_init(|| {
        let signal = libc::SIGRTMAX();
        let installed = take_over(signal).and_then(unblock);
        if let Err(why) = &installed {
            eprintln!("helper: SIGRTMAX {why}; a KILL waits out its fiber's step");
        }
        installed.ok()
    });
}

/// Handle `signal` with `woken`, if nothing else does: one that is
/// ignored is free to take over, as SIG_IGN is inherited across exec from
/// whatever started the helper, and means nothing to it.
fn take_over(signal: libc::c_int) -> Result<libc::c_int, String> {
    // SAFETY: a zeroed sigaction is a valid one to fill in, and the
    // handler is async-signal-safe: it does nothing.
    unsafe {
        let mut old: libc::sigaction = std::mem::zeroed();
        if libc::sigaction(signal, std::ptr::null(), &mut old) != 0 {
            return Err(format!(
                "cannot be read: {}",
                std::io::Error::last_os_error()
            ));
        }
        if old.sa_sigaction != libc::SIG_DFL && old.sa_sigaction != libc::SIG_IGN {
            return Err("is handled by something else".to_string());
        }
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = woken as extern "C" fn(libc::c_int) as libc::sighandler_t;
        action.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut action.sa_mask);
        if libc::sigaction(signal, &action, std::ptr::null_mut()) != 0 {
            return Err(format!(
                "cannot be handled: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(signal)
}

/// Unblock `signal` in the calling thread. A blocked signal is inherited
/// across exec from whatever started the helper, and one sent to a thread
/// that blocks it only waits, pending, so no nudge would wake anything.
fn unblock(signal: libc::c_int) -> Result<libc::c_int, String> {
    // SAFETY: the set is initialised by sigemptyset before it is used.
    let failed = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, signal);
        libc::pthread_sigmask(libc::SIG_UNBLOCK, &set, std::ptr::null_mut())
    };
    if failed != 0 {
        // pthread_sigmask returns its error rather than setting errno.
        let e = std::io::Error::from_raw_os_error(failed);
        return Err(format!("cannot be unblocked: {e}"));
    }
    Ok(signal)
}

/// The signal a nudge sends, once its handler is installed.
fn signal() -> Option<libc::c_int> {
    SIGNAL.get().copied().flatten()
}

extern "C" fn woken(_: libc::c_int) {}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::event::{PollFd, PollFlags, Timespec, poll};
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// A thread in poll(2), as a step waits, is woken by a nudge, and is
    /// a nudge's target only while it waits.
    #[test]
    fn a_nudge_ends_a_wait_in_poll() {
        install();
        assert!(signal().is_some(), "the handler was not installed");
        let nudge = Arc::new(Nudge::default());
        // Nothing waits yet: a nudge signals no one, and must not fail.
        nudge.nudge();
        let theirs = nudge.clone();
        let waiter = thread::spawn(move || {
            let (quiet, _other) = UnixStream::pair().unwrap();
            let _waiting = theirs.waiting();
            let started = Instant::now();
            let mut fds = [PollFd::new(&quiet, PollFlags::IN)];
            let long = Timespec {
                tv_sec: 10,
                tv_nsec: 0,
            };
            let woke = poll(&mut fds, Some(&long));
            (woke, started.elapsed())
        });
        // Again and again, as a sender does, since one sent before the
        // thread is in poll is lost.
        while !waiter.is_finished() {
            nudge.nudge();
            thread::sleep(Duration::from_millis(10));
        }
        let (woke, took) = waiter.join().unwrap();
        assert_eq!(woke, Err(rustix::io::Errno::INTR));
        assert!(took < Duration::from_secs(2), "the wait took {took:?}");
        // The thread has left its step: no one is signalled now.
        assert!(nudge.lock().is_none());
        nudge.nudge();
    }

    /// A signal blocked in the mask the helper was started with is
    /// unblocked, or a nudge would only leave it pending.
    #[test]
    fn a_blocked_signal_is_unblocked() {
        let signal = libc::SIGRTMAX();
        // On a thread of its own, so the mask of no other test changes.
        let blocked = thread::spawn(move || {
            // SAFETY: each set is initialised by sigemptyset first.
            let is_blocked = || unsafe {
                let mut now: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut now);
                libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut now);
                libc::sigismember(&now, signal) == 1
            };
            // SAFETY: as above.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, signal);
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            }
            let before = is_blocked();
            assert_eq!(unblock(signal), Ok(signal));
            (before, is_blocked())
        });
        assert_eq!(blocked.join().unwrap(), (true, false));
    }
}
