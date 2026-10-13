use anyhow::Result;
#[cfg(unix)]
use warp_errors::report_error;
use warpui_core::{AppContext, Entity, SingletonEntity};
#[cfg(unix)]
use {
    crate::local_tty::server::TerminalServer, anyhow::bail, std::cmp::Reverse,
    std::collections::HashMap, std::ffi::OsString, std::process::Child,
    warp_core::features::FeatureFlag,
};

#[cfg(target_os = "windows")]
use super::PseudoConsoleChild;
use super::{PtyOptions, PtySpawnResult};
use crate::event::ObservedExitStatus;
use crate::local_tty::{self};

#[derive(Clone, Copy, Debug)]
pub enum PtySpawnMode {
    TerminalServer,
    FallbackToDirect,
    Direct,
}

pub trait PtySpawnHooks {
    fn before_spawn(&self);
    fn after_spawn(&self);
    fn spawned(&self, mode: PtySpawnMode, ctx: &mut AppContext);
}
/// A handle that can be used to interact with a pty process.
pub trait PtyHandle: Send + Sync {
    /// Returns the pty's process ID.
    fn pid(&self) -> u32;

    /// Returns the observed status when the child has terminated. This may
    /// return `None` for an exited child (e.g. before a server-hosted PTY
    /// reports it), but never returns a status for a living child.
    fn process_termination_status(&mut self) -> Result<Option<ObservedExitStatus>>;

    /// Kills the pty process and waits for its successful termination.
    fn kill(&mut self) -> Result<()>;
}

/// A handle for a pty that is a direct child of the current process.
#[cfg(unix)]
struct DirectPtyHandle {
    child: Child,
}

#[cfg(unix)]
impl PtyHandle for DirectPtyHandle {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn process_termination_status(&mut self) -> Result<Option<ObservedExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        self.child
            .try_wait()
            .map(|status| {
                status.map(|status| {
                    status
                        .code()
                        .map(ObservedExitStatus::Code)
                        .or_else(|| status.signal().map(ObservedExitStatus::Signal))
                        .unwrap_or(ObservedExitStatus::Unavailable)
                })
            })
            .map_err(anyhow::Error::from)
    }

    fn kill(&mut self) -> Result<()> {
        self.child.kill()?;
        match self.child.wait() {
            Ok(_) => Ok(()),
            Err(err) => bail!(err),
        }
    }
}

#[cfg(target_os = "windows")]
struct DirectPtyHandle {
    child: PseudoConsoleChild,
}

#[cfg(target_os = "windows")]
impl PtyHandle for DirectPtyHandle {
    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn process_termination_status(&mut self) -> Result<Option<ObservedExitStatus>> {
        Ok(self
            .child
            .is_terminated()
            .then_some(ObservedExitStatus::Unavailable))
    }

    fn kill(&mut self) -> Result<()> {
        self.child.kill()
    }
}
/// Invokes the provided callback function without crash reporting enabled.
fn invoke_without_crash_reporting<T>(hooks: &dyn PtySpawnHooks, func: impl FnOnce() -> T) -> T {
    // Uninitialize cocoa-sentry before spawning the shell process to avoid passing any custom state
    // (such as BSD signal handlers and mach exception handlers) into the shell process. This means
    // we lose all Cocoa crash reports from now until when the session is successfully spawned,
    // which is not ideal but allows us to fully ensure that we don't improperly leak any Sentry state
    // into the child processes.
    hooks.before_spawn();

    let retval = func();

    // Now that the child has spawned--reinitialize cocoa sentry.
    hooks.after_spawn();

    retval
}

pub(super) struct PtySpawnInfo {
    pub result: PtySpawnResult,
    #[cfg(unix)]
    pub child: Child,
    #[cfg(windows)]
    pub child: PseudoConsoleChild,
}

/// A global singleton that provides the ability to spawn ptys.
///
/// This abstracts away from callers the manner in which the pty is spawned -
/// depending on configuration, the pty might be spawned as a child of the
/// current process, or it may be spawned by a subprocess that is responsible
/// for owning and managing ptys.
pub struct PtySpawner {
    #[cfg(unix)]
    server: Option<TerminalServer>,
}

impl PtySpawner {
    /// Creates a new PtySpawner.
    ///
    /// This should be called extremely early in the application startup
    /// process - we want to minimize the number of already-obtained resources
    /// that could leak into forked subprocesses (e.g.: file descriptors).
    pub fn new() -> Result<Self> {
        cfg_if::cfg_if! {
            if #[cfg(unix)] {
                let daemon = FeatureFlag::PersistentPtyDaemon
                    .is_enabled()
                    .then(TerminalServer::connect_daemon)
                    .and_then(|result| {
                        result
                            .map_err(|err| {
                                report_error!(err.context(
                                    "Failed to connect to pty daemon; falling back to terminal server"
                                ))
                            })
                            .ok()
                    });
                if !FeatureFlag::PersistentPtyDaemon.is_enabled() {
                    TerminalServer::retire_daemon();
                }
                let server = match daemon {
                    Some(daemon) => daemon,
                    None => TerminalServer::new()?,
                };
                Ok(Self {
                    server: Some(server),
                })
            } else if #[cfg(target_os = "windows")] {
                Ok(Self {})
            } else {
                unreachable!("Spawning a PTY is not supported on this platform.");
            }
        }
    }

    /// Creates a new PtySpanwer that is configured for unit test purposes.
    pub fn new_for_test() -> Self {
        cfg_if::cfg_if! {
            if #[cfg(unix)] {
                Self{ server: None }
            } else if #[cfg(target_os = "windows")] {
                Self {}
            } else {
                unreachable!("Spawning a PTY for tests is not supported on this platform.");
            }
        }
    }

    /// Does any work necessary to clean up state in advance of the app
    /// terminating.
    pub fn prepare_for_app_termination(&mut self) {
        // Drop the backing `TerminalServer`, if one exists, killing the child
        // process.
        #[cfg(unix)]
        if let Some(server) = self.server.take() {
            if server.is_daemon() {
                log::info!("Detaching from pty daemon...");
                server.client().detach();
            } else {
                log::info!("Tearing down terminal server...");
            }
            drop(server);
        }
    }

    /// Returns whether the pty daemon is still running a session for the pane
    /// with the given UUID, which spawning a pty for that pane will reattach to.
    pub fn has_live_session(&self, session_uuid_hex: &str) -> bool {
        #[cfg(unix)]
        if let Some(server) = self.server.as_ref().filter(|server| server.is_daemon()) {
            return server
                .client()
                .has_session(session_uuid_hex.to_owned())
                .unwrap_or_else(|err| {
                    report_error!(err.context("Failed to query pty daemon for session"));
                    false
                });
        }
        let _ = session_uuid_hex;
        false
    }

    /// Kills detached pty daemon sessions, except those for the panes with the
    /// given UUIDs, which are about to reattach.
    pub fn retain_sessions(&self, session_uuids: impl IntoIterator<Item = Vec<u8>>) {
        #[cfg(unix)]
        if let Some(server) = self.server.as_ref().filter(|server| server.is_daemon())
            && let Err(err) = server
                .client()
                .retain_sessions(session_uuids.into_iter().map(hex::encode).collect())
        {
            report_error!(err.context("Failed to clean up detached pty daemon sessions"));
        }
        #[cfg(not(unix))]
        let _ = session_uuids;
    }

    /// Spawns a pty, returning information about the pty and a handle that can
    /// be used to interact with the pty process.
    pub(super) fn spawn_pty(
        &self,
        options: PtyOptions,
        hooks: &dyn PtySpawnHooks,
        #[cfg(windows)] event_loop_tx: super::mio_channel::Sender<crate::writeable_pty::Message>,
        ctx: &mut AppContext,
    ) -> Result<(PtySpawnResult, Box<dyn PtyHandle>)> {
        #[cfg(not(unix))]
        let is_fallback = false;
        #[cfg(unix)]
        let mut is_fallback = false;

        #[cfg(unix)]
        if let Some(server) = &self.server {
            let result = Self::spawn_pty_via_server(server, options.clone());
            if let Err(err) = result {
                // Log env var names + sizes for any terminal server failure.
                // Large env vars are the most common cause (E2BIG on Linux,
                // socket overflow on macOS), so logging them on every failure
                // makes both cases diagnosable from the logs.
                log_env_var_diagnostics(&options.env_vars);

                // E2BIG is deterministic — retrying from the main process with
                // the same PtyOptions would hit the same limit — so fail
                // immediately rather than falling back.
                if is_e2big(&err) {
                    // err already contains the original message. We add some context on how to fix.
                    return Err(err.context(
                        "This can happen when env vars in the image or Oz secrets are \
                         too long.",
                    ));
                }
                report_error!(err.context(
                    "Failed to spawn pty via terminal server; falling back to spawning locally...",
                ));
                is_fallback = true;
            } else {
                hooks.spawned(PtySpawnMode::TerminalServer, ctx);
                return result;
            }
        }

        let mode = if is_fallback {
            PtySpawnMode::FallbackToDirect
        } else {
            PtySpawnMode::Direct
        };
        hooks.spawned(mode, ctx);

        Self::spawn_pty_directly(
            options,
            #[cfg(windows)]
            event_loop_tx,
            hooks,
        )
    }

    fn spawn_pty_directly(
        options: PtyOptions,
        #[cfg(windows)] event_loop_tx: super::mio_channel::Sender<crate::writeable_pty::Message>,
        hooks: &dyn PtySpawnHooks,
    ) -> Result<(PtySpawnResult, Box<dyn PtyHandle>)> {
        let pty_spawn_info = invoke_without_crash_reporting(hooks, move || {
            local_tty::spawn(
                options,
                #[cfg(windows)]
                event_loop_tx,
            )
        })?;
        let direct_pty_handle = Box::new(DirectPtyHandle {
            child: pty_spawn_info.child,
        });
        Ok((pty_spawn_info.result, direct_pty_handle))
    }

    #[cfg(unix)]
    fn spawn_pty_via_server(
        server: &TerminalServer,
        options: PtyOptions,
    ) -> Result<(PtySpawnResult, Box<dyn PtyHandle>)> {
        use crate::local_tty::server::ServerOwnedPtyHandle;

        let client = server.client().clone();
        let result = client.spawn_pty(options)?;
        let handle = Box::new(ServerOwnedPtyHandle {
            pid: result.pid,
            client,
        });
        Ok((result, handle))
    }
}

impl Entity for PtySpawner {
    type Event = ();
}

impl SingletonEntity for PtySpawner {}

/// Returns `true` if the error (or any cause in its chain) indicates that
/// `execve` failed with `ENAMETOOLONG` / E2BIG ("Argument list too long").
///
/// The terminal-server IPC path stringifies the raw `io::Error`, so we cannot
/// reliably downcast; instead we check the formatted error message as a
/// fallback.
#[cfg(unix)]
fn is_e2big(err: &anyhow::Error) -> bool {
    // Try the "real" io::Error in the chain first (direct-spawn path).
    if err.chain().any(|e| {
        e.downcast_ref::<std::io::Error>()
            .and_then(|e| e.raw_os_error())
            .is_some_and(|code| code == libc::E2BIG)
    }) {
        return true;
    }
    // Fall back to string matching for the IPC path where the io::Error
    // was serialised to a String before being sent back to the client.
    let msg = format!("{err:#}");
    msg.contains("os error 7") || msg.contains("Argument list too long")
}

/// Logs the names and byte-lengths (not values) of env vars passed to the
/// shell. Called on any terminal server failure to aid diagnosis of oversized
/// env var / secret configurations (E2BIG on Linux, socket overflow on macOS).
#[cfg(unix)]
fn log_env_var_diagnostics(extra_env_vars: &HashMap<OsString, OsString>) {
    log::error!("Shell spawn env var diagnostics (names and sizes only, no values):");

    // Log the additional env vars supplied via PtyOptions.
    let mut extra: Vec<(&OsString, usize)> = extra_env_vars
        .iter()
        .map(|(k, v)| (k, k.len() + v.len() + 2))
        .collect();
    extra.sort_by_key(|(_, size)| Reverse(*size));
    log::error!("PtyOptions env_vars entries={}", extra_env_vars.len());
    for (key, size) in extra.iter().take(20) {
        log::error!("PtyOptions env var entry key={key:?} bytes={size}");
    }

    // Log the largest vars from the inherited process environment.
    let mut inherited: Vec<(OsString, usize)> = std::env::vars_os()
        .map(|(k, v)| {
            let size = k.len() + v.len() + 2;
            (k, size)
        })
        .collect();
    inherited.sort_by_key(|(_, size)| Reverse(*size));
    let total: usize = inherited.iter().map(|(_, s)| s).sum();
    log::error!(
        "Inherited process env summary vars={} bytes={total}",
        inherited.len()
    );
    for (key, size) in inherited.iter().take(20) {
        log::error!("Inherited process env entry key={key:?} bytes={size}");
    }
}
