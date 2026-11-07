// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! fiberd's helper for Lampyr snapshots: fiberd's hack/hyperlight/PROTOCOL.md
//! over fd 3, with every fiber a hyperlight-unikraft micro-VM.
//!
//! One helper per warm template. Warm loads the template's hluk snapshot
//! once and reports READY with the facts its snapshots depend on. CLONE
//! restores a sandbox from it and starts the app, RESUME restores one from
//! a park, PARK writes the sandbox to disk, and KILL ends it.
//!
//!   lampyr-hluk-helper --template-dir <dir> [--max-restores N]
//!                                             (fd 3 = control)
//!   lampyr-hluk-helper --version              (the snapshot facts, then
//!                                              the template-dir capability)
//!
//! Flags take one or two dashes, and a value after `=` or as the next
//! argument.

mod bindings;
mod bridge;
mod facts;
mod fiber;
mod fleet;
mod nudge;
mod ports;
mod protocol;
mod restores;
mod sock_diag;
mod template;

use std::ffi::OsString;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::ops::RangeInclusive;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use bindings::Bindings;

use fiber::{Cmd, Ctl, Origin, Spec};
use fleet::Fleet;
use protocol::Command;
use restores::Restores;
use template::Template;

struct Args {
    template_dir: PathBuf,
    port_range: RangeInclusive<u16>,
    /// The grant's device engine, which warm refuses.
    engine: Option<String>,
    /// How many fibers restore at once.
    max_restores: usize,
}

/// Splits a flag into its name and the value given with `=`, if any, as
/// fiberd spells flags: one or two dashes, then the name up to the first
/// `=`. So `-template-dir X`, `--template-dir X`, `-template-dir=X` and
/// `--template-dir=X` are one flag.
fn flag(arg: &str) -> Option<(&str, Option<&str>)> {
    let rest = arg.strip_prefix("--").or_else(|| arg.strip_prefix('-'))?;
    let (name, value) = match rest.split_once('=') {
        Some((name, value)) => (name, Some(value)),
        None => (rest, None),
    };
    // `---x` and `-=x` name no flag.
    (!name.is_empty() && !name.starts_with('-')).then_some((name, value))
}

/// A flag's value: what followed its `=`, or else the next argument.
fn value(
    name: &str,
    inline: Option<&str>,
    rest: &mut impl Iterator<Item = String>,
    what: &str,
) -> Result<String> {
    match inline {
        Some(value) => Ok(value.to_owned()),
        None => rest
            .next()
            .with_context(|| format!("--{name} needs {what}")),
    }
}

fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Args> {
    let mut template_dir = None;
    let mut port_range = ports::DEFAULT_RANGE;
    let mut engine = None;
    let mut max_restores = restores::default_limit();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let Some((name, inline)) = flag(&arg) else {
            bail!("unknown argument {arg}");
        };
        match name {
            // A template's arguments follow fiberd's, and one of them may
            // be --version: answered as the query, it would end the warm
            // with no READY and a clean exit.
            "version" => {
                ensure!(inline.is_none(), "--version takes no value");
                bail!("--version must be the only argument");
            }
            "template-dir" => {
                template_dir = Some(PathBuf::from(value(name, inline, &mut it, "a path")?))
            }
            // The ports guests listen on, outside the kernel's ephemeral
            // range and fiberd's endpoint range.
            "port-range" => {
                port_range = ports::parse_range(&value(name, inline, &mut it, "LOW-HIGH")?)?
            }
            // fiberd passes this for its own helper's guest binary. A
            // Lampyr template carries its guest kernel in the snapshot, so
            // it is noted and left unused.
            "guest" => {
                let value = value(name, inline, &mut it, "a path")?;
                eprintln!("helper: ignoring --guest {value}: Lampyr templates do not use it");
            }
            // A grant's device engine. Lampyr fibers have no relay to one,
            // so warm refuses it rather than report READY for a grant whose
            // fibers could not reach their device.
            "engine" => {
                let value = value(name, inline, &mut it, "a path")?;
                engine = (!value.is_empty()).then_some(value);
            }
            // fiberd's hyperlight backend does not bound concurrent
            // restores, so the helper does (the restores module).
            "max-restores" => {
                let value = value(name, inline, &mut it, "a count")?;
                max_restores =
                    value.parse().ok().filter(|&n| n > 0).with_context(|| {
                        format!("--max-restores {value:?} is not a positive count")
                    })?;
            }
            _ => bail!("unknown argument {arg}"),
        }
    }
    Ok(Args {
        template_dir: template_dir.context("--template-dir is required")?,
        port_range,
        engine,
        max_restores,
    })
}

/// What warm refuses before it loads anything.
fn refuse(args: &Args) -> Result<()> {
    if let Some(engine) = &args.engine {
        bail!(
            "--engine {engine}: this helper's fibers cannot reach a device engine, so it does not run grants that have one"
        );
    }
    Ok(())
}

/// Whether `args` asks for the version facts: `--version` and nothing
/// else, as fiberd asks at open.
fn version_query(args: &[String]) -> bool {
    matches!(args, [only] if flag(only) == Some(("version", None)))
}

/// The arguments as UTF-8, which every flag and value the helper takes
/// is. std::env::args would panic on one that is not; this fails the warm.
fn utf8(args: impl IntoIterator<Item = OsString>) -> Result<Vec<String>> {
    args.into_iter()
        .map(|arg| {
            arg.into_string()
                .map_err(|arg| anyhow!("argument {arg:?} is not UTF-8"))
        })
        .collect()
}

fn main() -> Result<()> {
    let args = utf8(std::env::args_os().skip(1))?;
    if version_query(&args) {
        println!("{}", facts::version());
        return Ok(());
    }
    let args = parse_args(args)?;
    // Before any thread starts: see nudge::install.
    nudge::install();
    // SAFETY: fiberd starts the helper with its end of the control
    // socketpair as fd 3, and nothing else in this process owns it.
    let ctl_file = unsafe { File::from_raw_fd(3) };
    let mut reader = BufReader::new(ctl_file.try_clone().context("fd 3")?);
    let ctl = Ctl::new(ctl_file);

    let warm = refuse(&args).and_then(|()| {
        let t = Template::load(&args.template_dir)?;
        let bindings = Bindings::new(&t.bindings, bindings::NAMESPACE, || {
            std::fs::read_to_string(bindings::POD_RESOLV_CONF).context("read the Pod's DNS")
        })?;
        let ports = ports::Ports::new(args.port_range.clone())?;
        let snapshot =
            hyperlight_unikraft::load_snapshot(&t.snapshot).context("load the snapshot")?;
        Ok((t, Arc::new(bindings), ports, snapshot))
    });
    let (template, bindings, ports, snapshot) = match warm {
        Ok(w) => w,
        Err(e) => {
            ctl.say(format!("ERROR warm {e:#}"));
            return Err(e);
        }
    };
    ctl.say(format!("READY {}", facts::line()));

    // Fibers' own directories, under the helper's working directory, which
    // fiberd makes per grant.
    // fiberd ends a helper with SIGKILL, so a previous one's are cleared here.
    let fibers_dir = std::env::current_dir()?.join("fibers");
    let _ = std::fs::remove_dir_all(&fibers_dir);
    let mut fleet = Fleet::default();
    let restores = Restores::new(args.max_restores);
    // fiberd times its first CLONE after READY, the probe, to size the
    // deadlines it gives the rest, so the probe never waits for a slot.
    let mut probe = true;
    let mut seq: u64 = 0;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        // fiberd closed the channel: end every fiber and exit.
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // Forget fibers whose threads have ended on every command, not only
        // a CLONE or RESUME, so a grant that only parks and kills for a
        // while does not keep them.
        fleet.reap();
        // A line that is not text is refused, not repaired: a repaired path
        // would name another file.
        let Ok(line) = std::str::from_utf8(&buf) else {
            let lossy = String::from_utf8_lossy(&buf);
            eprintln!(
                "helper: < (not UTF-8) {}",
                protocol::log_line(lossy.trim_end())
            );
            if let Some(fence) = Command::fence_of(lossy.trim_end()) {
                ctl.say(format!("ERROR {fence} the command is not UTF-8"));
            }
            continue;
        };
        let line = line.trim_end_matches(['\n', '\r']);
        if line.is_empty() {
            continue;
        }
        let arrived = Instant::now();
        // stderr is the grant's zygote.log: one line per command, so a
        // park that never comes back can be told from one that never
        // arrived.
        eprintln!("helper: < {}", protocol::log_line(line));
        let command = match Command::parse(line) {
            Ok(c) => c,
            Err(e) => {
                if let Some(fence) = Command::fence_of(line) {
                    ctl.say(format!("ERROR {fence} {e:#}"));
                }
                continue;
            }
        };
        let (fence, endpoint, deadline, generation, origin, probing) = match command {
            Command::Clone {
                fence,
                endpoint,
                deadline,
                payload,
                incarnation,
            } => {
                let origin = Origin::Clone {
                    snapshot: snapshot.clone(),
                    command: template.command.clone(),
                    payload,
                };
                let probing = probe;
                (
                    fence,
                    endpoint,
                    deadline,
                    incarnation.generation,
                    origin,
                    probing,
                )
            }
            Command::Resume {
                fence,
                dir,
                endpoint,
                deadline,
                incarnation,
            } => (
                fence,
                endpoint,
                deadline,
                incarnation.generation,
                Origin::Resume { dir },
                false,
            ),
            Command::Park { fence, dir, sync } => {
                match fleet.get(&fence) {
                    Some(f) if f.killing() => {
                        ctl.say(format!("ERROR {fence} the fiber is being killed"))
                    }
                    Some(f) if f.send(Cmd::Park { dir, sync }) => {}
                    _ => ctl.say(format!("ERROR {fence} unknown fiber")),
                }
                continue;
            }
            // The fiber stays tracked until its thread ends, and answers
            // EXITED itself, one waiting for a restore slot included, which
            // leaves the queue; a second KILL does nothing.
            Command::Kill { fence } => {
                if let Some(f) = fleet.get(&fence) {
                    f.send(Cmd::Kill);
                }
                continue;
            }
        };
        if let Err(why) = fleet.admit(&fence, &endpoint) {
            ctl.say(format!("ERROR {fence} {why}"));
            continue;
        }
        // Refused now rather than queued to miss its deadline.
        let until = deadline_from(arrived, deadline);
        let ticket = match restores.admit(until, probing) {
            Ok(ticket) => ticket,
            Err(why) => {
                ctl.say(format!("ERROR {fence} {why}"));
                continue;
            }
        };
        // Only a CLONE that restores is the probe: one refused above, say
        // for a fence already in use, leaves the next to time.
        probe &= !probing;
        seq += 1;
        let spec = Spec {
            bindings: bindings.clone(),
            ports: ports.clone(),
            scratch: fibers_dir.join(seq.to_string()),
            fence: fence.clone(),
            endpoint,
            deadline,
            until,
            generation,
            origin,
        };
        // Nothing is claimed for the fiber until its thread runs, so one
        // that gets none leaves only its ERROR behind.
        match fiber::start(ctl.clone(), spec, ticket) {
            Ok(f) => fleet.insert(fence, f),
            Err(e) => ctl.say(format!("ERROR {fence} no thread for the fiber: {e}")),
        }
    }
    // End every fiber so each removes its endpoint and its directory, and
    // give them a moment to, since the process takes the rest with it.
    fleet.shutdown(SHUTDOWN_GRACE);
    let _ = std::fs::remove_dir_all(&fibers_dir);
    Ok(())
}

/// When a deadline of `deadline` from `arrived` passes. fiberd's deadline
/// is any u64 of milliseconds, and one past the clock's range would panic,
/// so a deadline beyond a year, which no restore needs, is a year.
fn deadline_from(arrived: Instant, deadline: Duration) -> Instant {
    const YEAR: Duration = Duration::from_secs(365 * 24 * 3600);
    arrived + deadline.min(YEAR)
}

/// How long the fibers get to clean up after fiberd closes the channel.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

#[cfg(test)]
mod tests {
    use super::*;

    fn args(line: &str) -> Result<Args> {
        parse_args(line.split(' ').map(String::from))
    }

    #[test]
    fn an_engine_is_refused_at_warm_and_a_guest_is_ignored() {
        let with = args("--guest /g --template-dir /t --engine /run/engine.sock").unwrap();
        let err = refuse(&with).unwrap_err().to_string();
        assert!(err.starts_with("--engine /run/engine.sock: "), "{err}");
        let without = args("--guest /g --template-dir /t").unwrap();
        assert!(refuse(&without).is_ok());
        assert_eq!(without.max_restores, restores::default_limit());
        let empty = parse_args(["--template-dir", "/t", "--engine", ""].map(String::from)).unwrap();
        assert!(refuse(&empty).is_ok());
    }

    #[test]
    fn every_flag_takes_one_or_two_dashes_and_a_value_after_an_equals_or_a_space() {
        for dashes in ["-", "--"] {
            for line in [
                "{d}template-dir /t {d}port-range 20000-20009 {d}guest /g {d}engine /e {d}max-restores 3",
                "{d}template-dir=/t {d}port-range=20000-20009 {d}guest=/g {d}engine=/e {d}max-restores=3",
            ] {
                let line = line.replace("{d}", dashes);
                let parsed = args(&line).unwrap_or_else(|e| panic!("{line}: {e:#}"));
                assert_eq!(parsed.template_dir, PathBuf::from("/t"), "{line}");
                assert_eq!(parsed.port_range, 20000..=20009, "{line}");
                assert_eq!(parsed.engine.as_deref(), Some("/e"), "{line}");
                assert_eq!(parsed.max_restores, 3, "{line}");
            }
        }
    }

    #[test]
    fn only_a_lone_version_flag_asks_for_the_version() {
        let words = |line: &str| line.split(' ').map(String::from).collect::<Vec<_>>();
        assert!(version_query(&words("--version")));
        assert!(version_query(&words("-version")));
        for line in [
            "--version=",
            "---version",
            "--template-dir /t --version",
            "--version --template-dir /t",
        ] {
            assert!(!version_query(&words(line)), "{line}");
        }
    }

    #[test]
    fn an_argument_that_is_not_utf8_is_refused() {
        use std::os::unix::ffi::OsStringExt;
        let ok = utf8(["--template-dir", "/t"].map(OsString::from)).unwrap();
        assert_eq!(ok, ["--template-dir", "/t"]);
        let bad = [
            OsString::from("--template-dir"),
            OsString::from_vec(vec![0xff]),
        ];
        let err = utf8(bad).unwrap_err().to_string();
        assert!(err.contains("is not UTF-8"), "{err}");
    }

    #[test]
    fn any_deadline_fiberd_can_write_has_an_end() {
        let now = Instant::now();
        let ms = |n: u64| Duration::from_millis(n);
        assert_eq!(deadline_from(now, ms(500)), now + ms(500));
        assert!(deadline_from(now, ms(u64::MAX)) > now + ms(500));
    }

    #[test]
    fn a_value_is_everything_after_the_first_equals() {
        let parsed = args("--template-dir=/t=1 -engine=").unwrap();
        assert_eq!(parsed.template_dir, PathBuf::from("/t=1"));
        assert_eq!(parsed.engine, None);
        // A value given as the next argument is taken whole, dashes and all.
        let parsed = args("-template-dir --engine").unwrap();
        assert_eq!(parsed.template_dir, PathBuf::from("--engine"));
    }

    #[test]
    fn malformed_and_unknown_flags_are_refused() {
        for (line, want) in [
            ("---template-dir /t", "unknown argument ---template-dir"),
            ("-=/t", "unknown argument -=/t"),
            ("-", "unknown argument -"),
            ("--", "unknown argument --"),
            ("template-dir /t", "unknown argument template-dir"),
            ("--template-dir /t --guests /g", "unknown argument --guests"),
            ("--template-dir", "--template-dir needs a path"),
            ("-port-range", "--port-range needs LOW-HIGH"),
            ("--version=1", "--version takes no value"),
            (
                "--template-dir /t --version",
                "--version must be the only argument",
            ),
            (
                "-version --template-dir /t",
                "--version must be the only argument",
            ),
            ("--template-dir /t guest", "unknown argument guest"),
            (
                "--template-dir /t --max-restores",
                "--max-restores needs a count",
            ),
            (
                "--template-dir /t --max-restores 0",
                "--max-restores \"0\" is not",
            ),
            (
                "--template-dir /t --max-restores=-1",
                "--max-restores \"-1\" is not",
            ),
            ("--port-range 1-2", "--template-dir is required"),
        ] {
            let err = match args(line) {
                Ok(_) => panic!("{line}: parsed"),
                Err(e) => format!("{e:#}"),
            };
            assert!(err.starts_with(want), "{line}: {err}");
        }
    }
}
