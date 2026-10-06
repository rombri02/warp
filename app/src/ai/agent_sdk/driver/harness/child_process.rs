//! Running a third-party harness as a direct child of the driver instead of as a command typed
//! into the terminal session. The driver owns the process's stdio pipes and its lifetime; the
//! terminal session is left free for the harness's own shell commands.
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result};
use async_process::{Child, ChildStdin, ChildStdout, Stdio};
use command::r#async::Command;
use futures::FutureExt as _;
use futures::io::{AsyncBufReadExt as _, BufReader};
use futures::lock::Mutex as AsyncMutex;
use warp_core::command::ExitCode;
use warpui::r#async::Timer;
use warpui::r#async::executor::{Background, BackgroundTask};

#[cfg(unix)]
use super::process_control::kill_process_group;

/// Exit code reported when the process was killed without reporting one of its own.
const KILLED_EXIT_CODE: i32 = 137;

/// Describes the harness process to launch.
pub(crate) struct HarnessProcessLaunch<'a> {
    pub program: &'a str,
    pub args: &'a [String],
    pub cwd: &'a Path,
    pub env: &'a HashMap<OsString, OsString>,
}

/// The pipes of a freshly spawned harness. Stderr is not included because the process forwards
/// it to the log.
pub(crate) struct HarnessStdio {
    pub stdin: ChildStdin,
    pub stdout: ChildStdout,
}

/// A harness running as a child process in its own process group.
pub(crate) struct HarnessProcess {
    child: AsyncMutex<Option<Child>>,
    pid: u32,
    program: String,
    _stderr_task: BackgroundTask,
}

impl HarnessProcess {
    /// Spawns the process with piped stdio and starts forwarding its stderr to the log. The
    /// process leads its own group so `npx`-style wrappers and their children can be killed
    /// together.
    pub(crate) fn spawn(
        launch: HarnessProcessLaunch<'_>,
        executor: &Arc<Background>,
    ) -> Result<(Self, HarnessStdio)> {
        let mut command = Command::new_with_process_group(launch.program);
        command
            .args(launch.args.iter().map(OsStr::new))
            .current_dir(launch.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (key, value) in launch.env {
            command.env(key, value);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("Failed to spawn harness process `{}`", launch.program))?;
        let pid = child.id();
        log::info!(
            "Spawned harness process `{}` with pid {pid}",
            launch.program
        );

        let stdin = child.stdin.take().context("harness process has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("harness process has no stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("harness process has no stderr")?;

        let stderr_task = executor.spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => log::info!("harness process [pid {pid}] stderr: {}", line.trim_end()),
                }
            }
        });

        Ok((
            Self {
                child: AsyncMutex::new(Some(child)),
                pid,
                program: launch.program.to_owned(),
                _stderr_task: stderr_task,
            },
            HarnessStdio { stdin, stdout },
        ))
    }

    pub(crate) fn pid(&self) -> u32 {
        self.pid
    }

    pub(crate) fn program(&self) -> &str {
        &self.program
    }

    /// Waits for the process to exit, killing its process group if it outlives `grace`.
    ///
    /// Callers that want a graceful shutdown close the process's stdin (or send it a protocol
    /// level shutdown) before calling this. Returns the exit code the process reported, or
    /// [`KILLED_EXIT_CODE`] when it had to be killed or was terminated by a signal. A second
    /// call after the process has been reaped also reports [`KILLED_EXIT_CODE`].
    pub(crate) async fn wait_for_exit(&self, grace: Duration) -> Result<ExitCode> {
        let Some(mut child) = self.child.lock().await.take() else {
            return Ok(ExitCode::from(KILLED_EXIT_CODE));
        };
        let status = futures::select! {
            status = child.status().fuse() => status?,
            _ = Timer::after(grace).fuse() => {
                log::warn!(
                    "Harness process `{}` (pid {}) did not exit within {grace:?}; killing",
                    self.program,
                    self.pid
                );
                self.kill_child(&mut child);
                child.status().await?
            }
        };
        Ok(ExitCode::from(status.code().unwrap_or(KILLED_EXIT_CODE)))
    }

    /// Kills the process group immediately without waiting for it to be reaped.
    pub(crate) async fn kill(&self) {
        if let Some(mut child) = self.child.lock().await.take() {
            self.kill_child(&mut child);
        }
    }

    fn kill_child(&self, child: &mut Child) {
        #[cfg(unix)]
        kill_process_group(self.pid);
        if let Err(error) = child.kill() {
            log::debug!(
                "Killing harness process `{}` (pid {}) after group kill: {error}",
                self.program,
                self.pid
            );
        }
    }
}

#[cfg(test)]
#[path = "child_process_tests.rs"]
mod tests;
