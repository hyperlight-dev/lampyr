// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! How many fibers restore at once.
//!
//! A restore is CPU-bound from the sandbox's boot until its app listens,
//! and fiberd's hyperlight backend does not cap how many it asks for (only
//! its gVisor backend has a MaxRestores). An unbounded burst, 1000 cold
//! requests at once, starves the grant Pod: every restore slows, all of
//! them miss their deadlines, and fibers already serving get no CPU
//! either. So a CLONE or RESUME waits for one of a few slots, in arrival
//! order, and one whose deadline cannot cover the restores ahead of it is
//! refused at once with an ERROR, which fiberd takes as a failed birth,
//! rather than queued only to miss its deadline. Lampyr's own fiberd home
//! did the same until grant Pods moved to stock fiberd.
//!
//! Every such refusal starts with [`BUSY`]. fiberd passes the ERROR's text
//! on as the Clone's reason, and fiber-router matches the marker to treat
//! the refusal as backpressure, retried shortly, rather than as a broken
//! home.
//!
//! The wait is estimated from recent restores: a running average of how
//! long one takes, from its slot to its app listening.

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The default number of slots is two per CPU the helper may use, so a
/// restore's waits on its guest overlap another's work, and at most this
/// many, so a grant on a large node still leaves CPU for the fibers that
/// already serve.
const MAX_DEFAULT: usize = 16;

/// Starts every refusal for too many restores, as fiber-router matches
/// it. One word, since fiberd rebuilds the ERROR's text from its words.
pub const BUSY: &str = "restores-busy:";

/// How much each new restore time moves the estimate.
const ALPHA: f64 = 0.2;

/// How far one sample can move an estimate it starts: one slow first
/// restore should not make the helper refuse everything after it.
const FIRST_SAMPLE_CAP: Duration = Duration::from_secs(2);

/// The shortest timed-out restore that can start an estimate. A request
/// with a short deadline that timed out says little about a restore.
const TIMEOUT_SEED_FLOOR: Duration = Duration::from_secs(1);

/// Two slots per CPU the helper may use (its cgroup's quota included), at
/// most MAX_DEFAULT.
pub fn default_limit() -> usize {
    let cpus = std::thread::available_parallelism().map_or(1, |n| n.get());
    (2 * cpus).min(MAX_DEFAULT)
}

/// The helper's restore slots, shared by the control loop and every fiber.
#[derive(Clone)]
pub struct Restores(Arc<Inner>);

struct Inner {
    limit: usize,
    state: Mutex<State>,
}

/// Signalled, under the state's lock, when its waiter may take a slot (it
/// reached the head of the queue with a slot free) or must stop waiting
/// (it was KILLed, through its Waker). Each waiter has its own, so a freed
/// slot wakes the one waiter who can take it rather than every one of a
/// burst's thousand, which would each take the lock only to wait again.
type Turn = Arc<Condvar>;

#[derive(Default)]
struct State {
    next: u64,
    /// The waiting restores, in arrival order.
    queue: VecDeque<(u64, Turn)>,
    /// When each restore holding a slot took it.
    running: Vec<(u64, Instant)>,
    /// How long a restore takes, once one has said.
    estimate: Option<Duration>,
    /// How many waiters are asleep on their turn: a test that frees a slot
    /// before they are would see them take it without being woken.
    #[cfg(test)]
    asleep: usize,
}

/// How a restore that held a slot ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Its app listened: its time is a sample.
    Ready,
    /// Its deadline passed first: its time is a lower bound.
    TimedOut,
    /// It failed or was killed: its time says nothing.
    Failed,
}

/// What waiting for a slot came to.
pub enum Waited {
    Slot(Slot),
    /// KILLed while it waited.
    Cancelled,
    /// Its deadline cannot cover a restore any more.
    Refused(String),
}

impl Restores {
    pub fn new(limit: usize) -> Self {
        Restores(Arc::new(Inner {
            limit: limit.max(1),
            state: Mutex::default(),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.0.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Wait until `n` waiters are asleep on their turns. The count is kept
    /// under the queue's lock, which a waiter gives up only as it sleeps,
    /// so a slot freed or a KILL sent after this wakes them rather than
    /// being seen by a waiter that has yet to look.
    #[cfg(test)]
    pub fn until_asleep(&self, n: usize) {
        let by = Instant::now() + Duration::from_secs(5);
        while self.lock().asleep < n {
            assert!(Instant::now() < by, "{n} waiters never slept");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Queue a restore whose deadline is `until`, or say why it cannot
    /// expect a slot and its own restore by then. A helper with nothing
    /// restoring or waiting never refuses, so its first restore, or one
    /// after the estimate grew past every deadline, runs and refreshes the
    /// estimate.
    ///
    /// The probe, fiberd's first CLONE after READY, which it times to size
    /// its default deadlines, takes a slot at once even if none is free:
    /// a wait would make fiberd's deadlines too long, a refusal leave it
    /// none.
    pub fn admit(&self, until: Instant, probe: bool) -> Result<Ticket, String> {
        let mut s = self.lock();
        let id = s.next;
        s.next += 1;
        let idle = s.running.is_empty() && s.queue.is_empty();
        if probe {
            s.running.push((id, Instant::now()));
            return Ok(Ticket {
                restores: self.clone(),
                id,
                turn: Turn::default(),
                holding: true,
                idle: true,
            });
        }
        if let Some(estimate) = s.estimate.filter(|_| !idle) {
            let now = Instant::now();
            let started: Vec<Instant> = s.running.iter().map(|&(_, at)| at).collect();
            let wait = slot_wait(now, self.0.limit, &started, s.queue.len(), estimate);
            if until.saturating_duration_since(now) < wait + estimate {
                return Err(format!(
                    "{BUSY} {} restores running and {} queued, about {estimate:?} each: \
                     too many for the deadline",
                    s.running.len(),
                    s.queue.len()
                ));
            }
        }
        let turn = Turn::default();
        s.queue.push_back((id, turn.clone()));
        Ok(Ticket {
            restores: self.clone(),
            id,
            turn,
            holding: false,
            idle,
        })
    }

    fn release(&self, id: u64, outcome: Outcome) {
        let mut s = self.lock();
        if let Some(i) = s.running.iter().position(|&(held, _)| held == id) {
            let (_, started) = s.running.swap_remove(i);
            s.estimate = next_estimate(s.estimate, started.elapsed(), outcome);
        }
        s.wake_head(self.0.limit);
    }

    /// A waiter leaves the queue. Only the head's leaving lets another
    /// take a slot; one behind it changes nobody's turn.
    fn leave(&self, id: u64) {
        let mut s = self.lock();
        if let Some(i) = s.queue.iter().position(|(queued, _)| *queued == id) {
            s.queue.remove(i);
            if i == 0 {
                s.wake_head(self.0.limit);
            }
        }
    }
}

impl State {
    /// Wake the waiter at the head of the queue if a slot is free for it.
    /// Called whenever the head or the free slots change, so whoever can
    /// take a slot is always woken: a waiter that has not waited yet
    /// misses the signal, but looks before it waits.
    fn wake_head(&self, limit: usize) {
        if self.running.len() < limit
            && let Some((_, turn)) = self.queue.front()
        {
            turn.notify_one();
        }
    }
}

/// Wakes one waiter whatever its place, so a KILLed one stops waiting.
pub struct Waker {
    restores: Restores,
    turn: Turn,
}

impl Waker {
    pub fn wake(&self) {
        // Under the lock, so the waiter is either waiting and woken, or
        // yet to look at its cancel flag, which the KILL set before this.
        let _s = self.restores.lock();
        self.turn.notify_all();
    }

    /// One that wakes nobody, for fibers made without a queue.
    #[cfg(test)]
    pub fn nobody() -> Self {
        Waker {
            restores: Restores::new(1),
            turn: Turn::default(),
        }
    }
}

/// A restore's place in the queue, until it takes a slot. Dropped, it
/// leaves the queue.
pub struct Ticket {
    restores: Restores,
    id: u64,
    turn: Turn,
    /// Whether it took a slot at admission (the probe).
    holding: bool,
    /// Whether the helper was idle when it arrived: it is then never
    /// refused, even after its wait.
    idle: bool,
}

impl Ticket {
    /// What wakes this ticket's wait when it is KILLed.
    pub fn waker(&self) -> Waker {
        Waker {
            restores: self.restores.clone(),
            turn: self.turn.clone(),
        }
    }

    /// Wait for a slot until `until`. `cancelled` is asked under the
    /// queue's lock whenever the waiter wakes, so a KILL followed by
    /// Waker::wake is never missed; it must only read, as every
    /// restore's admission waits on that lock meanwhile.
    pub fn wait(mut self, until: Instant, mut cancelled: impl FnMut() -> bool) -> Waited {
        let restores = self.restores.clone();
        if self.holding {
            self.holding = false;
            return Waited::Slot(Slot {
                restores,
                id: self.id,
                outcome: Outcome::Failed,
            });
        }
        let limit = restores.0.limit;
        let mut s = restores.lock();
        loop {
            if cancelled() {
                return Waited::Cancelled;
            }
            let now = Instant::now();
            if s.queue.front().is_some_and(|(head, _)| *head == self.id) && s.running.len() < limit
            {
                s.queue.pop_front();
                // One that waited may no longer have time for a restore,
                // by the estimate the restores that finished meanwhile
                // left: the slot goes to the next in line.
                let left = until.saturating_duration_since(now);
                if let Some(estimate) = s.estimate.filter(|&e| !self.idle && left < e) {
                    s.wake_head(limit);
                    return Waited::Refused(format!(
                        "{BUSY} {left:?} left after the wait for a restore slot, \
                         and a restore takes about {estimate:?}"
                    ));
                }
                s.running.push((self.id, now));
                // Another slot may be free for the next in line.
                s.wake_head(limit);
                return Waited::Slot(Slot {
                    restores: restores.clone(),
                    id: self.id,
                    outcome: Outcome::Failed,
                });
            }
            if now >= until {
                return Waited::Refused(format!(
                    "{BUSY} the deadline passed while waiting for one of {limit} restore slots"
                ));
            }
            #[cfg(test)]
            {
                s.asleep += 1;
            }
            s = self
                .turn
                .wait_timeout(s, until - now)
                .unwrap_or_else(|e| e.into_inner())
                .0;
            #[cfg(test)]
            {
                s.asleep -= 1;
            }
        }
    }
}

impl Drop for Ticket {
    fn drop(&mut self) {
        if self.holding {
            self.restores.release(self.id, Outcome::Failed);
        } else {
            self.restores.leave(self.id);
        }
    }
}

/// A slot, held while the sandbox restores and its app starts. Dropped,
/// it frees, with the outcome last set.
pub struct Slot {
    restores: Restores,
    id: u64,
    outcome: Outcome,
}

impl Slot {
    pub fn finish(mut self, outcome: Outcome) {
        self.outcome = outcome;
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.restores.release(self.id, self.outcome);
    }
}

/// How long until a restore arriving now gets a slot: each running
/// restore frees its slot an estimate after it started, and each of the
/// `queued` ahead holds the next free slot for an estimate. The probe can
/// hold a slot over the limit; it then counts as one more slot, which
/// underestimates the wait by at most one restore, once, at warm.
fn slot_wait(
    now: Instant,
    limit: usize,
    started: &[Instant],
    queued: usize,
    estimate: Duration,
) -> Duration {
    let mut free: Vec<Instant> = started.iter().map(|&s| (s + estimate).max(now)).collect();
    free.resize(free.len().max(limit), now);
    for _ in 0..queued {
        let soonest = free.iter_mut().min().expect("at least one slot");
        *soonest += estimate;
    }
    let soonest = free.into_iter().min().expect("at least one slot");
    soonest - now
}

/// The estimate after a restore that took `took` ended with `outcome`. A
/// restore that timed out took at least `took`, so under overload the
/// estimate rises instead of staying at the speed of quieter times.
fn next_estimate(estimate: Option<Duration>, took: Duration, outcome: Outcome) -> Option<Duration> {
    match (estimate, outcome) {
        (_, Outcome::Failed) => estimate,
        (None, Outcome::Ready) => Some(took.min(FIRST_SAMPLE_CAP)),
        (None, Outcome::TimedOut) if took >= TIMEOUT_SEED_FLOOR => Some(took.min(FIRST_SAMPLE_CAP)),
        (None, Outcome::TimedOut) => None,
        (Some(e), Outcome::TimedOut) if took <= e => estimate,
        (Some(e), _) => {
            let step = ALPHA * (took.as_secs_f64() - e.as_secs_f64());
            Some(Duration::from_secs_f64((e.as_secs_f64() + step).max(0.0)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::thread;

    const SECOND: Duration = Duration::from_secs(1);

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(60)
    }

    fn take(r: &Restores) -> Slot {
        match r.admit(far(), false).unwrap().wait(far(), || false) {
            Waited::Slot(s) => s,
            _ => panic!("no slot"),
        }
    }

    fn with_estimate(r: &Restores, e: Duration) {
        r.lock().estimate = Some(e);
    }

    #[test]
    fn the_default_limit_is_two_per_cpu_and_bounded() {
        let got = default_limit();
        assert!((2..=MAX_DEFAULT).contains(&got), "{got}");
        assert!(got.is_multiple_of(2), "{got}");
    }

    #[test]
    fn restores_never_exceed_the_limit() {
        let r = Restores::new(2);
        let (running, peak) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let (r, running, peak) = (r.clone(), running.clone(), peak.clone());
                thread::spawn(move || {
                    let slot = take(&r);
                    let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    // The first holds its slot until a second restore runs
                    // beside it, so the limit is reached however the
                    // threads are scheduled.
                    let give_up = Instant::now() + Duration::from_secs(10);
                    while peak.load(Ordering::SeqCst) < 2 && Instant::now() < give_up {
                        thread::sleep(Duration::from_millis(1));
                    }
                    thread::sleep(Duration::from_millis(5));
                    running.fetch_sub(1, Ordering::SeqCst);
                    slot.finish(Outcome::Ready);
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert_eq!(peak.load(Ordering::SeqCst), 2);
        let s = r.lock();
        assert!(s.running.is_empty() && s.queue.is_empty());
        assert!(s.estimate.is_some());
    }

    #[test]
    fn the_queue_is_served_in_arrival_order() {
        let r = Restores::new(1);
        let held = take(&r);
        let first = r.admit(far(), false).unwrap();
        let second = r.admit(far(), false).unwrap();
        // The second waits first, and still does not overtake the first.
        let (got, slot) = std::sync::mpsc::channel();
        let waiter = thread::spawn(move || {
            let Waited::Slot(s) = second.wait(far(), || false) else {
                panic!("no slot");
            };
            got.send(()).unwrap();
            s
        });
        r.until_asleep(1);
        drop(held);
        thread::sleep(Duration::from_millis(20));
        assert!(slot.try_recv().is_err(), "the second overtook the first");
        let Waited::Slot(s) = first.wait(far(), || false) else {
            panic!("no slot");
        };
        drop(s);
        slot.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(waiter.join().unwrap());
    }

    #[test]
    fn a_failed_restore_frees_its_slot_and_leaves_the_estimate() {
        let r = Restores::new(1);
        drop(take(&r));
        take(&r).finish(Outcome::Failed);
        let s = r.lock();
        assert!(s.running.is_empty());
        assert_eq!(s.estimate, None);
    }

    #[test]
    fn a_restore_the_queue_ahead_cannot_finish_in_time_is_refused_at_once() {
        let r = Restores::new(2);
        with_estimate(&r, SECOND);
        // One running and three queued over two slots: about two seconds
        // for a slot, and one for the restore.
        let _running = take(&r);
        let queued: Vec<_> = (0..3).map(|_| r.admit(far(), false).unwrap()).collect();
        let soon = Instant::now() + Duration::from_millis(2500);
        let err = r.admit(soon, false).err().expect("admitted");
        assert!(err.starts_with(BUSY), "{err}");
        assert!(err.contains("1 restores running and 3 queued"), "{err}");
        assert_eq!(r.lock().queue.len(), 3, "a refused restore was queued");
        let later = Instant::now() + Duration::from_millis(3500);
        assert!(r.admit(later, false).is_ok());
        drop(queued);
    }

    #[test]
    fn running_restores_count_toward_the_wait() {
        let r = Restores::new(2);
        with_estimate(&r, SECOND);
        let _a = take(&r);
        let _b = take(&r);
        // A second for a slot, and a second for its own restore.
        assert!(
            r.admit(Instant::now() + Duration::from_millis(1500), false)
                .is_err()
        );
        assert!(
            r.admit(Instant::now() + Duration::from_millis(2500), false)
                .is_ok()
        );
    }

    #[test]
    fn an_idle_helper_never_refuses() {
        let r = Restores::new(4);
        // An estimate above every deadline, from one slow restore.
        with_estimate(&r, Duration::from_secs(3600));
        let soon = Instant::now() + SECOND;
        let Waited::Slot(slot) = r.admit(soon, false).unwrap().wait(soon, || false) else {
            panic!("an idle helper refused a restore");
        };
        slot.finish(Outcome::Ready);
        assert!(r.lock().estimate.unwrap() < Duration::from_secs(3600));
    }

    #[test]
    fn the_probe_takes_a_slot_at_once_even_when_none_is_free() {
        let r = Restores::new(1);
        let _held = take(&r);
        let _queued = r.admit(far(), false).unwrap();
        with_estimate(&r, Duration::from_secs(3600));
        let soon = Instant::now() + Duration::from_millis(100);
        assert!(r.admit(soon, false).is_err());
        let probe = r.admit(soon, true).expect("the probe was refused");
        let Waited::Slot(slot) = probe.wait(soon, || false) else {
            panic!("the probe waited");
        };
        assert_eq!(r.lock().running.len(), 2);
        drop(slot);
        assert_eq!(r.lock().running.len(), 1);
    }

    #[test]
    fn a_waiter_killed_in_the_queue_leaves_it() {
        let r = Restores::new(1);
        let held = take(&r);
        let killed = Arc::new(AtomicBool::new(false));
        let ahead = r.admit(far(), false).unwrap();
        let ticket = r.admit(far(), false).unwrap();
        let next = r.admit(far(), false).unwrap();
        // Behind the head, so only its own waker can wake it.
        let waker = ticket.waker();
        let (said, cancelled) = std::sync::mpsc::channel();
        let waiter = {
            let killed = killed.clone();
            thread::spawn(move || {
                let _ = said.send(matches!(
                    ticket.wait(far(), || killed.load(Ordering::SeqCst)),
                    Waited::Cancelled
                ));
            })
        };
        r.until_asleep(1);
        killed.store(true, Ordering::SeqCst);
        waker.wake();
        // Within seconds, not at the deadline: a waiter never woken also
        // ends Cancelled, as it looks at the flag first when it times out.
        let cancelled = cancelled
            .recv_timeout(Duration::from_secs(5))
            .expect("the KILL never woke the waiter");
        assert!(cancelled, "the KILL was not seen");
        waiter.join().unwrap();
        assert_eq!(r.lock().queue.len(), 2, "the killed waiter is still queued");
        // The next in line gets the slot it would have had.
        drop(held);
        drop(ahead);
        assert!(matches!(next.wait(far(), || false), Waited::Slot(_)));
    }

    /// Only the head is woken when a slot frees, so when the head is
    /// KILLed while it waits, the one behind it must be woken in its
    /// place, or it would sleep until its deadline with a slot free.
    #[test]
    fn a_cancelled_head_passes_its_turn_to_the_next() {
        let r = Restores::new(1);
        let held = take(&r);
        let killed = Arc::new(AtomicBool::new(false));
        let head = r.admit(far(), false).unwrap();
        let next = r.admit(far(), false).unwrap();
        let (got, slot) = std::sync::mpsc::channel();
        let behind = thread::spawn(move || {
            let Waited::Slot(s) = next.wait(far(), || false) else {
                panic!("no slot");
            };
            got.send(()).unwrap();
            s
        });
        let (said, cancelled) = std::sync::mpsc::channel();
        let leaving = {
            let killed = killed.clone();
            thread::spawn(move || {
                let _ = said.send(matches!(
                    head.wait(far(), || killed.load(Ordering::SeqCst)),
                    Waited::Cancelled
                ));
            })
        };
        r.until_asleep(2);
        // The slot frees and wakes the head alone, which finds itself
        // KILLed and leaves: the wake the slot gave is spent on it.
        killed.store(true, Ordering::SeqCst);
        drop(held);
        // Within seconds, not at the deadline, when it would be Cancelled
        // too without the slot's wake.
        let cancelled = cancelled
            .recv_timeout(Duration::from_secs(5))
            .expect("the freed slot never woke the head");
        assert!(cancelled, "the KILL was not seen");
        leaving.join().unwrap();
        slot.recv_timeout(Duration::from_secs(5))
            .expect("the next in line was never woken");
        drop(behind.join().unwrap());
    }

    /// A head woken for a slot that the estimate says it no longer has
    /// time for has left the queue already, so its ticket's drop wakes
    /// nobody: it must wake the one behind it as it refuses, or that one
    /// would sleep until its deadline with the slot free.
    #[test]
    fn a_refused_head_passes_its_turn_to_the_next() {
        let r = Restores::new(1);
        let held = take(&r);
        let soon = Instant::now() + Duration::from_secs(5);
        let head = r.admit(soon, false).unwrap();
        let next = r.admit(far(), false).unwrap();
        // Set only now, or both would be refused as they arrive: the head
        // has under 5s left, and the next well over 30s.
        with_estimate(&r, Duration::from_secs(30));
        let (got, slot) = std::sync::mpsc::channel();
        let behind = thread::spawn(move || {
            let _ = got.send(next.wait(far(), || false));
        });
        let (said, refusal) = std::sync::mpsc::channel();
        let refused = thread::spawn(move || {
            let _ = said.send(head.wait(soon, || false));
        });
        r.until_asleep(2);
        drop(held);
        // Well before the head's deadline, when it would be refused the
        // same way without the slot's wake.
        let Waited::Refused(why) = refusal
            .recv_timeout(Duration::from_secs(2))
            .expect("the freed slot never woke the head")
        else {
            panic!("the head was not refused");
        };
        refused.join().unwrap();
        assert!(why.contains("left after the wait"), "{why}");
        let Waited::Slot(s) = slot
            .recv_timeout(Duration::from_secs(5))
            .expect("the next in line was never woken")
        else {
            panic!("the next in line got no slot");
        };
        drop(s);
        behind.join().unwrap();
    }

    /// With several slots free, the head that takes one wakes the next, and
    /// so on down the queue, so a burst fills every free slot at once.
    #[test]
    fn every_free_slot_is_taken_when_several_free_at_once() {
        let r = Restores::new(3);
        let held: Vec<_> = (0..3).map(|_| take(&r)).collect();
        let tickets: Vec<_> = (0..3).map(|_| r.admit(far(), false).unwrap()).collect();
        let (got, slots) = std::sync::mpsc::channel();
        let waiters: Vec<_> = tickets
            .into_iter()
            .map(|t| {
                let got = got.clone();
                thread::spawn(move || {
                    let Waited::Slot(s) = t.wait(far(), || false) else {
                        panic!("no slot");
                    };
                    got.send(()).unwrap();
                    s
                })
            })
            .collect();
        r.until_asleep(3);
        // All three free under one lock: one wake, for the head only.
        {
            let mut s = r.lock();
            s.running.clear();
            s.wake_head(3);
        }
        for _ in 0..3 {
            slots
                .recv_timeout(Duration::from_secs(5))
                .expect("a free slot was left untaken");
        }
        for w in waiters {
            drop(w.join().unwrap());
        }
        drop(held);
    }

    #[test]
    fn a_waiter_past_its_deadline_is_refused_and_leaves_the_queue() {
        let r = Restores::new(1);
        let _held = take(&r);
        let soon = Instant::now() + Duration::from_millis(30);
        let ticket = r.admit(soon, false).unwrap();
        let Waited::Refused(why) = ticket.wait(soon, || false) else {
            panic!("not refused");
        };
        assert!(why.starts_with(BUSY), "{why}");
        assert!(why.contains("deadline passed"), "{why}");
        assert!(r.lock().queue.is_empty());
    }

    /// Every restore queued before the first one finished saw no estimate;
    /// the waiter uses the one set while it waited.
    #[test]
    fn a_waiter_uses_the_estimate_set_while_it_waited() {
        let r = Restores::new(1);
        let held = take(&r);
        let until = Instant::now() + Duration::from_millis(1500);
        let ticket = r.admit(until, false).unwrap();
        let releaser = {
            let r = r.clone();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(100));
                r.lock().estimate = Some(Duration::from_secs(2));
                drop(held);
            })
        };
        let Waited::Refused(why) = ticket.wait(until, || false) else {
            panic!("a waiter with too little time took the slot");
        };
        assert!(why.starts_with(BUSY), "{why}");
        assert!(why.contains("after the wait"), "{why}");
        releaser.join().unwrap();
        assert!(r.lock().running.is_empty());
    }

    #[test]
    fn slot_wait_counts_running_and_queued_restores() {
        let now = Instant::now();
        let ago = |d: Duration| now.checked_sub(d).unwrap();
        for (name, started, queued, want) in [
            ("a free slot", vec![], 0, Duration::ZERO),
            ("two just started", vec![now, now], 0, SECOND),
            ("one half done", vec![ago(SECOND / 2), now], 0, SECOND / 2),
            ("overdue", vec![ago(3 * SECOND), now], 0, Duration::ZERO),
            ("one queued ahead", vec![now, now], 1, SECOND),
            ("three queued ahead", vec![now, now], 3, 2 * SECOND),
            ("over the limit", vec![now, now, now], 0, SECOND),
        ] {
            assert_eq!(slot_wait(now, 2, &started, queued, SECOND), want, "{name}");
        }
    }

    #[test]
    fn the_estimate_follows_outcomes() {
        let minute = Duration::from_secs(60);
        // One slow first restore cannot set it past the cap.
        assert_eq!(
            next_estimate(None, minute, Outcome::Ready),
            Some(FIRST_SAMPLE_CAP)
        );
        // A short-deadline timeout cannot start one; a long one can.
        let short = Duration::from_millis(50);
        assert_eq!(next_estimate(None, short, Outcome::TimedOut), None);
        assert_eq!(
            next_estimate(None, 3 * SECOND, Outcome::TimedOut),
            Some(FIRST_SAMPLE_CAP)
        );
        // A failure says nothing.
        assert_eq!(
            next_estimate(Some(SECOND), short, Outcome::Failed),
            Some(SECOND)
        );
        // Timeouts longer than it raise it; shorter ones do not lower it.
        let raised = next_estimate(Some(SECOND), 10 * SECOND, Outcome::TimedOut).unwrap();
        assert!(raised > SECOND, "{raised:?}");
        assert_eq!(
            next_estimate(Some(SECOND), short, Outcome::TimedOut),
            Some(SECOND)
        );
        // A faster restore lowers it, by a fifth of the difference.
        let lowered = next_estimate(Some(SECOND), Duration::ZERO, Outcome::Ready).unwrap();
        assert_eq!(lowered, Duration::from_millis(800));
    }
}
