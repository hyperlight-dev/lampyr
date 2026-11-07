// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 The Hyperlight Authors.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use hyperlight_unikraft::{AppSandbox, Exec, SandboxBuilder, Yield};
use serde_json::Value;

pub const PAYLOAD_LIMIT: usize = 64 * 1024;

#[derive(Clone, Default)]
pub struct Cancellation(Arc<AtomicBool>, Option<Instant>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) || self.1.is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub fn with_deadline(&self, deadline: Instant) -> Self {
        Self(
            self.0.clone(),
            Some(self.1.map_or(deadline, |previous| previous.min(deadline))),
        )
    }
}

pub struct CallOutput {
    pub value: Value,
    pub stdout: String,
    pub stderr: String,
    pub diagnostics_truncated: bool,
}

pub struct Runtime {
    sandbox: AppSandbox,
    stopped: bool,
}

impl Runtime {
    pub fn boot(builder: SandboxBuilder) -> Result<Self> {
        let builder = builder.output_limit(PAYLOAD_LIMIT).echo_output(false);
        let sandbox = builder.boot().context("boot provider sandbox")?;
        Ok(Self {
            sandbox,
            stopped: false,
        })
    }

    pub fn load(
        &mut self,
        script: &str,
        timeout: Duration,
        cancellation: &Cancellation,
    ) -> Result<()> {
        self.execute(timeout, cancellation, Exec::Code(script.to_owned()))
            .map(|_| ())
    }

    pub fn call(
        &mut self,
        function: &str,
        input: &Value,
        timeout: Duration,
        cancellation: &Cancellation,
    ) -> Result<CallOutput> {
        ensure!(
            !function.is_empty()
                && function.len() <= 128
                && function
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_'),
            "invalid provider export name"
        );
        let input = serde_json::to_string(input)?;
        ensure!(
            input.len() <= PAYLOAD_LIMIT,
            "provider input exceeds 64 KiB"
        );
        let output = self.execute(
            timeout,
            cancellation,
            Exec::Call {
                function: function.to_owned(),
                input,
            },
        )?;
        ensure!(
            output.value.len() <= PAYLOAD_LIMIT,
            "provider result exceeds 64 KiB"
        );
        Ok(CallOutput {
            value: serde_json::from_str(&output.value).context("decode provider result JSON")?,
            stdout: output.stdout,
            stderr: output.stderr,
            diagnostics_truncated: output.diagnostics_truncated,
        })
    }

    fn execute(
        &mut self,
        timeout: Duration,
        cancellation: &Cancellation,
        operation: Exec,
    ) -> Result<RawOutput> {
        ensure!(!self.stopped, "provider sandbox has been stopped");
        ensure!(!cancellation.is_cancelled(), "provider operation cancelled");
        ensure!(!timeout.is_zero(), "provider operation deadline exceeded");
        let deadline = Instant::now()
            .checked_add(timeout)
            .context("provider deadline is out of range")?;
        let interrupt = self.sandbox.interrupt_handle();
        let (finished, completion) = mpsc::channel();
        let (result, stopped) = std::thread::scope(|scope| {
            let watchdog = scope.spawn(move || {
                let mut stopped = None;
                loop {
                    let reason = if cancellation.is_cancelled() {
                        Some("provider operation cancelled")
                    } else if Instant::now() >= deadline {
                        Some("provider operation deadline exceeded")
                    } else {
                        None
                    };
                    if let Some(reason) = reason {
                        stopped = Some(reason);
                        interrupt.kill();
                    }
                    match completion.recv_timeout(Duration::from_millis(10)) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => return stopped,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
            });
            let result = (|| -> Result<String> {
                self.sandbox.submit(operation)?;
                loop {
                    ensure!(!cancellation.is_cancelled(), "provider operation cancelled");
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    ensure!(!remaining.is_zero(), "provider operation deadline exceeded");
                    match self
                        .sandbox
                        .step(remaining.min(Duration::from_millis(10)))?
                    {
                        Yield::CallDone => {
                            return String::from_utf8(
                                self.sandbox.take_result().unwrap_or_default(),
                            )
                            .context("provider result must be UTF-8");
                        }
                        Yield::CallFailed { status } => {
                            return Err(hyperlight_unikraft::Error::CallFailed { status }.into());
                        }
                        Yield::Exited { status } => {
                            return Err(hyperlight_unikraft::Error::GuestExited { status }.into());
                        }
                        Yield::Blocked { .. } => {}
                    }
                }
            })();
            // A closed receiver means the watchdog already interrupted the VM.
            let _ = finished.send(());
            (result, watchdog.join())
        });
        let stopped = stopped.map_err(|_| anyhow::anyhow!("provider watchdog panicked"))?;
        let stderr = self.sandbox.take_stderr();
        let stdout = self.sandbox.take_output();
        if let Some(reason) = stopped.or_else(|| {
            if cancellation.is_cancelled() {
                Some("provider operation cancelled")
            } else if Instant::now() >= deadline {
                Some("provider operation deadline exceeded")
            } else {
                None
            }
        }) {
            self.stopped = true;
            bail!("{reason}");
        }
        let result = result.with_context(|| {
            format!("provider operation failed\n{}{}", stderr.text, stdout.text)
        })?;
        Ok(RawOutput {
            value: result,
            diagnostics_truncated: stderr.truncated() || stdout.truncated(),
            stdout: stdout.text,
            stderr: stderr.text,
        })
    }
}

struct RawOutput {
    value: String,
    stdout: String,
    stderr: String,
    diagnostics_truncated: bool,
}
