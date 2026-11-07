// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! One fiber: a hyperlight-unikraft sandbox restored from the warm
//! snapshot (CLONE) or from a park (RESUME), owned by a thread that steps
//! the guest and takes fiberd's commands between steps.
//!
//! A fiber learns who it is before it can be reached: its fence and
//! generation are written to `/run/fiberd/incarnation` (JSON), a read-only
//! mount the guest gets on its restore, and only then is its endpoint
//! bound. The guest's random generators are reseeded by hyperlight-unikraft
//! itself on every restore, from host entropy, so fiberd's seed is checked
//! but not passed on, and never logged or written.
//!
//! The app serves on a port the helper picks, which the fiber's endpoint
//! reaches over loopback. A park records the port beside the snapshot,
//! since a resumed guest re-establishes its listener where it was; the
//! ports module keeps each port to one fiber and checks the listener is
//! the guest's before the fiber serves.

use std::fs::File;
use std::io::Write;
use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hyperlight_unikraft::hyperlight_host::hypervisor::InterruptHandle;
use hyperlight_unikraft::{AppSandbox, Exec, ListenPorts, Mount, SandboxBuilder, Snapshot, Yield};
use serde::{Deserialize, Serialize};

use crate::bindings::Bindings;
use crate::bridge::{Bridge, Splices};
use crate::nudge::Nudge;
use crate::ports::{Claim, Listeners, Ports};
use crate::restores::{Outcome, Ticket, Waited, Waker};

/// Where a guest finds its incarnation.
const INCARNATION_MOUNT: &str = "/run/fiberd";
/// What the helper writes beside a park's snapshot.
const PARK_NOTE: &str = "lampyr-helper.json";
/// How long one serving step may wait for the guest to become runnable.
/// The guest's timers and sockets end the wait early, and so does a
/// KILL's nudge (nudge.rs), so this is how often an idle fiber's thread
/// wakes for nothing, and how long a PARK waits for it at most, well
/// inside PARK_REACH. A KILL waits as long when its nudges cannot end the
/// wait: when the guest is not parked on a socket, or has no network, the
/// wait is a sleep, and when nudging is off there are none. Each wake
/// costs a W read, so a short one costs CPU for every idle fiber: at
/// 10ms, a thousand of them took about 1.7 CPUs.
const IDLE_STEP: Duration = Duration::from_millis(250);
/// How often KILL interrupts the guest and nudges its thread again, until
/// the fiber ends: an interrupt is dropped between entries, and a nudge
/// sent just before the thread waits is lost.
const AGAIN: Duration = Duration::from_millis(10);
/// How long a PARK waits for the fiber to take it. A fiber takes commands
/// between steps, and a step returns only once the guest blocks, so a
/// guest that spins never lets one in: the PARK is answered with an ERROR
/// then, well inside fiberd's own 15s, and the fiber keeps serving.
const PARK_REACH: Duration = Duration::from_secs(5);
/// How long fiberd's hyperlight backend waits for the answer to a PARK
/// (`parkTimeout` in pkg/backend/hyperlight) before it gives up on it.
const FIBERD_PARK_TIMEOUT: Duration = Duration::from_secs(15);
/// How long a PARK may take in all, from its arrival to its answer: just
/// inside FIBERD_PARK_TIMEOUT, so fiberd hears an ERROR rather than nothing.
/// A large guest's write can take most of it (a 300 MB Node.js guest takes
/// about 10s on a debug build). What was written after the ERROR is not a
/// park: a sync park's fiber serves again, an async park's ends.
const PARK_BUDGET: Duration = FIBERD_PARK_TIMEOUT.saturating_sub(Duration::from_secs(1));
/// How often a serving fiber may report a changed W. The guest moves it
/// (it is how far the guest's page allocator has gone), so a guest that
/// moves it on every step cannot flood fiberd's channel with W lines.
const W_EVERY: Duration = Duration::from_millis(100);

/// The control channel back to fiberd, shared by every fiber thread.
#[derive(Clone)]
pub struct Ctl(Arc<Mutex<File>>);

impl Ctl {
    pub fn new(file: File) -> Self {
        Ctl(Arc::new(Mutex::new(file)))
    }

    /// Send one line. Text inside it can come from the guest (an error's
    /// message, say), so a line break in it is replaced: a guest must not
    /// be able to write a line of the protocol.
    pub fn say(&self, line: impl AsRef<str>) {
        let line = one_line(line.as_ref());
        if let Ok(mut f) = self.0.lock() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

fn one_line(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

/// What a fiber is made from.
pub enum Origin {
    /// The warm snapshot, the command that starts the app on it, and the
    /// caller's payload for the guest.
    Clone {
        snapshot: Arc<Snapshot>,
        command: String,
        payload: Vec<u8>,
    },
    /// A park this helper wrote.
    Resume { dir: PathBuf },
}

pub struct Spec {
    pub fence: String,
    pub endpoint: PathBuf,
    pub deadline: Duration,
    /// When the deadline passes. It counts from the command's arrival, so
    /// a wait for a restore slot is part of it.
    pub until: Instant,
    pub generation: u64,
    pub origin: Origin,
    /// A directory of the fiber's own, for its incarnation mount.
    pub scratch: PathBuf,
    /// The ports every fiber of this helper holds.
    pub ports: Ports,
    /// What the template's service bindings give each fiber.
    pub bindings: Arc<Bindings>,
}

pub enum Cmd {
    Park { dir: PathBuf, sync: bool },
    Kill,
}

/// A command as the fiber's thread takes it.
enum Msg {
    Park {
        dir: PathBuf,
        sync: bool,
        answer: Arc<Answer>,
    },
    Kill,
}

/// What a fiber's thread and the control loop share.
#[derive(Default)]
struct Shared {
    /// Set, under its lock, while the fiber takes its last commands and
    /// says how it ended, so no command can arrive after that unanswered.
    ended: Mutex<bool>,
    /// Set by KILL, so a guest entry it breaks ends the fiber as killed.
    kill: AtomicBool,
    /// When the KILL came, so a restore it ends near its deadline counts
    /// as timed out.
    killed_at: OnceLock<Instant>,
    /// The guest's interrupt handle, from before its restore's resume entry.
    interrupt: Mutex<Option<Arc<dyn InterruptHandle>>>,
    /// Wakes the fiber's thread out of a serving step's wait, so a KILL is
    /// taken as it arrives. The interrupt handle cannot: it only stops a
    /// running vCPU.
    nudge: Nudge,
}

/// A fiber's thread, as the control loop holds it.
pub struct Fiber {
    commands: Sender<Msg>,
    thread: JoinHandle<()>,
    shared: Arc<Shared>,
    ctl: Ctl,
    fence: String,
    endpoint: PathBuf,
    /// Woken by a KILL, so a fiber waiting for a restore slot sees it.
    waker: Waker,
}

impl Fiber {
    /// Whether the fiber took the command; false once it has ended, and
    /// for a PARK once it is killed. A KILL after the first is taken and
    /// does nothing more: the fiber is already ending.
    pub fn send(&self, cmd: Cmd) -> bool {
        let ended = self.shared.ended.lock().unwrap_or_else(|e| e.into_inner());
        if *ended {
            return false;
        }
        match cmd {
            Cmd::Park { .. } if self.killing() => false,
            Cmd::Park { dir, sync } => {
                let answer = Arc::new(Answer::default());
                // The watchdog first: a PARK nothing watches could go
                // unanswered, so without one the fiber never sees it.
                if let Err(e) = watch_park(
                    self.ctl.clone(),
                    self.fence.clone(),
                    answer.clone(),
                    self.shared.clone(),
                    PARK_REACH,
                    PARK_BUDGET,
                ) {
                    let fence = &self.fence;
                    self.ctl
                        .say(format!("ERROR {fence} park: no thread to watch it: {e}"));
                    return true;
                }
                let msg = Msg::Park {
                    dir,
                    sync,
                    answer: answer.clone(),
                };
                if self.commands.send(msg).is_err() {
                    // Silences the watchdog: the caller answers this one.
                    answer.refuse();
                    return false;
                }
                // Not nudged (nudge.rs): the fiber takes it within a step.
                true
            }
            Cmd::Kill => {
                // Before the flag, so whoever sees the flag sees the time.
                let _ = self.shared.killed_at.set(Instant::now());
                if self.shared.kill.swap(true, Ordering::AcqRel) {
                    return true;
                }
                if self.commands.send(Msg::Kill).is_err() {
                    return false;
                }
                drop(ended);
                self.waker.wake();
                interrupt_until_ended(self.shared.clone());
                true
            }
        }
    }

    /// Whether fiberd has sent it KILL.
    pub fn killing(&self) -> bool {
        self.shared.kill.load(Ordering::Acquire)
    }

    /// Whether it has said how it ended. Its endpoint is gone by then, and
    /// it writes nothing more, so its fence and endpoint are free.
    pub fn has_ended(&self) -> bool {
        *self.shared.ended.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether its thread is done: it holds nothing, not even a directory.
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    pub fn endpoint(&self) -> &Path {
        &self.endpoint
    }
}

/// Who answers a PARK: the fiber, or the watchdog that gives up on it.
/// Exactly one does, once. The watchdog waits on `changed`, so it leaves
/// as soon as the fiber answers rather than when its time is up.
#[derive(Default)]
struct Answer {
    state: Mutex<u8>,
    changed: Condvar,
}

const WAITING: u8 = 0;
const PARKING: u8 = 1;
const ANSWERED: u8 = 2;

impl Answer {
    /// The fiber starts on the PARK; false when it was answered already.
    fn start(&self) -> bool {
        self.swap(WAITING, PARKING)
    }

    /// The fiber answers the PARK it started; false when the watchdog has.
    fn finish(&self) -> bool {
        self.swap(PARKING, ANSWERED)
    }

    /// Answer a PARK no one started; false when one did, or it is answered.
    fn refuse(&self) -> bool {
        self.swap(WAITING, ANSWERED)
    }

    fn swap(&self, from: u8, to: u8) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if *state != from {
            return false;
        }
        *state = to;
        self.changed.notify_all();
        true
    }

    /// Wait until `until`, or until the state is no longer `from`; the
    /// state then.
    fn wait_while(&self, from: u8, until: Instant) -> u8 {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        while *state == from {
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            state = self
                .changed
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        *state
    }
}

/// Answer a PARK with an ERROR if the fiber has not started it within
/// `reach`, or has not finished it within `budget` of its arrival. The
/// watchdog ends once the PARK is answered.
fn watch_park(
    ctl: Ctl,
    fence: String,
    answer: Arc<Answer>,
    shared: Arc<Shared>,
    reach: Duration,
    budget: Duration,
) -> std::io::Result<JoinHandle<()>> {
    watch_park_pausing(ctl, fence, answer, shared, reach, budget, Box::new(|| {}))
}

/// watch_park, calling `paused` between taking the answer and writing it,
/// for tests.
fn watch_park_pausing(
    ctl: Ctl,
    fence: String,
    answer: Arc<Answer>,
    shared: Arc<Shared>,
    reach: Duration,
    budget: Duration,
    paused: Box<dyn Fn() + Send>,
) -> std::io::Result<JoinHandle<()>> {
    let arrived = Instant::now();
    // The answer is taken and written under `ended`, which the fiber holds
    // to say how it ended: an ERROR never follows the fiber's EXITED, which
    // could also follow a new fiber on its fence. Once the fiber has ended,
    // it has answered every PARK it was sent, and the watchdog says nothing.
    // give_up says whether the PARK needs no more watching.
    let watched_answer = answer.clone();
    let give_up = move |take: fn(&Answer) -> bool, line: String| -> bool {
        let ended = shared.ended.lock().unwrap_or_else(|e| e.into_inner());
        if *ended {
            return true;
        }
        if !take(&answer) {
            return false;
        }
        paused();
        ctl.say(line);
        true
    };
    let watched = move || {
        let answer = watched_answer;
        if answer.wait_while(WAITING, arrived + reach) == WAITING
            && give_up(
                Answer::refuse,
                format!("ERROR {fence} park: the guest did not stop running within {reach:?}"),
            )
        {
            return;
        }
        if answer.wait_while(PARKING, arrived + budget) == PARKING {
            give_up(
                Answer::finish,
                format!("ERROR {fence} park: not written within {budget:?}"),
            );
        }
    };
    thread::Builder::new().spawn(watched)
}

/// A KILL is taken between steps too, so the fiber's thread is nudged out
/// of a step's wait (nudge.rs); but a guest that spins never ends its
/// step, or its restore's resume entry, so the guest is also interrupted
/// until the fiber ends. Again and again: an interrupt between two
/// entries does nothing, hyperlight drops one made before an entry's guest
/// call starts (so one made before a restore's resume entry has started
/// does nothing either), and one made while the vCPU is not running
/// returns false, though it still cancels the call once the vCPU runs. A
/// nudge sent just before the thread waits is lost too.
/// hyperlight-unikraft asks for interrupts until one lands or the restore
/// returns, and for none during a guest call that should run after it. A
/// killed fiber makes no call it needs: whatever it calls next is broken
/// off, and it ends. So interrupting until the fiber has ended keeps to
/// both. Without a thread for that, the guest is interrupted a few times
/// here instead, which ends a spinning guest unless it slips between
/// entries every time, or, as a CLONE or RESUME restores it, the
/// interrupts all come before its resume entry has started.
fn interrupt_until_ended(shared: Arc<Shared>) {
    let ours = shared.clone();
    let spawned = thread::Builder::new().spawn(move || {
        while interrupt(&shared) {
            thread::sleep(AGAIN);
        }
    });
    if spawned.is_err() {
        for _ in 0..INTERRUPTS_WITHOUT_A_THREAD {
            if !interrupt(&ours) {
                break;
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

/// How many times KILL interrupts the guest itself when it has no thread to.
const INTERRUPTS_WITHOUT_A_THREAD: usize = 5;

/// Interrupt the guest and nudge its thread, if the fiber has not ended;
/// whether it had not.
fn interrupt(shared: &Shared) -> bool {
    if *shared.ended.lock().unwrap_or_else(|e| e.into_inner()) {
        return false;
    }
    let handle = shared
        .interrupt
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(handle) = handle {
        handle.kill();
    }
    // After the interrupt: a nudge that lands while the vCPU runs then
    // finds the call cancelled, rather than one hyperlight runs again.
    shared.nudge.nudge();
    true
}

/// Start the fiber on its own thread, to restore once `ticket` gets a
/// restore slot. It answers CLONED, or an ERROR; and without a thread,
/// nothing, and the caller answers.
pub fn start(ctl: Ctl, spec: Spec, ticket: Ticket) -> std::io::Result<Fiber> {
    let (commands, rx) = channel();
    let shared = Arc::new(Shared::default());
    let theirs = shared.clone();
    let fence = spec.fence.clone();
    let endpoint = spec.endpoint.clone();
    let ours = ctl.clone();
    let waker = ticket.waker();
    let thread = thread::Builder::new().spawn(move || run(ctl, spec, ticket, rx, theirs))?;
    Ok(Fiber {
        commands,
        thread,
        shared,
        ctl: ours,
        fence,
        endpoint,
        waker,
    })
}

#[derive(Serialize)]
struct IncarnationFile<'a> {
    fence: &'a str,
    generation: u64,
}

#[derive(Serialize, Deserialize)]
struct ParkNote {
    port: u16,
    /// W when the park was written, which a resume of it starts from.
    dirtied: u64,
}

struct Live {
    sandbox: AppSandbox,
    /// The guest's port, held for as long as the guest lives.
    port: Claim,
    /// The guest's listeners as restore found them, until the first bind
    /// takes them: finding them is a large part of a birth under a burst,
    /// and once is enough.
    listeners: Option<Listeners>,
    /// Its open connections, whichever of its bridges took them.
    splices: Splices,
    /// What the fiber dirtied before its sandbox last began counting:
    /// hyperlight-unikraft counts from the sandbox's restore or its last
    /// snapshot, and W is what the fiber dirtied since the warm snapshot,
    /// so what the park it was resumed from recorded, and what was counted
    /// before a sync park of its own that failed after its snapshot, carry
    /// over.
    dirtied_before: u64,
    /// The last W it reported.
    said: Said,
}

impl Live {
    /// W: what the fiber has dirtied since the warm snapshot, that is
    /// hyperlight-unikraft's count since the sandbox was last restored or
    /// snapshotted, plus what carried over to it. The guest controls the
    /// count, so this is the guest's word for it; the grant's memory
    /// ceiling still bounds what the guest can use.
    fn dirtied(&self) -> Result<u64> {
        let now = self
            .sandbox
            .dirty_bytes()
            .context("read what the guest dirtied")?;
        Ok(self.dirtied_before.saturating_add(now))
    }

    /// Report W if it has changed, and its last report is W_EVERY old;
    /// returns how long the next step may wait: less than IDLE_STEP while
    /// a changed W is held back, so a guest that goes idle right after it
    /// moves W does not hold its last W for a whole idle step. The count is
    /// a word of the guest's scratch memory, so one that cannot be read
    /// means the sandbox is broken: the error ends the fiber, as at its
    /// birth, rather than leave fiberd an old W.
    fn report(&mut self, ctl: &Ctl, fence: &str) -> Result<Duration> {
        let w = self.dirtied()?;
        let now = Instant::now();
        if self.said.due(w, now) {
            ctl.say(format!("W {fence} {w}"));
        }
        Ok(self
            .said
            .held(w, now)
            .map_or(IDLE_STEP, |h| h.min(IDLE_STEP)))
    }
}

/// The last W a fiber reported, and when.
#[derive(Default)]
struct Said(Option<(u64, Instant)>);

impl Said {
    /// Whether `w` is to be reported at `now`, noting it if it is.
    fn due(&mut self, w: u64, now: Instant) -> bool {
        let due = match self.0 {
            None => true,
            Some((said, at)) => said != w && now.saturating_duration_since(at) >= W_EVERY,
        };
        if due {
            self.note(w, now);
        }
        due
    }

    /// How long until `w`, held back at `now`, is due; None when it is
    /// what was said.
    fn held(&self, w: u64, now: Instant) -> Option<Duration> {
        match self.0 {
            Some((said, at)) if said != w => {
                Some(W_EVERY.saturating_sub(now.saturating_duration_since(at)))
            }
            _ => None,
        }
    }

    /// Note `w` as said at `now`, as publish says it with CLONED.
    fn note(&mut self, w: u64, now: Instant) {
        self.0 = Some((w, now));
    }
}

fn run(ctl: Ctl, spec: Spec, ticket: Ticket, cmds: Receiver<Msg>, shared: Arc<Shared>) {
    let fence = spec.fence.clone();
    let scratch = spec.scratch.clone();
    // fiberd may give up on a fiber before it answers, and KILL it then:
    // such a fiber ends as it is, with an EXITED and never a CLONED, and
    // its endpoint never serves. Each step below is dropped, sandbox
    // included, before the EXITED, so fiberd reads the grant's memory with
    // it freed.
    let born = birth(&spec, ticket, &cmds, &shared, &ctl).and_then(|live| {
        let Some(mut live) = live else {
            return Ok(None);
        };
        let bridge = bind(&spec.endpoint, &mut live)?;
        // fiberd is owed a W after CLONED, and one that cannot be read
        // would not be honest, so the fiber fails instead.
        let w = live.dirtied()?;
        live.said.note(w, Instant::now());
        Ok(Some((live, bridge, w)))
    });
    let last = match born {
        Ok(Some((live, bridge, w))) => {
            if publish(&ctl, &fence, &cmds, &shared.ended, w) {
                let end = serve(&ctl, &fence, &spec.endpoint, live, bridge, &cmds, &shared);
                format!("EXITED {fence} {end}")
            } else {
                // The endpoint goes before the guest's port is released.
                drop(bridge);
                drop(live);
                format!("EXITED {fence} exit:137")
            }
        }
        Ok(None) => format!("EXITED {fence} exit:137"),
        // Restoring or starting a guest that spins is broken off by KILL.
        Err(_) if shared.kill.load(Ordering::Acquire) => format!("EXITED {fence} exit:137"),
        Err(e) => {
            eprintln!("helper: {fence}: {e:#}");
            format!("ERROR {fence} {e:#}")
        }
    };
    // The control loop sends nothing once this is set, and what it sent
    // before is answered here, so a PARK racing the fiber's end is never
    // left to fiberd's timeout.
    {
        let mut ended = shared.ended.lock().unwrap_or_else(|e| e.into_inner());
        *ended = true;
        while let Ok(cmd) = cmds.try_recv() {
            if let Msg::Park { answer, .. } = cmd
                && answer.refuse()
            {
                ctl.say(format!("ERROR {fence} park: the fiber has exited"));
            }
        }
        ctl.say(last);
    }
    let _ = std::fs::remove_dir_all(scratch);
}

/// Answer CLONED and the fiber's first W, unless fiberd has ended the fiber
/// first. The last check and the answer hold `ended`, which Fiber::send
/// takes to admit a command, so a KILL is either seen here or admitted
/// after CLONED is written.
fn publish(ctl: &Ctl, fence: &str, cmds: &Receiver<Msg>, ended: &Mutex<bool>, w: u64) -> bool {
    let _admission = ended.lock().unwrap_or_else(|e| e.into_inner());
    if killed(cmds, ctl, fence) {
        return false;
    }
    ctl.say(format!("CLONED {fence}"));
    ctl.say(format!("W {fence} {w}"));
    true
}

/// Whether fiberd has ended a fiber that has not answered yet. Only a KILL
/// (or the helper going away) should arrive for a fence before its CLONED,
/// since fiberd keeps one operation per fence outstanding; a PARK that does
/// is answered here, so it is never left unanswered once the fiber ends.
fn killed(cmds: &Receiver<Msg>, ctl: &Ctl, fence: &str) -> bool {
    loop {
        match cmds.try_recv() {
            Ok(Msg::Kill) | Err(TryRecvError::Disconnected) => return true,
            Ok(Msg::Park { answer, .. }) => {
                if answer.refuse() {
                    ctl.say(format!("ERROR {fence} park: the fiber is not serving yet"));
                }
            }
            Err(TryRecvError::Empty) => return false,
        }
    }
}

/// Wait for a restore slot, then restore. None when fiberd ends the fiber
/// first, waiting or restoring. The slot is held until the app listens,
/// since that is what keeps the CPU busy, and freed before the endpoint
/// is bound.
fn birth(
    spec: &Spec,
    ticket: Ticket,
    cmds: &Receiver<Msg>,
    shared: &Arc<Shared>,
    ctl: &Ctl,
) -> Result<Option<Live>> {
    let until = spec.until;
    // The wait asks under the queue's lock, so it asks only the flag a
    // KILL sets before waking it; what fiberd sent is answered after.
    let slot = match ticket.wait(until, || shared.kill.load(Ordering::Acquire)) {
        Waited::Slot(slot) => slot,
        Waited::Cancelled => {
            killed(cmds, ctl, &spec.fence);
            return Ok(None);
        }
        Waited::Refused(why) => bail!("{why}"),
    };
    let restored = restore(spec, until, cmds, shared, ctl);
    let killed_at = shared
        .kill
        .load(Ordering::Acquire)
        .then(|| shared.killed_at.get().copied())
        .flatten();
    slot.finish(outcome(
        matches!(restored, Ok(Some(_))),
        killed_at,
        Instant::now(),
        until,
        spec.deadline,
    ));
    restored
}

/// How near its deadline a KILL can come and still mean fiberd gave up
/// on the deadline: fiberd counts it from when it wrote the command, a
/// little before the helper read it, so its KILL lands just before
/// `until`. fiberd's own window for a probe that ran out its time.
const KILL_LATE: Duration = Duration::from_millis(250);

/// How a restore that held a slot ended: ready, failed, or timed out,
/// which is what feeds the estimate under overload. fiberd KILLs a fiber
/// at its own deadline, before the helper's, so a KILL that close to
/// `until` is the deadline passing, not a caller giving up. The window is
/// at most a quarter of the deadline, so on a short one an early KILL
/// still says nothing.
fn outcome(
    ready: bool,
    killed_at: Option<Instant>,
    now: Instant,
    until: Instant,
    deadline: Duration,
) -> Outcome {
    let late = KILL_LATE.min(deadline / 4);
    match killed_at {
        _ if ready => Outcome::Ready,
        Some(at) if at + late >= until => Outcome::TimedOut,
        Some(_) => Outcome::Failed,
        None if now >= until => Outcome::TimedOut,
        None => Outcome::Failed,
    }
}

/// Restore the sandbox, tell it who it is, start the app on a clone, and
/// wait until it listens, by `until`. None when fiberd ends the fiber
/// first.
fn restore(
    spec: &Spec,
    until: Instant,
    cmds: &Receiver<Msg>,
    shared: &Arc<Shared>,
    ctl: &Ctl,
) -> Result<Option<Live>> {
    let Some(policy) = spec
        .bindings
        .resolve(until, || shared.kill.load(Ordering::Acquire))?
    else {
        killed(cmds, ctl, &spec.fence);
        return Ok(None);
    };
    std::fs::create_dir_all(&spec.scratch)?;
    let incarnation = serde_json::to_vec(&IncarnationFile {
        fence: &spec.fence,
        generation: spec.generation,
    })?;
    std::fs::write(spec.scratch.join("incarnation"), incarnation)?;

    let (builder, claim, command, dirtied_before) = match &spec.origin {
        Origin::Clone {
            snapshot,
            command,
            payload,
        } => {
            // The payload is delivered to the guest as data, beside its
            // incarnation.
            if !payload.is_empty() {
                std::fs::write(spec.scratch.join("payload"), payload)?;
            }
            let claim = spec.ports.claim_fresh()?;
            let builder = SandboxBuilder::from_snapshot(snapshot.clone())
                .env("PORT", claim.port.to_string())
                .env("LAMPYR_FIBER_ID", spec.fence.as_str());
            let builder = spec.bindings.apply(builder, true, policy);
            (builder, claim, Some(command.clone()), 0)
        }
        Origin::Resume { dir } => {
            let note: ParkNote = serde_json::from_slice(
                &std::fs::read(dir.join(PARK_NOTE)).context("read the park's note")?,
            )
            .context("decode the park's note")?;
            let claim = spec.ports.claim(note.port)?;
            let builder =
                SandboxBuilder::from_snapshot_dir(dir.join("snapshot")).context("load the park")?;
            let builder = spec.bindings.apply(builder, false, policy);
            (builder, claim, None, note.dirtied)
        }
    };
    let port = claim.port;
    // What came before the boot may have outlasted the fiber: a KILL or
    // the deadline is answered now, not after a whole restore.
    if killed(cmds, ctl, &spec.fence) {
        return Ok(None);
    }
    if Instant::now() > until {
        bail!("the deadline passed before the sandbox restored");
    }
    // A restore ends with the guest's resume entry, which a guest that
    // spins never comes back from, so KILL gets the handle before it.
    let handed = shared.clone();
    // Unless the template has bindings (see bindings.rs), the guest may
    // listen on its port and reach nothing: hyperlight-unikraft gives
    // listen ports without a network policy an allow list that lists
    // nothing. Its endpoint is reached from the host, so it needs no
    // egress, and with none it cannot reach the other fibers' ports.
    let mut sandbox = builder
        .listen_ports(ListenPorts::from_ports([port]))
        .mount(Mount::ro(&spec.scratch, INCARNATION_MOUNT))
        // The guest's output goes to the helper's log and nowhere else:
        // nothing here drains a capture, so one would grow for as long as
        // the app logs.
        .echo_output(true)
        .output_limit(0)
        .on_interrupt_handle(move |handle| {
            *handed.interrupt.lock().unwrap_or_else(|e| e.into_inner()) = Some(handle);
        })
        .boot()
        .context("restore the sandbox")?;
    if let Some(command) = command {
        sandbox
            .submit(Exec::Guest(command))
            .context("start the app")?;
    }

    // Serving means the app's listener takes a connection. The guest's
    // listener is a host socket, so the handshake completes as soon as it
    // listens.
    let guest = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    loop {
        if killed(cmds, ctl, &spec.fence) {
            return Ok(None);
        }
        let listening = TcpStream::connect_timeout(&guest, Duration::from_millis(5)).is_ok();
        // fiberd has given up on a fiber past its deadline, so a late one
        // is an error, not a CLONED.
        if Instant::now() > until {
            bail!(
                "the app did not listen on port {port} within {:?}",
                spec.deadline
            );
        }
        if listening {
            // No other fiber holds the port, and no guest's ephemeral bind
            // can land in the range, so this process's listeners on it are
            // this guest's. A listener of any other process could take the
            // endpoint's connections, and is refused.
            let Some(listeners) = Listeners::of(port)? else {
                bail!("port {port} is served by another process, not this fiber's guest");
            };
            return Ok(Some(Live {
                sandbox,
                port: claim,
                listeners: Some(listeners),
                splices: Splices::default(),
                dirtied_before,
                said: Said::default(),
            }));
        }
        match sandbox
            .step(Duration::from_millis(2))
            .context("run the guest")?
        {
            Yield::Exited { status } => {
                bail!("the app exited with status {status} before it listened")
            }
            Yield::CallDone => bail!("the app returned before it listened"),
            Yield::CallFailed { status } => {
                bail!("the app failed with status {status} before it listened")
            }
            Yield::Blocked { .. } => {}
        }
    }
}

/// Step the guest and take commands until the fiber ends; returns how it
/// ended, as EXITED reports it.
fn serve(
    ctl: &Ctl,
    fence: &str,
    endpoint: &Path,
    mut live: Live,
    bridge: Bridge,
    cmds: &Receiver<Msg>,
    shared: &Shared,
) -> String {
    let mut bridge = Some(bridge);
    let mut wait = IDLE_STEP;
    loop {
        // Every command waiting, not one per step, so a PARK is taken
        // within a step of arriving; a KILL's nudge ends the step's wait.
        loop {
            match cmds.try_recv() {
                Err(TryRecvError::Empty) => break,
                Ok(Msg::Kill) | Err(TryRecvError::Disconnected) => {
                    // The endpoint goes before the guest's port is released.
                    drop(bridge.take());
                    return "exit:137".to_string();
                }
                // A PARK queued ahead of a KILL. The KILL's nudge may have
                // handed the guest an EINTR that a park would keep, so the
                // guest is never saved once it is killed (nudge.rs), and
                // the fiber ends as the KILL behind it would end it.
                Ok(Msg::Park { answer, .. }) if shared.kill.load(Ordering::Acquire) => {
                    if answer.refuse() {
                        ctl.say(format!("ERROR {fence} park: the fiber is being killed"));
                    }
                    drop(bridge.take());
                    return "exit:137".to_string();
                }
                // Already answered: the guest kept the fiber from it too long.
                Ok(Msg::Park { answer, .. }) if !answer.start() => {}
                Ok(Msg::Park { dir, sync, answer }) => {
                    // A park ends serving before the state is written.
                    drop(bridge.take());
                    eprintln!("helper: {fence} parking into {}", dir.display());
                    let parked_into = park(&mut live, &dir);
                    // False when the watchdog has answered ERROR already:
                    // whatever the write did, it is not a park, and the
                    // PARK gets no second answer.
                    let answering = answer.finish();
                    if !answering {
                        eprintln!("helper: {fence} park finished after its ERROR");
                    }
                    match parked_into {
                        Ok(bytes) if answering => {
                            ctl.say(format!("PARKED {fence} {bytes}"));
                            if !sync {
                                return "exit:0".to_string();
                            }
                            return parked(ctl, fence, cmds);
                        }
                        failed => {
                            // A failed write is answered with its error,
                            // unless the watchdog answered first.
                            let error = failed.err().map(|e| {
                                eprintln!("helper: {fence} park failed: {e:#}");
                                format!("ERROR {fence} park: {e:#}")
                            });
                            let error = error.filter(|_| answering);
                            // An async park's fiber ends, its sandbox freed
                            // before its EXITED.
                            if !sync {
                                if let Some(error) = error {
                                    ctl.say(error);
                                }
                                return "exit:0".to_string();
                            }
                            // A sync park keeps its fiber: it serves again,
                            // before fiberd hears the park failed.
                            let rebound = bind(endpoint, &mut live);
                            if let Some(error) = error {
                                ctl.say(error);
                            }
                            match rebound {
                                Ok(b) => bridge = Some(b),
                                Err(e) => {
                                    eprintln!("helper: {fence}: {e:#}");
                                    return "exit:1".to_string();
                                }
                            }
                        }
                    }
                }
            }
        }
        let stepped = {
            let _waiting = shared.nudge.waiting();
            live.sandbox.step(wait)
        };
        // W is read even after a step that only waited: one that entered
        // the guest can return the same Blocked, so the two cannot be told
        // apart, and an idle step is long enough for the read not to cost.
        let end = match stepped {
            Ok(Yield::Blocked { .. }) => match live.report(ctl, fence) {
                Ok(next) => {
                    wait = next;
                    continue;
                }
                Err(e) => {
                    eprintln!("helper: {fence}: {e:#}");
                    "exit:1".to_string()
                }
            },
            Ok(Yield::Exited { status }) | Ok(Yield::CallFailed { status }) => {
                format!("exit:{status}")
            }
            Ok(Yield::CallDone) => "exit:0".to_string(),
            // A KILL broke off a guest that would not stop running, which
            // is no error to log.
            Err(_) if shared.kill.load(Ordering::Acquire) => "exit:137".to_string(),
            Err(e) => {
                eprintln!("helper: {fence}: {e:#}");
                "exit:1".to_string()
            }
        };
        // A KILL ends the fiber however the guest ended. Its handle.kill()
        // can land just before an entry and be dropped, and its nudge then
        // hand the guest an EINTR it does not expect, which the guest can
        // exit or fail on before the next step looks for the KILL.
        let end = if shared.kill.load(Ordering::Acquire) {
            "exit:137".to_string()
        } else {
            end
        };
        // A PARK that arrives now is answered as the fiber ends (run).
        drop(bridge.take());
        return end;
    }
}

/// A sync park's fiber, alive and not serving, until KILL.
fn parked(ctl: &Ctl, fence: &str, cmds: &Receiver<Msg>) -> String {
    loop {
        match cmds.recv() {
            Ok(Msg::Kill) | Err(_) => return "exit:137".to_string(),
            Ok(Msg::Park { answer, .. }) => {
                if answer.refuse() {
                    ctl.say(format!("ERROR {fence} already parked"))
                }
            }
        }
    }
}

/// The fiber's endpoint, spliced to its guest's port. Every connection is
/// checked before and after it reaches the port: the guest's listening
/// sockets must be the ones it had, or, if it has made new ones, every
/// listener on the port must be this process's, and the record must not
/// change across the connect. Otherwise the connection is dropped.
fn bind(endpoint: &Path, live: &mut Live) -> Result<Bridge> {
    let port = live.port.port;
    let guest = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let check = guest_check(port, live.listeners.take())?;
    Bridge::bind(endpoint, guest, &live.splices, check).context("bind endpoint")
}

/// The bridge's check of the guest's listeners on `port`: the generation of
/// the record of them, which is new each time they change, or None when
/// they are not this process's alone. Checking an unchanged record costs
/// two system calls per listener, or three where sockets are told apart
/// by inode, and one more for each that takes the endpoint's connections
/// (Listeners); a changed one reads the socket tables.
///
/// `taken` is the record restore made, which starts the check while every
/// listener in it is still open: no other socket can have bound the port
/// since on an address the endpoint's connections reach (the guarantee the
/// check relies on between connections), so the record still holds. One
/// on another address, such as 127.0.0.2, would fail a new reading of the
/// tables but takes none of those connections. Otherwise they are read.
fn guest_check(
    port: u16,
    taken: Option<Listeners>,
) -> Result<impl Fn() -> Option<u64> + Send + Sync + 'static> {
    let known = match taken.filter(Listeners::still_open) {
        Some(known) => known,
        None => Listeners::of(port)?
            .context("the guest's listener is gone, or not this process's alone")?,
    };
    let record = Mutex::new((known, 0u64));
    Ok(move || {
        let mut record = record.lock().unwrap_or_else(|e| e.into_inner());
        if record.0.still_open() {
            return Some(record.1);
        }
        let now = Listeners::of(port).ok().flatten()?;
        *record = (now, record.1 + 1);
        Some(record.1)
    })
}

/// Write the sandbox, its port and its W into `dir`, which fiberd made
/// empty. Returns the bytes on disk.
fn park(live: &mut Live, dir: &Path) -> Result<u64> {
    std::fs::create_dir_all(dir)?;
    let counted = live
        .sandbox
        .dirty_bytes()
        .context("read what the guest dirtied")?;
    let dirtied = live.dirtied_before.saturating_add(counted);
    let snapshot = live.sandbox.snapshot_to(dir.join("snapshot"));
    // A snapshot restarts the count, even when saving it then fails, and a
    // sync park's fiber may serve again, so what was counted carries over.
    // Only a restart, or the guest, can lower the count, so a count below
    // what was read before is taken as restarted (a guest that lowers it
    // can only make W over-count); one that cannot be read is taken as
    // restarted too, so W may count some bytes twice but never drops what
    // was counted before the snapshot.
    // The snapshot may enter the guest first, for hyperlight's transport
    // checkpoint; what that entry writes before the restart is not counted.
    if !matches!(live.sandbox.dirty_bytes(), Ok(now) if now >= counted) {
        live.dirtied_before = dirtied;
    }
    snapshot.context("snapshot")?;
    std::fs::write(
        dir.join(PARK_NOTE),
        serde_json::to_vec(&ParkNote {
            port: live.port.port,
            dirtied,
        })?,
    )?;
    Ok(dir_bytes(dir))
}

fn dir_bytes(dir: &Path) -> u64 {
    let mut n = 0;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => n += dir_bytes(&e.path()),
                Ok(_) => n += e.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => {}
            }
        }
    }
    n
}

/// Fibers without a guest, for the control loop's tests.
#[cfg(test)]
pub mod testing {
    use super::*;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::atomic::AtomicUsize;

    /// Ends the fiber in two steps, as a real one does: `end` says EXITED,
    /// `finish` lets its thread return, as after it clears its directory.
    pub struct Stages {
        pub end: Sender<()>,
        pub finish: Sender<()>,
        /// The KILLs its thread took.
        pub kills: Arc<AtomicUsize>,
    }

    pub fn fiber(fence: &str, endpoint: &Path) -> (Fiber, Stages) {
        let (commands, rx) = channel();
        let shared = Arc::new(Shared::default());
        let (end, ending) = channel();
        let (finish, finishing) = channel();
        let kills = Arc::new(AtomicUsize::new(0));
        let (theirs, count) = (shared.clone(), kills.clone());
        let thread = thread::spawn(move || {
            let take = |m: Msg| {
                if matches!(m, Msg::Kill) {
                    count.fetch_add(1, Ordering::SeqCst);
                }
            };
            loop {
                if let Ok(m) = rx.recv_timeout(Duration::from_millis(1)) {
                    take(m);
                }
                if ending.try_recv().is_ok() {
                    break;
                }
            }
            *theirs.ended.lock().unwrap() = true;
            rx.try_iter().for_each(take);
            let _ = finishing.recv();
        });
        let (ctl, _) = UnixStream::pair().unwrap();
        let fiber = Fiber {
            commands,
            thread,
            shared,
            ctl: Ctl::new(File::from(OwnedFd::from(ctl))),
            fence: fence.to_string(),
            endpoint: endpoint.to_path_buf(),
            waker: Waker::nobody(),
        };
        (fiber, Stages { end, finish, kills })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::test_port;
    use crate::restores::Restores;
    use std::io::{ErrorKind, Read};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use tempfile::TempDir;

    /// A new listener on the port is a new record, so a connection made
    /// across the change is not approved.
    #[test]
    fn a_changed_listener_is_a_new_generation() {
        // A port of the default range, as it is bound again once free.
        let claim = test_port();
        let port = claim.port;
        let first = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        let check = guest_check(port, None).unwrap();
        let generation = check().unwrap();
        assert_eq!(check(), Some(generation));
        drop(first);
        let second = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        let changed = check().unwrap();
        assert_ne!(changed, generation);
        assert_eq!(check(), Some(changed));
        drop(second);
        assert_eq!(check(), None);
    }

    /// The record restore took starts the check while it is still open,
    /// and one that went stale since is not trusted: the tables are read.
    #[test]
    fn the_check_starts_from_restore_s_record_only_while_it_is_open() {
        // A port of the default range, as it is bound again once free.
        let claim = test_port();
        let port = claim.port;
        let first = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        // Nothing listens on `empty`, so only a check that started from
        // the record, which is another port's, can approve it.
        let empty_claim = test_port();
        let empty = empty_claim.port;
        let taken = Listeners::of(port).unwrap().unwrap();
        assert!(
            guest_check(empty, Some(taken)).unwrap()().is_some(),
            "the check read the tables, not the record"
        );

        let stale = Listeners::of(port).unwrap().unwrap();
        drop(first);
        assert!(
            guest_check(port, Some(stale)).is_err(),
            "a stale record approved a port nothing listens on"
        );

        let stale = {
            let gone = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            Listeners::of(gone.local_addr().unwrap().port())
                .unwrap()
                .unwrap()
        };
        let second = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).unwrap();
        let check = guest_check(port, Some(stale)).unwrap();
        // A check that started from the stale record would find it closed
        // and read the tables only now, as a second generation.
        assert_eq!(
            check(),
            Some(0),
            "the check started from the stale record, not the tables"
        );
        drop(second);
        assert_eq!(check(), None);
    }

    /// The first W is always said; after it, a changed one at most every
    /// W_EVERY, and an unchanged one not at all.
    #[test]
    fn w_is_said_when_it_changes_at_most_every_w_every() {
        let mut said = Said::default();
        let t = Instant::now();
        assert!(said.due(0, t));
        assert_eq!(said.held(0, t), None);
        assert!(!said.due(5, t + W_EVERY / 4), "W was said too soon");
        // The next step waits no longer than the held W.
        assert_eq!(said.held(5, t + W_EVERY / 4), Some(W_EVERY * 3 / 4));
        assert!(said.due(5, t + W_EVERY), "a changed W was not said");
        assert_eq!(said.held(5, t + W_EVERY), None);
        assert!(
            !said.due(5, t + W_EVERY * 3),
            "an unchanged W was said, long after"
        );
        // Down too, as the guest, which controls the count, can make it.
        assert!(said.due(3, t + W_EVERY * 3));
    }

    /// What the watchdog says for a PARK `run` handles one way or another,
    /// with `reach` 100ms and `budget` 400ms: every line within 800ms.
    fn watched(fiber: impl FnOnce(&Answer)) -> String {
        let (w, mut r) = UnixStream::pair().unwrap();
        let answer = Arc::new(Answer::default());
        let reach = Duration::from_millis(100);
        let budget = Duration::from_millis(400);
        watch_park(
            Ctl::new(File::from(OwnedFd::from(w))),
            "f".into(),
            answer.clone(),
            Arc::new(Shared::default()),
            reach,
            budget,
        )
        .unwrap();
        fiber(&answer);
        thread::sleep(Duration::from_millis(800));
        r.set_nonblocking(true).unwrap();
        let mut got = Vec::new();
        match r.read_to_end(&mut got) {
            // The watchdog may be done, and its end of the channel closed.
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {}
            other => panic!("{other:?}"),
        }
        String::from_utf8(got).unwrap()
    }

    /// A burst of parks leaves no watchdogs behind: each ends with its
    /// answer, not when its time would be up.
    #[test]
    fn a_park_answered_at_once_ends_its_watchdog() {
        let (w, _r) = UnixStream::pair().unwrap();
        let answer = Arc::new(Answer::default());
        let watchdog = watch_park(
            Ctl::new(File::from(OwnedFd::from(w))),
            "f".into(),
            answer.clone(),
            Arc::new(Shared::default()),
            Duration::from_secs(5),
            Duration::from_secs(14),
        )
        .unwrap();
        assert!(answer.start());
        assert!(answer.finish());
        let until = Instant::now() + Duration::from_secs(1);
        while !watchdog.is_finished() {
            assert!(Instant::now() < until, "the watchdog outlived its answer");
            thread::sleep(Duration::from_millis(1));
        }
        // Refused before it was started, too.
        let answer = Arc::new(Answer::default());
        let (w, _r) = UnixStream::pair().unwrap();
        let watchdog = watch_park(
            Ctl::new(File::from(OwnedFd::from(w))),
            "f".into(),
            answer.clone(),
            Arc::new(Shared::default()),
            Duration::from_secs(5),
            Duration::from_secs(14),
        )
        .unwrap();
        assert!(answer.refuse());
        let until = Instant::now() + Duration::from_secs(1);
        while !watchdog.is_finished() {
            assert!(Instant::now() < until, "the watchdog outlived its answer");
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// The fiber's end, as `run` says it: under `ended`.
    fn end(ctl: &Ctl, shared: &Shared) {
        let mut ended = shared.ended.lock().unwrap();
        *ended = true;
        ctl.say("EXITED f exit:0");
    }

    /// A watchdog that gave up on a PARK, held between taking the answer
    /// and writing it, while its async park's fiber ends: the ERROR still
    /// comes before the EXITED.
    #[test]
    fn a_watchdog_error_never_follows_its_fiber_s_exit() {
        let (w, mut r) = UnixStream::pair().unwrap();
        let ctl = Ctl::new(File::from(OwnedFd::from(w)));
        let shared = Arc::new(Shared::default());
        let answer = Arc::new(Answer::default());
        let (taken, at_pause) = channel();
        let (go, resume) = channel::<()>();
        let resume = Mutex::new(resume);
        let watchdog = watch_park_pausing(
            ctl.clone(),
            "f".into(),
            answer.clone(),
            shared.clone(),
            Duration::from_millis(10),
            Duration::from_millis(20),
            Box::new(move || {
                let _ = taken.send(());
                let _ = resume.lock().unwrap().recv();
            }),
        )
        .unwrap();
        assert!(answer.start());
        at_pause.recv_timeout(Duration::from_secs(5)).unwrap();
        // The write returns too late to answer, and the fiber ends.
        assert!(!answer.finish());
        let ending = {
            let (ctl, shared) = (ctl.clone(), shared.clone());
            thread::spawn(move || end(&ctl, &shared))
        };
        thread::sleep(Duration::from_millis(50));
        go.send(()).unwrap();
        ending.join().unwrap();
        watchdog.join().unwrap();
        drop(ctl);
        let mut said = String::new();
        r.read_to_string(&mut said).unwrap();
        assert_eq!(
            said,
            "ERROR f park: not written within 20ms\nEXITED f exit:0\n"
        );
    }

    /// Once its fiber has ended, the fiber has answered the PARK, and the
    /// watchdog says nothing.
    #[test]
    fn a_watchdog_says_nothing_once_its_fiber_has_ended() {
        let (w, mut r) = UnixStream::pair().unwrap();
        let ctl = Ctl::new(File::from(OwnedFd::from(w)));
        let shared = Arc::new(Shared::default());
        let answer = Arc::new(Answer::default());
        let watchdog = watch_park(
            ctl.clone(),
            "f".into(),
            answer.clone(),
            shared.clone(),
            Duration::from_millis(10),
            Duration::from_millis(20),
        )
        .unwrap();
        end(&ctl, &shared);
        watchdog.join().unwrap();
        drop(ctl);
        let mut said = String::new();
        r.read_to_string(&mut said).unwrap();
        assert_eq!(said, "EXITED f exit:0\n");
    }

    /// A PARK that reaches a fiber before it serves is answered as the
    /// fiber checks for a KILL, so ending it later leaves nothing unanswered.
    #[test]
    fn a_park_before_cloned_is_answered() {
        let (w, mut r) = UnixStream::pair().unwrap();
        let ctl = Ctl::new(File::from(OwnedFd::from(w)));
        let (tx, rx) = channel();
        let answer = Arc::new(Answer::default());
        tx.send(Msg::Park {
            dir: PathBuf::from("/park"),
            sync: true,
            answer: answer.clone(),
        })
        .unwrap();
        assert!(!killed(&rx, &ctl, "f"));
        // Answered once: the watchdog can no longer refuse it.
        assert!(!answer.refuse());
        tx.send(Msg::Kill).unwrap();
        assert!(killed(&rx, &ctl, "f"));
        drop(ctl);
        let mut said = String::new();
        r.read_to_string(&mut said).unwrap();
        assert_eq!(said, "ERROR f park: the fiber is not serving yet\n");
    }

    #[test]
    fn a_park_not_started_in_time_is_an_error_and_never_starts() {
        let said = watched(|answer| {
            thread::sleep(Duration::from_millis(250));
            assert!(!answer.start(), "a refused park started");
        });
        assert_eq!(
            said,
            "ERROR f park: the guest did not stop running within 100ms\n"
        );
    }

    #[test]
    fn a_park_finished_in_time_is_the_fiber_s_to_answer() {
        let said = watched(|answer| {
            assert!(answer.start());
            thread::sleep(Duration::from_millis(250));
            assert!(answer.finish());
        });
        assert_eq!(said, "");
    }

    /// The park's write outlives its budget: the watchdog answers, and the
    /// fiber, when the write returns, may not.
    #[test]
    fn a_park_not_written_in_time_is_an_error_and_never_parked() {
        let said = watched(|answer| {
            assert!(answer.start());
            thread::sleep(Duration::from_millis(600));
            assert!(!answer.finish(), "a park answered twice");
            assert!(!answer.refuse());
        });
        assert_eq!(said, "ERROR f park: not written within 400ms\n");
    }

    /// A RESUME of a park that is not there, queued behind a held restore
    /// slot, for the queue's tests: it fails at once if it ever restores.
    fn queued(restores: &Restores, deadline: Duration) -> (Fiber, UnixStream, TempDir) {
        let work = tempfile::tempdir().unwrap();
        let (w, r) = UnixStream::pair().unwrap();
        let until = Instant::now() + deadline;
        let ticket = restores.admit(until, false).unwrap();
        let spec = Spec {
            fence: "f".into(),
            endpoint: work.path().join("ep"),
            deadline,
            until,
            generation: 2,
            origin: Origin::Resume {
                dir: work.path().join("no-park"),
            },
            scratch: work.path().join("scratch"),
            ports: Ports::new(crate::ports::DEFAULT_RANGE).unwrap(),
            bindings: Default::default(),
        };
        let ctl = Ctl::new(File::from(OwnedFd::from(w)));
        let fiber = start(ctl, spec, ticket).unwrap();
        (fiber, r, work)
    }

    fn said_by(fiber: Fiber, mut r: UnixStream) -> String {
        fiber.thread.join().unwrap();
        drop(fiber.ctl);
        let mut said = String::new();
        r.read_to_string(&mut said).unwrap();
        said
    }

    /// What the fiber said by its end, which must come within seconds of
    /// a KILL. A queued fiber whose KILL never woke it ends the same way,
    /// only at its deadline: it looks at the KILL first when it times out.
    fn said_soon(fiber: Fiber, r: UnixStream) -> String {
        let (said, got) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let _ = said.send(said_by(fiber, r));
        });
        got.recv_timeout(Duration::from_secs(5))
            .expect("the KILL never woke the fiber")
    }

    /// KILL reaches a fiber still waiting for a restore slot: it leaves
    /// the queue and answers EXITED, never having restored.
    #[test]
    fn a_fiber_killed_while_queued_exits_and_leaves_the_queue() {
        let restores = Restores::new(1);
        let held = restores.admit(Instant::now(), true).unwrap();
        let (fiber, r, _work) = queued(&restores, Duration::from_secs(60));
        // Asleep, so the KILL must wake it rather than be seen by the
        // check made before it waits.
        restores.until_asleep(1);
        assert!(!fiber.has_ended(), "the fiber did not wait for a slot");
        assert!(fiber.send(Cmd::Kill));
        assert_eq!(said_soon(fiber, r), "EXITED f exit:137\n");
        // The queue is empty: the next restore takes the slot freed now.
        drop(held);
        let soon = Instant::now() + Duration::from_secs(1);
        let next = restores.admit(soon, false).unwrap();
        assert!(matches!(next.wait(soon, || false), Waited::Slot(_)));
    }

    /// A PARK that reaches a fiber waiting for a restore slot is answered
    /// once its KILL ends the wait, before the fiber says how it ended.
    #[test]
    fn a_park_sent_while_queued_is_answered_when_the_kill_ends_the_wait() {
        let restores = Restores::new(1);
        let _held = restores.admit(Instant::now(), true).unwrap();
        let (fiber, r, work) = queued(&restores, Duration::from_secs(60));
        restores.until_asleep(1);
        assert!(fiber.send(Cmd::Park {
            dir: work.path().join("park"),
            sync: false,
        }));
        assert!(fiber.send(Cmd::Kill));
        assert_eq!(
            said_soon(fiber, r),
            "ERROR f park: the fiber is not serving yet\nEXITED f exit:137\n"
        );
    }

    /// fiberd KILLs a fiber whose restore ran past its own deadline,
    /// a little before the helper's: that restore timed out, so the
    /// estimate learns from it. An early KILL, or a short deadline's,
    /// says nothing.
    #[test]
    fn a_kill_at_the_deadline_is_a_timeout() {
        let now = Instant::now();
        let second = Duration::from_secs(1);
        let ms = Duration::from_millis;
        let until = now + second;
        for (name, ready, killed_at, at, deadline, want) in [
            ("ready", true, Some(now), now, second, Outcome::Ready),
            (
                "killed at fiberd's deadline",
                false,
                Some(until - ms(20)),
                until - ms(10),
                second,
                Outcome::TimedOut,
            ),
            (
                "killed after it",
                false,
                Some(until),
                until,
                second,
                Outcome::TimedOut,
            ),
            (
                "killed early",
                false,
                Some(until - ms(500)),
                until - ms(490),
                second,
                Outcome::Failed,
            ),
            (
                "killed early in a short deadline",
                false,
                Some(until - ms(200)),
                until - ms(190),
                ms(500),
                Outcome::Failed,
            ),
            (
                "failed in time",
                false,
                None,
                until - ms(10),
                second,
                Outcome::Failed,
            ),
            (
                "failed past the deadline",
                false,
                None,
                until,
                second,
                Outcome::TimedOut,
            ),
        ] {
            assert_eq!(
                outcome(ready, killed_at, at, until, deadline),
                want,
                "{name}"
            );
        }
    }

    /// A fiber whose deadline passes in the queue fails its birth.
    #[test]
    fn a_fiber_whose_deadline_passes_while_queued_is_an_error() {
        let restores = Restores::new(1);
        let _held = restores.admit(Instant::now(), true).unwrap();
        let (fiber, r, _work) = queued(&restores, Duration::from_millis(30));
        let said = said_by(fiber, r);
        let want =
            "ERROR f restores-busy: the deadline passed while waiting for one of 1 restore slots";
        assert!(said.starts_with(want), "{said:?}");
    }

    /// fiberd may KILL a fiber at any moment before it answers. Whichever
    /// comes first, the KILL is seen and there is no CLONED, or CLONED is
    /// already written when the KILL is admitted.
    #[test]
    fn a_kill_is_seen_before_cloned_or_admitted_after_it() {
        for _ in 0..500 {
            let (w, mut r) = UnixStream::pair().unwrap();
            r.set_nonblocking(true).unwrap();
            let ctl = Ctl::new(File::from(OwnedFd::from(w)));
            let (commands, rx) = channel();
            let shared = Arc::new(Shared::default());
            let theirs = shared.clone();
            let ours = ctl.clone();
            let (result, published) = channel();
            let thread = thread::spawn(move || {
                // The commands stay open, as a fiber's do while it lives.
                let _ = result.send((publish(&ours, "f", &rx, &theirs.ended, 7), rx));
            });
            let fiber = Fiber {
                commands,
                thread,
                shared,
                ctl,
                fence: "f".to_string(),
                endpoint: PathBuf::new(),
                waker: Waker::nobody(),
            };
            assert!(fiber.send(Cmd::Kill));
            let mut buf = [0u8; 64];
            let seen = match r.read(&mut buf) {
                Ok(n) => String::from_utf8_lossy(&buf[..n]).into_owned(),
                Err(e) if e.kind() == ErrorKind::WouldBlock => String::new(),
                Err(e) => panic!("{e}"),
            };
            if published.recv().unwrap().0 {
                assert!(seen.starts_with("CLONED f\nW f 7\n"), "{seen:?}");
            } else {
                assert!(seen.is_empty(), "{seen:?}");
            }
            // Ends the KILL's interrupter, as the fiber's end would.
            *fiber.shared.ended.lock().unwrap() = true;
        }
    }
}
