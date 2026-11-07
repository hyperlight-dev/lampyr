// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! The helper driven as fiberd drives it: a socketpair on fd 3, one line
//! per command, against a real Lampyr template.
//!
//! A suite of its own, run by `just helper::test-kvm <template>
//! <incarnation-template>` rather than with the tests that need no
//! hypervisor. It needs KVM and two template directories `ly build` wrote:
//! a docs/examples app named by LAMPYR_HELPER_TEMPLATE, and the
//! tests/fixtures/incarnation handler (`just helper::incarnation-template`)
//! named by LAMPYR_HELPER_INCARNATION_TEMPLATE. It fails without them
//! rather than skipping:
//!
//!   LAMPYR_HELPER_TEMPLATE=<dir> LAMPYR_HELPER_INCARNATION_TEMPLATE=<dir> \
//!     cargo test --test protocol

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

const SEED: &str = "5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed5eed";
/// How long a test waits for the helper's next line.
const LINE_TIMEOUT: Duration = Duration::from_secs(60);

struct Helper {
    child: Child,
    ctl: UnixStream,
    lines: BufReader<UnixStream>,
    log: PathBuf,
    /// The highest W each fence has reported, whoever read the line.
    highest_w: BTreeMap<String, u64>,
    /// A W each fence's reports may not drop below while it is set,
    /// whoever reads them.
    w_floor: BTreeMap<String, u64>,
    _work: tempfile::TempDir,
}

impl Helper {
    fn start(template: &Path) -> Self {
        Self::start_with(template, &[])
    }

    /// The helper with flags of its own after `--template-dir`.
    fn start_with(template: &Path, flags: &[&str]) -> Self {
        let work = tempfile::tempdir().unwrap();
        let log = work.path().join("zygote.log");
        let (ours, theirs) = UnixStream::pair().unwrap();
        let fd = theirs.as_raw_fd();
        // One file for both, as fiberd points them at the grant's zygote.log.
        let logf = std::fs::File::create(&log).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_lampyr-hluk-helper"));
        command
            .arg("--template-dir")
            .arg(template)
            .args(flags)
            .current_dir(work.path())
            .stdout(Stdio::from(logf.try_clone().unwrap()))
            .stderr(Stdio::from(logf));
        // SAFETY: dup2 is async-signal-safe, and fd 3 is what the helper
        // reads as its control channel, as fiberd hands it over.
        unsafe {
            command.pre_exec(move || {
                // dup2 onto itself keeps close-on-exec, so clear it then.
                let ok = if fd == 3 {
                    libc_fcntl(3, F_SETFD, 0) >= 0
                } else {
                    libc_dup2(fd, 3) >= 0
                };
                if !ok {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn().unwrap();
        drop(theirs);
        ours.set_read_timeout(Some(LINE_TIMEOUT)).unwrap();
        let lines = BufReader::new(ours.try_clone().unwrap());
        Helper {
            child,
            ctl: ours,
            lines,
            log,
            highest_w: BTreeMap::new(),
            w_floor: BTreeMap::new(),
            _work: work,
        }
    }

    fn send(&mut self, line: &str) {
        writeln!(self.ctl, "{line}").unwrap();
    }

    fn line(&mut self) -> String {
        let mut line = String::new();
        self.lines.read_line(&mut line).unwrap();
        assert!(
            !line.is_empty(),
            "the helper closed its channel; log:\n{}",
            self.log()
        );
        let line = line.trim_end().to_string();
        self.track_w(&line);
        line
    }

    /// Note `line`'s W, if it is a W report, in `highest_w`, and check it
    /// against its fence's `w_floor`.
    fn track_w(&mut self, line: &str) {
        if let Some((fence, w)) = line.strip_prefix("W ").and_then(|w| w.split_once(' ')) {
            let w: u64 = w.parse().unwrap();
            if let Some(floor) = self.w_floor.get(fence) {
                assert!(w >= *floor, "W {fence} {w} is below its floor {floor}");
            }
            let highest = self.highest_w.entry(fence.to_string()).or_default();
            *highest = (*highest).max(w);
        }
    }

    /// The next line that is not a W report.
    fn reply(&mut self) -> String {
        loop {
            let line = self.line();
            if !line.starts_with("W ") {
                return line;
            }
        }
    }

    /// The fiber's W reports, read until one is at least `least`: that one.
    /// Anything else on the channel first, or a W below `floor`, fails the
    /// test.
    fn w_at_least(&mut self, fence: &str, floor: u64, least: u64) -> u64 {
        let prefix = format!("W {fence} ");
        loop {
            let line = self.line();
            let Some(w) = line.strip_prefix(&prefix) else {
                panic!("{line:?} before W {fence} reached {least}");
            };
            let w: u64 = w.parse().unwrap();
            assert!(w >= floor, "W {fence} {w} is below {floor}");
            if w >= least {
                return w;
            }
        }
    }

    /// The helper's next line, if it says one within `wait`.
    fn line_within(&mut self, wait: Duration) -> Option<String> {
        self.ctl.set_read_timeout(Some(wait)).unwrap();
        let mut line = String::new();
        let read = self.lines.read_line(&mut line);
        self.ctl.set_read_timeout(Some(LINE_TIMEOUT)).unwrap();
        match read {
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => None,
            Err(e) => panic!("{e}"),
            Ok(0) => panic!("the helper closed its channel; log:\n{}", self.log()),
            Ok(_) => {
                let line = line.trim_end().to_string();
                self.track_w(&line);
                Some(line)
            }
        }
    }

    fn log(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

const F_SETFD: i32 = 2;

unsafe extern "C" {
    #[link_name = "dup2"]
    fn libc_dup2(old: i32, new: i32) -> i32;
    #[link_name = "fcntl"]
    fn libc_fcntl(fd: i32, cmd: i32, arg: i32) -> i32;
}

fn get(endpoint: &Path, path: &str) -> String {
    let mut s = UnixStream::connect(endpoint).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: fiber\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut response = String::new();
    s.read_to_string(&mut response).unwrap();
    response
}

/// Ask the app for `path` and check it answered that request itself: the
/// docs/examples apps greet from Hyperlight and echo the path.
fn assert_served(endpoint: &Path, path: &str) {
    let response = get(endpoint, path);
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    assert!(response.contains("Hyperlight edge"), "{response}");
    assert!(response.contains(path), "{response}");
}

fn template() -> PathBuf {
    PathBuf::from(
        std::env::var("LAMPYR_HELPER_TEMPLATE")
            .expect("LAMPYR_HELPER_TEMPLATE names a ly build artifact"),
    )
}

/// What the incarnation fixture's guest reads from /run/fiberd.
fn incarnation_seen(endpoint: &Path) -> serde_json::Value {
    let response = get(endpoint, "/");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    // The body is one JSON object, chunked or not.
    let body = response
        .find('{')
        .zip(response.rfind('}'))
        .map(|(start, end)| &response[start..=end])
        .unwrap_or_else(|| panic!("no JSON body: {response}"));
    serde_json::from_str(body).unwrap_or_else(|e| panic!("{e}: {body}"))
}

/// The port a park recorded for its guest.
fn parked_port(park: &Path) -> u16 {
    let note: serde_json::Value =
        serde_json::from_slice(&std::fs::read(park.join("lampyr-helper.json")).unwrap()).unwrap();
    note["port"].as_u64().unwrap() as u16
}

#[test]
fn a_resume_never_serves_another_listener() {
    let runs = tempfile::tempdir().unwrap();
    let mut helper = Helper::start(&template());
    assert!(helper.line().starts_with("READY "));
    let a = runs.path().join("a");
    helper.send(&format!("CLONE g/1/1 {} 10000 - 1 {SEED}", a.display()));
    assert_eq!(helper.reply(), "CLONED g/1/1");

    // A sync park keeps the original, and its port, until KILL.
    let park = runs.path().join("park");
    std::fs::create_dir(&park).unwrap();
    helper.send(&format!("PARK g/1/1 {} 1", park.display()));
    assert!(helper.reply().starts_with("PARKED g/1/1 "));
    let b = runs.path().join("b");
    helper.send(&format!(
        "RESUME g/1/2 {} {} 10000 2 {SEED}",
        park.display(),
        b.display()
    ));
    let held = helper.reply();
    assert!(
        held.starts_with("ERROR g/1/2 ") && held.contains("another fiber"),
        "{held}"
    );
    helper.send("KILL g/1/1");
    assert_eq!(helper.reply(), "EXITED g/1/1 exit:137");

    // A port another process took is refused, not served.
    let port = parked_port(&park);
    let thief = std::net::TcpListener::bind(("0.0.0.0", port)).unwrap();
    helper.send(&format!(
        "RESUME g/1/3 {} {} 10000 2 {SEED}",
        park.display(),
        b.display()
    ));
    let taken = helper.reply();
    assert!(
        taken.starts_with("ERROR g/1/3 ") && taken.contains("taken"),
        "{taken}"
    );
    drop(thief);

    // Once the port is free the park resumes, and only once.
    helper.send(&format!(
        "RESUME g/1/4 {} {} 10000 2 {SEED}",
        park.display(),
        b.display()
    ));
    assert_eq!(helper.reply(), "CLONED g/1/4");
    assert_served(&b, "/once");
    let c = runs.path().join("c");
    helper.send(&format!(
        "RESUME g/1/5 {} {} 10000 2 {SEED}",
        park.display(),
        c.display()
    ));
    let twice = helper.reply();
    assert!(
        twice.starts_with("ERROR g/1/5 ") && twice.contains("already resumed"),
        "{twice}"
    );
    assert!(!c.exists(), "a refused resume bound its endpoint");
}

#[test]
fn a_fiber_killed_before_it_answers_never_serves() {
    let runs = tempfile::tempdir().unwrap();
    let mut helper = Helper::start(&template());
    assert!(helper.line().starts_with("READY "));
    // fiberd gives up on a fiber at its deadline and KILLs it, which can
    // reach the helper before the fiber has answered.
    let a = runs.path().join("a");
    helper.send(&format!(
        "CLONE g/2/1 {} 10000 - 1 {SEED}\nKILL g/2/1",
        a.display()
    ));
    assert_eq!(helper.line(), "EXITED g/2/1 exit:137");
    assert!(!a.exists(), "a killed fiber bound its endpoint");
    // The helper carries on.
    let b = runs.path().join("b");
    helper.send(&format!("CLONE g/2/2 {} 10000 - 1 {SEED}", b.display()));
    assert_eq!(helper.reply(), "CLONED g/2/2");
    assert_served(&b, "/after");
}

#[test]
fn clone_serve_park_resume_kill() {
    let template = template();
    let runs = tempfile::tempdir().unwrap();
    let mut helper = Helper::start(&template);

    let ready = helper.line();
    let version = Command::new(env!("CARGO_BIN_EXE_lampyr-hluk-helper"))
        .arg("--version")
        .output()
        .unwrap();
    // READY carries the facts alone: `--version`'s first line.
    let version = String::from_utf8(version.stdout).unwrap();
    assert_eq!(ready, format!("READY {}", version.lines().next().unwrap()));

    // Two clones serve side by side.
    let (a, b) = (runs.path().join("a"), runs.path().join("b"));
    helper.send(&format!("CLONE g/1/1 {} 10000 - 1 {SEED}", a.display()));
    assert_eq!(helper.line(), "CLONED g/1/1");
    // W is reported at least once after CLONED.
    assert!(helper.line().starts_with("W g/1/1 "));
    helper.send(&format!("CLONE g/1/2 {} 10000 - 1 {SEED}", b.display()));
    assert_eq!(helper.reply(), "CLONED g/1/2");
    assert_served(&a, "/first");
    assert_served(&b, "/second");

    // A park ends serving, and a resume serves again under a new fence.
    let park = runs.path().join("park");
    std::fs::create_dir(&park).unwrap();
    helper.send(&format!("PARK g/1/1 {} 0", park.display()));
    let parked = helper.reply();
    assert!(parked.starts_with("PARKED g/1/1 "), "{parked}");
    assert!(parked["PARKED g/1/1 ".len()..].parse::<u64>().unwrap() > 0);
    assert_eq!(helper.reply(), "EXITED g/1/1 exit:0");
    assert!(
        UnixStream::connect(&a).is_err(),
        "a parked fiber still serves"
    );

    let c = runs.path().join("c");
    helper.send(&format!(
        "RESUME g/1/3 {} {} 10000 2 {SEED}",
        park.display(),
        c.display()
    ));
    assert_eq!(helper.reply(), "CLONED g/1/3");
    assert_served(&c, "/resumed");

    helper.send("KILL g/1/2");
    assert_eq!(helper.reply(), "EXITED g/1/2 exit:137");
    helper.send("KILL g/1/3");
    assert_eq!(helper.reply(), "EXITED g/1/3 exit:137");

    // A command the helper cannot take is an ERROR for its fence.
    helper.send(&format!("PARK g/9/9 {} 1", park.display()));
    assert_eq!(helper.reply(), "ERROR g/9/9 unknown fiber");

    let log = helper.log();
    assert!(log.contains("seed:32B"), "{log}");
    assert!(!log.contains(SEED), "the seed reached the log");
}

/// A fiber reads who it is, and its payload, from a mount it cannot write;
/// and a resumed one reads its own incarnation, not its parked original's.
#[test]
fn a_fiber_reads_its_own_incarnation_and_payload() {
    let template =
        PathBuf::from(std::env::var("LAMPYR_HELPER_INCARNATION_TEMPLATE").expect(
            "LAMPYR_HELPER_INCARNATION_TEMPLATE names the tests/fixtures/incarnation build",
        ));
    let runs = tempfile::tempdir().unwrap();
    let mut helper = Helper::start(&template);
    assert!(helper.line().starts_with("READY "));

    let a = runs.path().join("a");
    let payload = "hello, fiber";
    let hex: String = payload.bytes().map(|b| format!("{b:02x}")).collect();
    helper.send(&format!("CLONE g/7/1 {} 60000 {hex} 1 {SEED}", a.display()));
    assert_eq!(helper.reply(), "CLONED g/7/1");
    let seen = incarnation_seen(&a);
    assert_eq!(
        seen["incarnation"],
        serde_json::json!({"fence": "g/7/1", "generation": 1}),
        "{seen}"
    );
    assert_eq!(seen["payload"], payload, "{seen}");
    assert_eq!(
        seen["writable"], false,
        "the guest wrote its incarnation mount"
    );

    let park = runs.path().join("park");
    std::fs::create_dir(&park).unwrap();
    helper.send(&format!("PARK g/7/1 {} 0", park.display()));
    assert!(helper.reply().starts_with("PARKED g/7/1 "));
    assert_eq!(helper.reply(), "EXITED g/7/1 exit:0");

    let b = runs.path().join("b");
    helper.send(&format!(
        "RESUME g/7/2 {} {} 60000 2 {SEED}",
        park.display(),
        b.display()
    ));
    assert_eq!(helper.reply(), "CLONED g/7/2");
    let seen = incarnation_seen(&b);
    assert_eq!(
        seen["incarnation"],
        serde_json::json!({"fence": "g/7/2", "generation": 2}),
        "the resumed guest reads another incarnation: {seen}"
    );
    // A resume carries no payload, and its mount is its own.
    assert_eq!(seen["payload"], serde_json::Value::Null, "{seen}");
    assert_eq!(
        seen["writable"], false,
        "the guest wrote its incarnation mount"
    );

    helper.send("KILL g/7/2");
    assert_eq!(helper.reply(), "EXITED g/7/2 exit:137");
}

/// W is what the fiber dirtied since the warm snapshot, from
/// hyperlight-unikraft's count: it grows as the app starts and as the
/// guest writes; a sync park that fails after its snapshot keeps what was
/// counted before it, and a resume starts from what its park recorded.
#[test]
fn w_is_what_the_fiber_dirtied() {
    let template =
        PathBuf::from(std::env::var("LAMPYR_HELPER_INCARNATION_TEMPLATE").expect(
            "LAMPYR_HELPER_INCARNATION_TEMPLATE names the tests/fixtures/incarnation build",
        ));
    let runs = tempfile::tempdir().unwrap();
    let mut helper = Helper::start(&template);
    assert!(helper.line().starts_with("READY "));

    let a = runs.path().join("a");
    helper.send(&format!("CLONE g/8/1 {} 60000 - 1 {SEED}", a.display()));
    assert_eq!(helper.line(), "CLONED g/8/1");
    // The app started after the restore, which wrote memory.
    let started = helper.w_at_least("g/8/1", 0, 1);

    const DIRTY: u64 = 16 << 20;
    let response = get(&a, &format!("/?dirty={DIRTY}"));
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    // Not all of it: the guest may put some of it in memory it had dirtied
    // already, and freed.
    helper.w_at_least("g/8/1", started, started + DIRTY * 3 / 4);

    // A sync park whose snapshot is taken and then cannot be saved, since
    // its path is a file: the fiber serves again, and its W carries what
    // was counted before the snapshot restarted the count.
    let failed = runs.path().join("failed");
    std::fs::create_dir(&failed).unwrap();
    std::fs::write(failed.join("snapshot"), b"not a directory").unwrap();
    helper.send(&format!("PARK g/8/1 {} 1", failed.display()));
    let error = helper.reply();
    assert!(error.starts_with("ERROR g/8/1 park: "), "{error}");
    // From here until the fiber exits, every W it reports, whoever reads
    // it, must be at least what was counted before the failed park: the
    // highest W the test has read for it.
    let floor = helper.highest_w["g/8/1"];
    helper.w_floor.insert("g/8/1".into(), floor);
    let response = get(&a, "/");
    assert!(response.starts_with("HTTP/1.1 200"), "{response}");
    // Waits for a W above what was counted before the failed park, so at
    // least one W after it is checked against the floor.
    helper.w_at_least("g/8/1", floor, floor + 1);

    let park = runs.path().join("park");
    std::fs::create_dir(&park).unwrap();
    helper.send(&format!("PARK g/8/1 {} 0", park.display()));
    assert!(helper.reply().starts_with("PARKED g/8/1 "));
    assert_eq!(helper.reply(), "EXITED g/8/1 exit:0");
    helper.w_floor.remove("g/8/1");

    let b = runs.path().join("b");
    helper.send(&format!(
        "RESUME g/8/2 {} {} 60000 2 {SEED}",
        park.display(),
        b.display()
    ));
    assert_eq!(helper.line(), "CLONED g/8/2");
    let resumed = helper.line();
    let resumed: u64 = resumed
        .strip_prefix("W g/8/2 ")
        .unwrap_or_else(|| panic!("{resumed:?} is not the resumed fiber's W"))
        .parse()
        .unwrap();
    // What the park recorded, at least the highest W reported before it,
    // then what was counted since the restore, but not the park's count
    // twice.
    let note: serde_json::Value =
        serde_json::from_slice(&std::fs::read(park.join("lampyr-helper.json")).unwrap()).unwrap();
    let dirtied = note["dirtied"].as_u64().unwrap();
    let reported = helper.highest_w["g/8/1"];
    assert!(
        dirtied >= reported,
        "the park recorded {dirtied}, below the {reported} reported before it"
    );
    assert!(
        (dirtied..dirtied + (4 << 20)).contains(&resumed),
        "the resumed fiber reports {resumed}, its park {dirtied}"
    );

    helper.send("KILL g/8/2");
    assert_eq!(helper.reply(), "EXITED g/8/2 exit:137");
}

/// A resume whose guest spins in its restore never answers, and KILL still
/// ends it. The park is taken while the app waits on a connection, which
/// is gone once the park is restored, so the app spins as it is restored.
#[test]
fn a_resume_that_spins_as_it_is_restored_is_killed() {
    let template =
        PathBuf::from(std::env::var("LAMPYR_HELPER_INCARNATION_TEMPLATE").expect(
            "LAMPYR_HELPER_INCARNATION_TEMPLATE names the tests/fixtures/incarnation build",
        ));
    let runs = tempfile::tempdir().unwrap();
    let mut helper = Helper::start(&template);
    assert!(helper.line().starts_with("READY "));

    let a = runs.path().join("a");
    helper.send(&format!("CLONE g/9/1 {} 60000 - 1 {SEED}", a.display()));
    assert_eq!(helper.reply(), "CLONED g/9/1");
    // A body announced and held back: the app waits for it.
    let mut held = UnixStream::connect(&a).unwrap();
    write!(
        held,
        "POST /?spin=1 HTTP/1.1\r\nHost: fiber\r\nContent-Length: 8\r\n\r\n"
    )
    .unwrap();
    let started = std::time::Instant::now();
    while !helper
        .log()
        .contains("helper-fixture: waiting for the body")
    {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the app never waited for the body; log:\n{}",
            helper.log()
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let park = runs.path().join("park");
    std::fs::create_dir(&park).unwrap();
    helper.send(&format!("PARK g/9/1 {} 0", park.display()));
    assert!(helper.reply().starts_with("PARKED g/9/1 "));
    assert_eq!(helper.reply(), "EXITED g/9/1 exit:0");
    drop(held);

    let b = runs.path().join("b");
    helper.send(&format!(
        "RESUME g/9/2 {} {} 60000 2 {SEED}",
        park.display(),
        b.display()
    ));
    // The restore does not come back.
    let early = helper.line_within(Duration::from_secs(3));
    assert_eq!(early, None, "the spinning resume answered");
    helper.send("KILL g/9/2");
    assert_eq!(helper.line(), "EXITED g/9/2 exit:137");

    // The helper carries on.
    let c = runs.path().join("c");
    helper.send(&format!("CLONE g/9/3 {} 60000 - 1 {SEED}", c.display()));
    assert_eq!(helper.reply(), "CLONED g/9/3");
    helper.send("KILL g/9/3");
    assert_eq!(helper.reply(), "EXITED g/9/3 exit:137");
}

/// With one restore slot, a CLONE whose deadline the restore ahead of it
/// cannot leave room for is refused at once, and one KILLed while it waits
/// for the slot exits before the restore ahead has answered. The probe,
/// the first CLONE after READY, sets the estimate the refusal uses.
#[test]
fn restores_wait_for_a_slot_and_hopeless_ones_are_refused() {
    let runs = tempfile::tempdir().unwrap();
    let mut helper = Helper::start_with(&template(), &["--max-restores", "1"]);
    assert!(helper.line().starts_with("READY "));
    let ep = |name: &str| runs.path().join(name);
    helper.send(&format!(
        "CLONE g/8/1 {} 5000 - 1 {SEED}",
        ep("p").display()
    ));
    assert_eq!(helper.reply(), "CLONED g/8/1");
    helper.send(&format!(
        "CLONE g/8/2 {} 10000 - 1 {SEED}\n\
         CLONE g/8/3 {} 1 - 1 {SEED}\n\
         CLONE g/8/4 {} 60000 - 1 {SEED}\n\
         KILL g/8/4",
        ep("a").display(),
        ep("b").display(),
        ep("c").display(),
    ));
    let replies: Vec<String> = (0..3).map(|_| helper.reply()).collect();
    let at = |prefix: &str| {
        replies
            .iter()
            .position(|r| r.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix:?} in {replies:?}"))
    };
    assert!(
        replies[at("ERROR g/8/3 restores-busy: ")].contains("too many for the deadline"),
        "{replies:?}"
    );
    assert_eq!(replies[at("EXITED g/8/4")], "EXITED g/8/4 exit:137");
    assert!(
        at("EXITED g/8/4") < at("CLONED g/8/2"),
        "the KILLed CLONE was not waiting for the slot: {replies:?}"
    );
    assert!(!ep("b").exists() && !ep("c").exists());
    assert_served(&ep("a"), "/after-the-queue");
}

/// A PARK and a KILL sent together race, and the PARK is answered once
/// whichever way it goes, and the fiber ends as killed. A PARK taken
/// before the KILL is seen parks the fiber, and the KILL ends the parked
/// fiber. One still queued when the step returns is refused, as the
/// KILL's signal may have handed the guest an EINTR a park would keep:
/// that is how it mostly goes, since the fiber is idle and the signal
/// only ends its step's wait. One still queued when the step itself
/// fails is answered as the fiber ends. Any of the three is accepted,
/// so the race never fails the test.
#[test]
fn a_park_sent_with_a_kill_is_answered_and_the_fiber_killed() {
    let runs = tempfile::tempdir().unwrap();
    let mut helper = Helper::start(&template());
    assert!(helper.line().starts_with("READY "));
    for i in 1..=3 {
        let fence = format!("g/8/{i}");
        let ep = runs.path().join(format!("f{i}"));
        helper.send(&format!("CLONE {fence} {} 10000 - 1 {SEED}", ep.display()));
        assert_eq!(helper.reply(), format!("CLONED {fence}"));
        assert_served(&ep, "/before-the-kill");
        let park = runs.path().join(format!("park{i}"));
        std::fs::create_dir(&park).unwrap();
        // One write, so the KILL is right behind the PARK.
        helper.send(&format!("PARK {fence} {} 1\nKILL {fence}", park.display()));
        let answer = helper.reply();
        let parked = format!("PARKED {fence} ");
        let refused = format!("ERROR {fence} park: ");
        assert!(
            answer.starts_with(&parked) || answer.starts_with(&refused),
            "{answer}"
        );
        eprintln!("{answer}");
        assert_eq!(helper.reply(), format!("EXITED {fence} exit:137"));
    }
}

/// What idle fibers cost the host, and how soon an idle fiber takes PARK
/// and KILL, with LAMPYR_HELPER_IDLE_FIBERS fibers (50 by default). An
/// idle fiber's thread must wake well under the 100 times a second a
/// 10 ms step woke it, as it wakes about 4 times, and a KILL must not
/// wait out a whole step. CPU time is printed but not checked, as a
/// shared host makes it too noisy to.
#[test]
fn idle_fibers_cost() {
    let n: usize = std::env::var("LAMPYR_HELPER_IDLE_FIBERS")
        .ok()
        .and_then(|n| n.parse().ok())
        .unwrap_or(50);
    assert!(
        n >= 2,
        "LAMPYR_HELPER_IDLE_FIBERS={n}: one fiber is parked and the rest killed"
    );
    let runs = tempfile::tempdir().unwrap();
    let mut helper = Helper::start(&template());
    assert!(helper.line().starts_with("READY "));
    let ep = |i: usize| runs.path().join(format!("f{i}"));
    for i in 0..n {
        helper.send(&format!(
            "CLONE g/7/{i} {} 60000 - 1 {SEED}",
            ep(i).display()
        ));
        assert_eq!(helper.reply(), format!("CLONED g/7/{i}"));
    }
    // The births' last W reports and restores settle first.
    std::thread::sleep(Duration::from_secs(2));
    let pid = helper.child.id();
    // The helper's CPU time in clock ticks, its live threads' context
    // switches, and which threads those are. CPU time counts threads that
    // have exited, and switches do not, so the threads are compared too.
    let usage = || {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
        let fields: Vec<&str> = stat[stat.rfind(')').unwrap() + 2..].split(' ').collect();
        let ticks = fields[11].parse::<u64>().unwrap() + fields[12].parse::<u64>().unwrap();
        let mut switches = 0u64;
        let mut tasks = Vec::new();
        for task in std::fs::read_dir(format!("/proc/{pid}/task")).unwrap() {
            let task = task.unwrap();
            tasks.push(task.file_name());
            let status = std::fs::read_to_string(task.path().join("status")).unwrap_or_default();
            for line in status.lines() {
                if let Some(v) = line
                    .strip_prefix("voluntary_ctxt_switches:")
                    .or_else(|| line.strip_prefix("nonvoluntary_ctxt_switches:"))
                {
                    switches += v.trim().parse::<u64>().unwrap();
                }
            }
        }
        tasks.sort();
        (ticks, switches, tasks)
    };
    let window = Duration::from_secs(3);
    let (t0, s0, before) = usage();
    std::thread::sleep(window);
    let (t1, s1, after) = usage();
    assert_eq!(
        before, after,
        "threads came or went in the window, so the switches are not all counted"
    );
    // USER_HZ, which /proc counts CPU time in: 100 on most builds, but
    // set by the architecture, so the kernel is asked.
    // SAFETY: sysconf has no preconditions.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    assert!(hz > 0.0, "sysconf(_SC_CLK_TCK) failed");
    let switches = s1.saturating_sub(s0) as f64 / window.as_secs_f64();
    let per_fiber = switches / n as f64;
    println!(
        "{n} idle fibers: {:.3} CPUs, {switches:.0} context switches/s, \
         {per_fiber:.1} per fiber",
        (t1 - t0) as f64 / hz / window.as_secs_f64(),
    );
    // The helper's other threads switch too, and are counted against the
    // fibers, which only leaves the ceiling more room with more fibers.
    assert!(
        per_fiber < 20.0,
        "{per_fiber:.1} context switches a second per idle fiber, nearer \
         a 10 ms step's 100 than the 4 of a 250 ms one"
    );

    let park = runs.path().join("park");
    std::fs::create_dir(&park).unwrap();
    let started = std::time::Instant::now();
    helper.send(&format!("PARK g/7/0 {} 0", park.display()));
    let parked = helper.reply();
    let took = started.elapsed();
    assert!(parked.starts_with("PARKED g/7/0 "), "{parked}");
    assert_eq!(helper.reply(), "EXITED g/7/0 exit:0");
    println!("PARK of an idle fiber answered in {took:?}");
    // Without the nudge, a KILL waits for its fiber's 250 ms step to run
    // out, which comes at a different point in each fiber's step, so the
    // median would be near half a step. With it, the KILL is taken in
    // about 10 ms. The gap between KILLs keeps them from all landing at
    // one point in the fibers' steps.
    let mut kills = Vec::new();
    for i in 1..n.min(21) {
        std::thread::sleep(Duration::from_millis(37));
        let started = std::time::Instant::now();
        helper.send(&format!("KILL g/7/{i}"));
        assert_eq!(helper.reply(), format!("EXITED g/7/{i} exit:137"));
        kills.push(started.elapsed());
    }
    kills.sort();
    let (median, max) = (kills[kills.len() / 2], kills[kills.len() - 1]);
    println!(
        "KILL of an idle fiber: median {median:?}, max {max:?} over {}",
        kills.len()
    );
    assert!(
        median < Duration::from_millis(60),
        "a KILL of an idle fiber took a median {median:?} to be EXITED, as \
         if it waited out its 250 ms step rather than being woken"
    );
}
