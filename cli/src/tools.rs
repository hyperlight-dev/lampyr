// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

use crate::runtime::Cancellation;

pub struct CommandSpec {
    pub program: PathBuf,
    pub arguments: Vec<OsString>,
    pub directory: PathBuf,
}

pub type LocalTools = BTreeMap<String, CommandSpec>;

#[derive(Serialize)]
pub struct ToolOutput {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

pub enum Output {
    Capture,
    File(std::fs::File),
    Interactive,
}

pub fn run(
    tools: &LocalTools,
    name: &str,
    deadline: Instant,
    cancellation: &Cancellation,
) -> Result<ToolOutput> {
    let spec = tools
        .get(name)
        .context("local tool operation is not authorized")?;
    ensure!(
        !cancellation.is_cancelled(),
        "local tool operation cancelled"
    );
    ensure!(Instant::now() < deadline, "local tool deadline exceeded");
    let program = which::which(&spec.program).context("resolve authorized local tool")?;
    let mut command = std::process::Command::new(program);
    command
        .args(&spec.arguments)
        .current_dir(&spec.directory)
        .stdin(std::process::Stdio::null());
    execute(
        &mut command,
        Output::Capture,
        16 * 1024,
        Some(deadline),
        cancellation,
    )
}

pub fn execute(
    command: &mut std::process::Command,
    output: Output,
    limit: usize,
    deadline: Option<Instant>,
    cancellation: &Cancellation,
) -> Result<ToolOutput> {
    ensure!(!cancellation.is_cancelled(), "command cancelled");
    ensure!(
        deadline.is_none_or(|deadline| Instant::now() < deadline),
        "command deadline exceeded"
    );
    ensure!(
        (1..=16 * 1024 * 1024).contains(&limit),
        "invalid command output limit"
    );
    #[cfg(unix)]
    {
        unix::execute(command, output, limit, deadline, cancellation)
    }
    #[cfg(not(unix))]
    {
        let _ = (command, output);
        anyhow::bail!("process execution is not supported on this host platform")
    }
}

#[cfg(unix)]
mod unix {
    use std::io::{ErrorKind, Read};
    use std::os::fd::AsRawFd;
    use std::os::unix::process::{CommandExt, ExitStatusExt};
    #[cfg(target_os = "linux")]
    use std::path::Path;
    use std::process::{Child, Command, Stdio};
    use std::time::Duration;

    use anyhow::bail;

    use super::*;

    pub fn execute(
        command: &mut Command,
        output: Output,
        limit: usize,
        deadline: Option<Instant>,
        cancellation: &Cancellation,
    ) -> Result<ToolOutput> {
        let interactive = matches!(&output, Output::Interactive);
        match output {
            Output::Capture => {
                command.stdout(Stdio::piped()).stderr(Stdio::piped());
            }
            Output::File(file) => {
                command.stdout(file).stderr(Stdio::piped());
            }
            Output::Interactive => {
                command.stdout(Stdio::inherit()).stderr(Stdio::inherit());
            }
        }
        let mut child = command.process_group(0).spawn().context("start process")?;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        if let Err(error) = stdout
            .as_ref()
            .map(nonblocking)
            .transpose()
            .and_then(|_| stderr.as_ref().map(nonblocking).transpose())
        {
            terminate(&mut child)?;
            return Err(error);
        }
        std::thread::scope(|scope| {
            let stdout = stdout
                .map(|pipe| scope.spawn(move || capture(pipe, limit, deadline, cancellation)));
            let stderr = stderr
                .map(|pipe| scope.spawn(move || capture(pipe, limit, deadline, cancellation)));
            let status = loop {
                if cancellation.is_cancelled() {
                    if interactive {
                        terminate_gracefully(&mut child)?;
                    } else {
                        terminate(&mut child)?;
                    }
                    break None;
                }
                if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                    terminate(&mut child)?;
                    break None;
                }
                // Keep the leader unreaped until readers finish, so its process
                // group ID cannot be recycled before a deadline kills the group.
                if stdout.as_ref().is_none_or(|reader| reader.is_finished())
                    && stderr.as_ref().is_none_or(|reader| reader.is_finished())
                {
                    match child.try_wait() {
                        Ok(Some(status)) => break Some(status),
                        Ok(None) => {}
                        Err(error) => {
                            terminate(&mut child)?;
                            return Err(error).context("wait for process");
                        }
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            };
            let (stdout, out_truncated) = match stdout {
                Some(reader) => reader
                    .join()
                    .map_err(|_| anyhow::anyhow!("stdout reader panicked"))??,
                None => (String::new(), false),
            };
            let (stderr, err_truncated) = match stderr {
                Some(reader) => reader
                    .join()
                    .map_err(|_| anyhow::anyhow!("stderr reader panicked"))??,
                None => (String::new(), false),
            };
            ensure!(
                !cancellation.is_cancelled(),
                "local tool operation cancelled"
            );
            let status = status.context("local tool deadline exceeded")?;
            Ok(ToolOutput {
                exit: status
                    .code()
                    .or_else(|| status.signal().map(|signal| 128 + signal))
                    .context("process exit status is missing")?,
                stdout,
                stderr,
                truncated: out_truncated || err_truncated,
            })
        })
    }

    fn nonblocking(pipe: &impl AsRawFd) -> Result<()> {
        let fd = pipe.as_raw_fd();
        // The pipe owns this live descriptor; only its nonblocking flag changes.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(std::io::Error::last_os_error()).context("configure local tool pipe");
        }
        Ok(())
    }

    fn capture(
        mut pipe: impl Read,
        limit: usize,
        deadline: Option<Instant>,
        cancellation: &Cancellation,
    ) -> Result<(String, bool)> {
        let mut contents = Vec::new();
        let mut buffer = [0_u8; 4096];
        let mut truncated = false;
        while !cancellation.is_cancelled()
            && deadline.is_none_or(|deadline| Instant::now() < deadline)
        {
            match pipe.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let kept = count.min(limit - contents.len());
                    contents.extend_from_slice(&buffer[..kept]);
                    truncated |= kept < count;
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return Err(error).context("read local tool output"),
            }
        }
        Ok((
            String::from_utf8(contents).context("local tool output is not UTF-8")?,
            truncated,
        ))
    }

    fn terminate(child: &mut Child) -> Result<()> {
        let pid = i32::try_from(child.id()).context("local tool PID is out of range")?;
        ensure!(pid > 0, "invalid local tool process group");
        // process_group(0) created this group; the leader has not been reaped.
        let status = unsafe { libc::kill(-pid, libc::SIGKILL) };
        if status < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                bail!("terminate local tool group: {error}");
            }
        }
        child.wait().context("reap cancelled local tool")?;
        Ok(())
    }

    fn terminate_gracefully(child: &mut Child) -> Result<()> {
        let pid = i32::try_from(child.id()).context("local tool PID is out of range")?;
        ensure!(pid > 0, "invalid local tool process group");
        signal_process(pid, libc::SIGTERM)?;
        // ly-dev gets five seconds to drain requests; leave time for cleanup and summary output.
        let deadline = Instant::now() + Duration::from_secs(7);
        let mut reaped = false;
        loop {
            reaped |= child
                .try_wait()
                .context("wait for interactive local tool")?
                .is_some();
            if !process_group_exists(pid)? {
                if !reaped {
                    child.wait().context("reap interactive local tool")?;
                }
                return Ok(());
            }
            if Instant::now() >= deadline {
                signal_group(pid, libc::SIGKILL)?;
                if !reaped {
                    child.wait().context("reap killed interactive local tool")?;
                }
                let gone = Instant::now() + Duration::from_secs(1);
                while process_group_exists(pid)? && Instant::now() < gone {
                    std::thread::sleep(Duration::from_millis(10));
                }
                ensure!(
                    !process_group_exists(pid)?,
                    "interactive local tool process group did not exit"
                );
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn signal_group(pid: i32, signal: i32) -> Result<()> {
        let status = unsafe { libc::kill(-pid, signal) };
        if status < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                bail!("signal local tool process group: {error}");
            }
        }
        Ok(())
    }

    fn signal_process(pid: i32, signal: i32) -> Result<()> {
        let status = unsafe { libc::kill(pid, signal) };
        if status < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                bail!("signal local tool process: {error}");
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn process_group_exists(pid: i32) -> Result<bool> {
        process_group_has_live_members(Path::new("/proc"), pid)
    }

    #[cfg(target_os = "linux")]
    fn process_group_has_live_members(proc_root: &Path, pid: i32) -> Result<bool> {
        for entry in std::fs::read_dir(proc_root).context("inspect proc process groups")? {
            let entry = entry.context("inspect proc process entry")?;
            if entry.file_name().to_string_lossy().parse::<i32>().is_err() {
                continue;
            }
            let stat = match std::fs::read_to_string(entry.path().join("stat")) {
                Ok(stat) => stat,
                Err(_) => continue,
            };
            let Some(command_end) = stat.rfind(')') else {
                continue;
            };
            let Some(fields) = stat.get(command_end + 1..) else {
                continue;
            };
            let fields = fields.split_whitespace().collect::<Vec<_>>();
            if fields.len() > 2
                && !matches!(fields[0], "Z" | "X" | "x")
                && fields[2].parse::<i32>().ok() == Some(pid)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    #[cfg(not(target_os = "linux"))]
    fn process_group_exists(pid: i32) -> Result<bool> {
        let status = unsafe { libc::kill(-pid, 0) };
        if status == 0 {
            return Ok(true);
        }
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            return Ok(false);
        }
        if error.raw_os_error() == Some(libc::EPERM) {
            return Ok(true);
        }
        Err(error).context("inspect local tool process group")
    }

    #[cfg(all(test, target_os = "linux"))]
    mod tests {
        use super::*;

        #[test]
        fn zombie_members_do_not_keep_a_process_group_alive() {
            let proc_root = tempfile::tempdir().unwrap();
            let zombie = proc_root.path().join("123");
            std::fs::create_dir(&zombie).unwrap();
            std::fs::write(zombie.join("stat"), "123 (sleep) Z 1 77 77 0 0\n").unwrap();
            assert!(!process_group_has_live_members(proc_root.path(), 77).unwrap());

            let malformed = proc_root.path().join("124");
            std::fs::create_dir(&malformed).unwrap();
            std::fs::write(malformed.join("stat"), "process exited while read\n").unwrap();
            let live = proc_root.path().join("125");
            std::fs::create_dir(&live).unwrap();
            std::fs::write(live.join("stat"), "125 (sleep) S 1 77 77 0 0\n").unwrap();
            assert!(process_group_has_live_members(proc_root.path(), 77).unwrap());

            std::fs::write(live.join("stat"), "125 (sleep) S 1 88 88 0 0\n").unwrap();
            assert!(!process_group_has_live_members(proc_root.path(), 77).unwrap());
        }
    }
}
