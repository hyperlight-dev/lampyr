// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressStyle};

use crate::runtime::Cancellation;
use crate::tools::{self, Output, ToolOutput};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Status {
    Pending,
    Ok,
    Fail,
}

pub(crate) fn status_label(status: Status, color: bool) -> String {
    let (label, style) = match status {
        Status::Pending => ("[..]", "36;1"),
        Status::Ok => ("[ok]", "32;1"),
        Status::Fail => ("[fail]", "31;1"),
    };
    if color {
        format!("\x1b[{style}m{label}\x1b[0m")
    } else {
        label.to_owned()
    }
}

fn write_step_status(
    output: &mut impl Write,
    inline: bool,
    color: bool,
    status: Status,
    label: &str,
) -> std::io::Result<()> {
    if status == Status::Pending {
        if inline {
            write!(output, "  {} {label}", status_label(status, color))?;
            output.flush()?;
        }
        return Ok(());
    }
    if inline {
        write!(output, "\r\x1b[2K")?;
    }
    writeln!(output, "  {} {label}", status_label(status, color))
}

pub(crate) fn progress_bar() -> Result<ProgressBar> {
    let progress = ProgressBar::new_spinner();
    let color = std::io::stderr().is_terminal() && std::env::var_os("NO_COLOR").is_none();
    progress.set_style(progress_style(color)?);
    Ok(progress)
}

fn progress_style(color: bool) -> Result<ProgressStyle> {
    let template = if color {
        "  {spinner:.cyan.bold} {msg}"
    } else {
        "  {spinner} {msg}"
    };
    Ok(
        ProgressStyle::with_template(template)?
            .tick_strings(&["[|]", "[/]", "[-]", "[\\]", "[..]"]),
    )
}

pub(crate) fn run_step<T>(
    progress: &ProgressBar,
    label: &str,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    progress.reset();
    progress.set_message(label.to_owned());
    progress.enable_steady_tick(Duration::from_millis(100));
    let output = action();
    progress.finish_and_clear();
    let stderr = std::io::stderr();
    let color = stderr.is_terminal() && std::env::var_os("NO_COLOR").is_none();
    let status = if output.is_ok() {
        Status::Ok
    } else {
        Status::Fail
    };
    write_step_status(&mut stderr.lock(), false, color, status, label)
        .context("write CLI progress")?;
    output
}

fn verbose_step<T>(
    output: &mut impl Write,
    verbose: bool,
    label: &str,
    action: impl FnOnce() -> Result<T>,
) -> Result<T> {
    if !verbose {
        return action();
    }
    let started = Instant::now();
    writeln!(
        output,
        "[verbose {}] {label}: starting",
        chrono::Utc::now().format("%H:%M:%S%.3fZ"),
    )
    .context("write CLI diagnostics")?;
    output.flush().context("flush CLI diagnostics")?;
    let result = action();
    let status = if result.is_ok() {
        "completed"
    } else {
        "failed"
    };
    writeln!(
        output,
        "[verbose {}] {label}: {status} ({:.3}s)",
        chrono::Utc::now().format("%H:%M:%S%.3fZ"),
        started.elapsed().as_secs_f64(),
    )
    .context("write CLI diagnostics")?;
    result
}

#[derive(Clone)]
pub struct Host {
    pub verbose: bool,
    pub cancellation: Cancellation,
}

impl Host {
    pub(crate) fn verbose_step<T>(
        &self,
        label: &str,
        action: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        verbose_step(&mut std::io::stderr(), self.verbose, label, action)
    }

    pub fn command(&self, program: impl AsRef<std::ffi::OsStr>) -> Result<Command> {
        let program = program.as_ref();
        let path = match which::which(program) {
            Ok(path) => path,
            Err(which::Error::CannotFindBinaryPath)
                if program == std::ffi::OsStr::new("kubectl") =>
            {
                crate::assets::kubectl(self)?
            }
            Err(error) => return Err(error).context("resolve executable"),
        };
        Ok(Command::new(path))
    }

    pub fn output(&self, command: &mut Command) -> Result<ToolOutput> {
        self.output_timeout(command, Duration::from_secs(600))
    }

    pub fn output_timeout(&self, command: &mut Command, timeout: Duration) -> Result<ToolOutput> {
        command.stdin(Stdio::null());
        tools::execute(
            command,
            Output::Capture,
            8 * 1024 * 1024,
            Some(Instant::now() + timeout),
            &self.cancellation,
        )
    }

    pub fn capture(&self, command: &mut Command) -> Result<String> {
        let output = self.output(command)?;
        self.require_success(command, &output)?;
        ensure!(!output.truncated, "command output exceeds 8 MiB");
        Ok(output.stdout.trim().to_owned())
    }

    pub fn run(&self, command: &mut Command) -> Result<()> {
        let output = self.output(command)?;
        self.finish(command, output)
    }

    pub fn file(&self, command: &mut Command, destination: std::fs::File) -> Result<()> {
        let output = tools::execute(
            command,
            Output::File(destination),
            8 * 1024 * 1024,
            Some(Instant::now() + Duration::from_secs(600)),
            &self.cancellation,
        )?;
        self.finish(command, output)
    }

    pub fn interactive(&self, command: &mut Command) -> Result<()> {
        command.stdin(Stdio::inherit());
        let output = tools::execute(
            command,
            Output::Interactive,
            8 * 1024 * 1024,
            None,
            &self.cancellation,
        )?;
        self.require_success(command, &output)
    }

    pub fn cleanup(&self, command: &mut Command) -> Result<()> {
        let output = tools::execute(
            command,
            Output::Capture,
            8 * 1024 * 1024,
            Some(Instant::now() + Duration::from_secs(30)),
            &Cancellation::default(),
        )?;
        self.finish(command, output)
    }

    pub fn require_success(&self, command: &Command, output: &ToolOutput) -> Result<()> {
        ensure!(
            output.exit == 0,
            "{} exited {}\n{}{}{}",
            command.get_program().to_string_lossy(),
            output.exit,
            output.stderr,
            output.stdout,
            if output.truncated {
                "\n[command output truncated]"
            } else {
                ""
            }
        );
        Ok(())
    }

    fn finish(&self, command: &Command, output: ToolOutput) -> Result<()> {
        if self.verbose {
            eprint!("{}{}", output.stdout, output.stderr);
            if output.truncated {
                eprintln!("[command output truncated]");
            }
        }
        self.require_success(command, &output)
    }

    pub fn step<T>(&self, label: &str, action: impl FnOnce() -> Result<T>) -> Result<T> {
        let progress = progress_bar()?;
        if self.verbose {
            progress.set_draw_target(ProgressDrawTarget::hidden());
        }
        run_step(&progress, label, action)
    }
}

pub(crate) fn interruptible_http<T>(
    agent: &ureq::Agent,
    host: &Host,
    operation: impl FnOnce() -> T,
) -> Result<T> {
    ensure!(
        !host.cancellation.is_cancelled(),
        "HTTP operation cancelled"
    );
    std::thread::scope(|scope| {
        let (finished, waiting) = std::sync::mpsc::channel::<()>();
        let interrupt = agent.clone();
        scope.spawn(move || {
            loop {
                match waiting.recv_timeout(Duration::from_millis(10)) {
                    Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if host.cancellation.is_cancelled() {
                            interrupt.shutdown();
                            break;
                        }
                    }
                }
            }
        });
        let result = operation();
        let _ = finished.send(());
        ensure!(
            !host.cancellation.is_cancelled(),
            "HTTP operation cancelled"
        );
        Ok(result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbose_steps_are_silent_unless_enabled() {
        let mut output = Vec::new();
        assert_eq!(
            verbose_step(&mut output, false, "sandbox boot", || Ok(42)).unwrap(),
            42
        );
        assert!(output.is_empty());
        assert_eq!(
            verbose_step(&mut output, true, "sandbox boot", || Ok(42)).unwrap(),
            42
        );
        let text = String::from_utf8(output).unwrap();
        assert_eq!(text.lines().count(), 2);
        assert!(text.contains("[verbose "));
        assert!(text.contains("Z] sandbox boot: starting"));
        assert!(text.contains("Z] sandbox boot: completed ("));
        assert!(text.ends_with("s)\n"));
    }

    #[test]
    fn verbose_steps_report_failures_without_logging_error_contents() {
        let mut output = Vec::new();
        let error = verbose_step::<()>(&mut output, true, "provider load", || {
            anyhow::bail!("private diagnostic")
        })
        .unwrap_err();
        assert_eq!(error.to_string(), "private diagnostic");
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("provider load: starting"));
        assert!(text.contains("provider load: failed ("));
        assert!(!text.contains("private diagnostic"));
    }

    #[test]
    fn status_labels_match_the_doctor_style() {
        assert_eq!(status_label(Status::Pending, false), "[..]");
        assert_eq!(status_label(Status::Ok, false), "[ok]");
        assert_eq!(status_label(Status::Fail, false), "[fail]");
        assert_eq!(status_label(Status::Ok, true), "\u{1b}[32;1m[ok]\u{1b}[0m");
    }

    #[test]
    fn noninteractive_steps_emit_only_the_final_result() {
        let mut output = Vec::new();
        write_step_status(&mut output, false, false, Status::Pending, "Doing work").unwrap();
        write_step_status(&mut output, false, false, Status::Ok, "Doing work").unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "  [ok] Doing work\n");
    }

    #[test]
    fn interactive_steps_replace_pending_status_in_place() {
        let mut output = Vec::new();
        write_step_status(&mut output, true, false, Status::Pending, "Doing work").unwrap();
        write_step_status(&mut output, true, false, Status::Fail, "Doing work").unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "  [..] Doing work\r\u{1b}[2K  [fail] Doing work\n"
        );
    }

    #[test]
    fn spinner_preserves_long_urls_on_narrow_terminals() {
        let terminal = indicatif::InMemoryTerm::new(10, 40);
        let progress = ProgressBar::with_draw_target(
            None,
            ProgressDrawTarget::term_like(Box::new(terminal.clone())),
        )
        .with_style(progress_style(false).unwrap());
        let url =
            "https://login.microsoftonline.com/organizations/oauth2/v2.0/authorize?state=complete";
        progress.set_message(url);
        progress.tick();
        assert!(terminal.contents().replace('\n', "").contains(url));
    }
}
