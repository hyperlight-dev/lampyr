// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

//! The line protocol of fiberd's hack/hyperlight/PROTOCOL.md, as data.
//!
//! Every message is one line, fields one space apart. This module only
//! parses fiberd's commands and redacts them for the log; what a command
//! does is the fiber module's business.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};

/// Bytes of entropy fiberd draws for every CLONE and RESUME.
pub const SEED_BYTES: usize = 32;

/// What fiberd tells a fiber about itself before it serves.
#[derive(Debug, PartialEq, Eq)]
pub struct Incarnation {
    /// fiberd's count of the fiber's lineage: 1 for a clone, the parked
    /// generation plus one for a resume.
    pub generation: u64,
    /// The fresh seed. hyperlight-unikraft reseeds the guest's generators
    /// from host entropy on every restore, so the helper only checks it.
    pub seed: [u8; SEED_BYTES],
}

/// One line from fiberd.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Clone {
        fence: String,
        endpoint: PathBuf,
        deadline: Duration,
        payload: Vec<u8>,
        incarnation: Incarnation,
    },
    Park {
        fence: String,
        dir: PathBuf,
        sync: bool,
    },
    Resume {
        fence: String,
        dir: PathBuf,
        endpoint: PathBuf,
        deadline: Duration,
        incarnation: Incarnation,
    },
    Kill {
        fence: String,
    },
}

impl Command {
    /// The fence the command is about, so a line that fails to parse can
    /// still be answered with an ERROR for it. Only a well-formed fence is:
    /// one a lossy decode of a non-UTF-8 line mangled names no fiber fiberd
    /// sent, and an ERROR for it would only confuse fiberd's log.
    pub fn fence_of(line: &str) -> Option<&str> {
        line.split(' ').nth(1).filter(|f| fence(f).is_ok())
    }

    pub fn parse(line: &str) -> Result<Self> {
        let f: Vec<&str> = line.split(' ').collect();
        let want = |n: usize| -> Result<()> {
            if f.len() != n + 1 {
                bail!("{} needs {n} fields, got {}", f[0], f.len() - 1);
            }
            Ok(())
        };
        match f[0] {
            "CLONE" => {
                want(6)?;
                Ok(Command::Clone {
                    fence: fence(f[1])?,
                    endpoint: path(f[2])?,
                    deadline: deadline(f[3])?,
                    payload: if f[4] == "-" {
                        Vec::new()
                    } else {
                        unhex(f[4]).context("payload")?
                    },
                    incarnation: incarnation(f[5], f[6])?,
                })
            }
            "PARK" => {
                want(3)?;
                let sync = match f[3] {
                    "0" => false,
                    "1" => true,
                    other => bail!("PARK sync is 0 or 1, not {other:?}"),
                };
                Ok(Command::Park {
                    fence: fence(f[1])?,
                    dir: path(f[2])?,
                    sync,
                })
            }
            "RESUME" => {
                want(6)?;
                Ok(Command::Resume {
                    fence: fence(f[1])?,
                    dir: path(f[2])?,
                    endpoint: path(f[3])?,
                    deadline: deadline(f[4])?,
                    incarnation: incarnation(f[5], f[6])?,
                })
            }
            "KILL" => {
                want(1)?;
                Ok(Command::Kill {
                    fence: fence(f[1])?,
                })
            }
            other => Err(anyhow!("unknown command {other:?}")),
        }
    }
}

/// A fence as fiberd writes one: printable ASCII without a space.
fn fence(f: &str) -> Result<String> {
    if f.is_empty() || !f.bytes().all(|b| (0x21..=0x7e).contains(&b)) {
        bail!("invalid fence {f:?}");
    }
    Ok(f.to_string())
}

/// Paths in the protocol are absolute host paths.
fn path(p: &str) -> Result<PathBuf> {
    let p = PathBuf::from(p);
    if !p.is_absolute() {
        bail!("{} is not an absolute path", p.display());
    }
    Ok(p)
}

fn deadline(ms: &str) -> Result<Duration> {
    Ok(Duration::from_millis(
        ms.parse().with_context(|| format!("deadline {ms:?}"))?,
    ))
}

fn incarnation(generation: &str, seed: &str) -> Result<Incarnation> {
    let generation: u64 = generation
        .parse()
        .with_context(|| format!("generation {generation:?}"))?;
    if generation < 1 {
        bail!("generation {generation} is not an incarnation");
    }
    let bytes = unhex(seed).context("seed")?;
    let seed: [u8; SEED_BYTES] = bytes
        .try_into()
        .map_err(|_| anyhow!("seed is not {SEED_BYTES} hex bytes"))?;
    Ok(Incarnation { generation, seed })
}

fn unhex(h: &str) -> Result<Vec<u8>> {
    // Hex digits only, so the byte slicing below never splits a character.
    if !h.len().is_multiple_of(2) || !h.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("not an even run of hex digits");
    }
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&h[i..i + 2], 16).map_err(|_| anyhow!("not hex")))
        .collect()
}

/// The command line as the log shows it. A CLONE's payload is the
/// caller's and a CLONE's or RESUME's seed is the fiber's alone, so both
/// are shown by size, never by value.
pub fn log_line(line: &str) -> String {
    let f: Vec<&str> = line.split(' ').collect();
    let (payload, seed) = match f[0] {
        "CLONE" => (Some(4), Some(6)),
        "RESUME" => (None, Some(6)),
        _ => (None, None),
    };
    f.iter()
        .enumerate()
        .map(|(i, v)| match (Some(i) == payload, Some(i) == seed) {
            (true, _) if *v != "-" => format!("payload:{}B", v.len() / 2),
            (_, true) => format!("seed:{}B", v.len() / 2),
            _ => v.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    #[test]
    fn commands_parse_as_fiberd_writes_them() {
        let clone = Command::parse(&format!("CLONE g/1/2 /run/ep 500 6869 1 {SEED}")).unwrap();
        let Command::Clone {
            fence,
            endpoint,
            deadline,
            payload,
            incarnation,
        } = clone
        else {
            panic!("{clone:?}")
        };
        assert_eq!(fence, "g/1/2");
        assert_eq!(endpoint, PathBuf::from("/run/ep"));
        assert_eq!(deadline, Duration::from_millis(500));
        assert_eq!(payload, b"hi");
        assert_eq!(incarnation.generation, 1);
        assert_eq!(incarnation.seed[31], 0x1f);

        assert!(matches!(
            Command::parse(&format!("CLONE f /ep 500 - 3 {SEED}")).unwrap(),
            Command::Clone { payload, .. } if payload.is_empty()
        ));
        assert_eq!(
            Command::parse("PARK f /park 1").unwrap(),
            Command::Park {
                fence: "f".into(),
                dir: "/park".into(),
                sync: true
            }
        );
        assert!(matches!(
            Command::parse(&format!("RESUME f /park /ep 2000 7 {SEED}")).unwrap(),
            Command::Resume {
                incarnation: Incarnation { generation: 7, .. },
                ..
            }
        ));
        assert_eq!(
            Command::parse("KILL f").unwrap(),
            Command::Kill { fence: "f".into() }
        );
    }

    #[test]
    fn malformed_commands_are_refused() {
        for line in [
            "CLONE f /ep 500 -".to_string(),
            format!("CLONE f /ep 500 - 0 {SEED}"),
            format!("CLONE f /ep 500 - x {SEED}"),
            "CLONE f /ep 500 - 1 0011".to_string(),
            format!("CLONE f /ep 500 - 1 {SEED}zz"),
            format!("CLONE f relative 500 - 1 {SEED}"),
            format!("CLONE f /ep soon - 1 {SEED}"),
            format!("CLONE f /ep 500 6 1 {SEED}"),
            // Not hex, and a slice into it would split a character.
            format!("CLONE f /ep 500 é0 1 {SEED}"),
            format!("CLONE f /ep 500 - 1 {}é0", &SEED[..62]),
            "PARK f /park 2".to_string(),
            "KILL".to_string(),
            "KILL f extra".to_string(),
            "SPAWN f".to_string(),
        ] {
            assert!(Command::parse(&line).is_err(), "{line}");
        }
        assert_eq!(Command::fence_of("PARK f /park 2"), Some("f"));
        assert_eq!(Command::fence_of("KILL"), None);
        let lossy = String::from_utf8_lossy(b"KILL f\xff");
        assert_eq!(Command::fence_of(&lossy), None);
        assert_eq!(Command::fence_of("KILL f\u{e9}"), None);
    }

    #[test]
    fn the_log_shows_no_seed_and_no_payload() {
        for (line, want) in [
            (
                format!("CLONE f /ep 500 6869 1 {SEED}"),
                "CLONE f /ep 500 payload:2B 1 seed:32B",
            ),
            (
                format!("CLONE f /ep 500 - 3 {SEED}"),
                "CLONE f /ep 500 - 3 seed:32B",
            ),
            (
                format!("RESUME f /park /ep 2000 2 {SEED}"),
                "RESUME f /park /ep 2000 2 seed:32B",
            ),
            ("PARK f /park 1".to_string(), "PARK f /park 1"),
            ("KILL f".to_string(), "KILL f"),
        ] {
            let got = log_line(&line);
            assert_eq!(got, want, "{line}");
            assert!(!got.contains(SEED), "{got}");
        }
    }
}
