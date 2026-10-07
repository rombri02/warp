//! A detached process that owns ptys, so that shell sessions (and whatever runs
//! in them, such as CLI coding agents) survive the app quitting or crashing.
//!
//! The daemon speaks the same [`api`] as the terminal server, but instead of
//! handing the pty leader to the app it relays the session through a socket
//! pair, recording the output.  When a restored pane asks to spawn a shell
//! for a session that is still running, the daemon replays the recording and
//! resumes relaying instead.  Sessions are keyed by the pane UUID that the app
//! passes to every shell (see [`session_uuid_from_env_vars`]).

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::prelude::*;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rand::RngCore;
use warp_core::channel::ChannelState;
use warp_core::session_id::SessionId;

use super::{api, protocol};
use crate::event::ObservedExitStatus;
use crate::focus_env::session_uuid_from_env_vars;
use crate::local_tty::{self, PtyAttachInfo, PtyOptions, PtySpawnResult};

/// Bump whenever [`api`] changes, so that a newer app never talks to an older daemon.
const PROTOCOL_VERSION: u32 = 1;

/// Output recorded from the start of a session, which includes the shell bootstrap
/// that the app needs to replay to understand the session.
const LOG_HEAD_CAPACITY: usize = 1 << 20;
/// Most recent output recorded after the head fills up.
const LOG_TAIL_CAPACITY: usize = 7 << 20;

/// How long to wait for a session's remaining output after its shell exits.
const DRAIN_TIMEOUT: Duration = Duration::from_millis(500);

/// Returns the path of the socket the daemon for the current channel listens on.
pub fn socket_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join(format!(
            "warp-pty-{}-v{PROTOCOL_VERSION}.sock",
            ChannelState::channel()
        ))
}

/// Runs the pty daemon, listening on `socket_path`, until it has neither sessions
/// nor clients left.
pub fn run_pty_daemon(
    socket_path: &Path,
    init_feature_flags: impl FnOnce(),
    init_platform: impl FnOnce() -> anyhow::Result<()>,
) {
    init_feature_flags();
    log::set_boxed_logger(Box::new(StderrLogger)).expect("should not fail to set logger");
    log::set_max_level(log::LevelFilter::Info);
    init_platform().expect("should not fail to perform platform-level terminal initialization");

    let _ = std::fs::remove_file(socket_path);
    let listener = UnixListener::bind(socket_path).expect("should be able to bind daemon socket");
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        .expect("should be able to restrict daemon socket permissions");
    log::info!("Pty daemon listening on {}", socket_path.display());

    let daemon = Arc::new(Daemon {
        socket_path: socket_path.to_owned(),
        state: Default::default(),
    });
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let daemon = daemon.clone();
                thread::spawn(move || daemon.serve(stream));
            }
            Err(err) => log::warn!("Failed to accept pty daemon connection: {err:#}"),
        }
    }
}

struct Daemon {
    socket_path: PathBuf,
    state: Mutex<DaemonState>,
}

#[derive(Default)]
struct DaemonState {
    /// Live sessions, keyed by shell process ID.
    sessions: HashMap<u32, Arc<Session>>,
    /// Connections that receive fire-and-forget messages.
    subscribers: Vec<UnixStream>,
    /// Number of connected request/response clients.
    clients: usize,
}

struct Session {
    pid: u32,
    /// `None` for sessions that no pane can reattach to; those are killed on detach.
    uuid: Option<String>,
    shell_session_id: SessionId,
    leader: File,
    output: Mutex<Output>,
}

struct Output {
    log: OutputLog,
    attachment: Option<Attachment>,
}

struct Attachment {
    id: u64,
    socket: UnixStream,
    /// Whether the recorded output is still being sent; live output is held back meanwhile.
    replaying: bool,
}

static NEXT_ATTACHMENT_ID: AtomicU64 = AtomicU64::new(0);

impl Daemon {
    fn serve(self: Arc<Self>, stream: UnixStream) {
        self.state.lock().clients += 1;
        loop {
            let message = match protocol::receive_message(stream.as_fd()) {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(err) => {
                    log::warn!("Failed to receive message from pty daemon client: {err:#}");
                    break;
                }
            };
            if let api::Message::SubscribeRequest = message {
                let mut state = self.state.lock();
                state.clients -= 1;
                state.subscribers.push(stream);
                return;
            }
            if let Err(err) = self.handle(&stream, message) {
                log::warn!("Failed to respond to pty daemon client: {err:#}");
                break;
            }
        }
        self.state.lock().clients -= 1;
        self.exit_if_idle();
    }

    fn handle(self: &Arc<Self>, stream: &UnixStream, message: api::Message) -> Result<()> {
        match message {
            api::Message::SpawnShellRequest { options } => {
                let (spawn_result, fds) = match self.spawn_or_attach(*options) {
                    Ok((result, session)) => {
                        let fds = vec![result.leader_fd, session.leader.as_raw_fd()];
                        (Ok(result), fds)
                    }
                    Err(err) => (Err(err), vec![]),
                };
                let sent = protocol::send_message(
                    stream.as_fd(),
                    api::Message::SpawnShellResponse {
                        spawn_result: spawn_result.into(),
                    },
                    &fds,
                );
                // The client holds its own copy of its end of the relay socket pair.
                if let Some(client_socket) = fds.first() {
                    drop(unsafe { OwnedFd::from_raw_fd(*client_socket) });
                }
                sent?;
            }
            api::Message::KillChildRequest { pid } => {
                let session = self.state.lock().sessions.get(&pid).cloned();
                if let Some(session) = session {
                    log::info!("Killing session with pid {pid} at client request");
                    session.kill();
                }
                protocol::send_message(
                    stream.as_fd(),
                    api::Message::KillChildResponse { error_msg: None },
                    &[],
                )?;
            }
            api::Message::HasSessionRequest { session_uuid } => {
                let exists = self.find_session(&session_uuid).is_some();
                protocol::send_message(
                    stream.as_fd(),
                    api::Message::HasSessionResponse { exists },
                    &[],
                )?;
            }
            api::Message::RetainSessionsRequest { session_uuids } => {
                let sessions = self
                    .state
                    .lock()
                    .sessions
                    .values()
                    .cloned()
                    .collect::<Vec<_>>();
                for session in sessions {
                    let is_retained = session
                        .uuid
                        .as_ref()
                        .is_some_and(|uuid| session_uuids.contains(uuid));
                    if !is_retained && session.output.lock().attachment.is_none() {
                        log::info!("Killing unclaimed session with pid {}", session.pid);
                        session.kill();
                    }
                }
                protocol::send_message(stream.as_fd(), api::Message::RetainSessionsResponse, &[])?;
            }
            api::Message::SpawnShellResponse { .. }
            | api::Message::KillChildResponse { .. }
            | api::Message::WriteLogRequest { .. }
            | api::Message::ChildrenTerminatedRequest { .. }
            | api::Message::HasSessionResponse { .. }
            | api::Message::RetainSessionsResponse
            | api::Message::SubscribeRequest => {
                log::error!("Pty daemon received unexpected message: {message:?}");
            }
        }
        Ok(())
    }

    fn find_session(&self, uuid: &str) -> Option<Arc<Session>> {
        self.state
            .lock()
            .sessions
            .values()
            .find(|session| session.uuid.as_deref() == Some(uuid))
            .cloned()
    }

    /// Returns the spawn result to send to the client, whose `leader_fd` is the
    /// client's end of a fresh relay socket pair.
    fn spawn_or_attach(
        self: &Arc<Self>,
        mut options: PtyOptions,
    ) -> Result<(PtySpawnResult, Arc<Session>)> {
        let uuid = session_uuid_from_env_vars(&options.env_vars).map(str::to_owned);

        if let Some(session) = uuid.as_deref().and_then(|uuid| self.find_session(uuid)) {
            log::info!("Reattaching to session with pid {}", session.pid);
            let (client_socket, attach) = session.attach(true)?;
            return Ok((
                PtySpawnResult {
                    pid: session.pid,
                    leader_fd: client_socket.into_raw_fd(),
                    ioctl_fd: None,
                    attach,
                },
                session,
            ));
        }

        // Unlike the terminal server, the daemon holds other sessions' leaders
        // and client sockets, which must not leak into new shells.
        options.close_fds = true;
        let shell_session_id = options.shell_starter.session_id();
        let spawned = local_tty::spawn(options)?;
        let pid = spawned.child.id();
        log::info!("Spawned session with pid {pid}");
        let session = Arc::new(Session {
            pid,
            uuid,
            shell_session_id,
            leader: unsafe { File::from_raw_fd(spawned.result.leader_fd) },
            output: Mutex::new(Output {
                log: OutputLog::default(),
                attachment: None,
            }),
        });
        let (client_socket, _) = session.attach(false)?;
        self.state.lock().sessions.insert(pid, session.clone());
        self.start_session_threads(session.clone(), spawned.child);
        Ok((
            PtySpawnResult {
                pid,
                leader_fd: client_socket.into_raw_fd(),
                ioctl_fd: None,
                attach: None,
            },
            session,
        ))
    }

    fn start_session_threads(self: &Arc<Self>, session: Arc<Session>, mut child: Child) {
        let (drained_tx, drained_rx) = mpsc::channel();
        let reader_session = session.clone();
        thread::spawn(move || {
            reader_session.relay_output();
            let _ = drained_tx.send(());
        });

        let daemon = self.clone();
        thread::spawn(move || {
            let status = child.wait().map_or(ObservedExitStatus::Unavailable, observed_exit_status);
            // Background jobs may keep the pty open after the shell exits, so
            // don't wait forever for the output to drain.
            let _ = drained_rx.recv_timeout(DRAIN_TIMEOUT);
            log::info!("Session with pid {} exited", session.pid);
            if let Some(attachment) = session.output.lock().attachment.take() {
                let _ = attachment.socket.shutdown(std::net::Shutdown::Both);
            }
            daemon.session_exited(session.pid, status);
        });
    }

    fn session_exited(&self, pid: u32, status: ObservedExitStatus) {
        {
            let mut state = self.state.lock();
            state.sessions.remove(&pid);
            state.subscribers.retain(|subscriber| {
                protocol::send_message(
                    subscriber.as_fd(),
                    api::Message::ChildrenTerminatedRequest {
                        children: vec![(pid, status)],
                    },
                    &[],
                )
                .is_ok()
            });
        }
        self.exit_if_idle();
    }

    fn exit_if_idle(&self) {
        let state = self.state.lock();
        if state.sessions.is_empty() && state.clients == 0 {
            log::info!("No sessions or clients left; shutting down pty daemon.");
            let _ = std::fs::remove_file(&self.socket_path);
            std::process::exit(0);
        }
    }
}

impl Session {
    /// Attaches a new client, replacing any existing one.  When `replay` is set,
    /// the recorded output is sent first and input stays muted until the client
    /// sends the returned unmute token.
    fn attach(self: &Arc<Self>, replay: bool) -> Result<(UnixStream, Option<PtyAttachInfo>)> {
        let (socket, client_socket) =
            UnixStream::pair().context("Failed to create relay socket pair")?;
        let id = NEXT_ATTACHMENT_ID.fetch_add(1, Ordering::Relaxed);

        let mut output = self.output.lock();
        let replay = replay.then(|| (output.log.snapshot(), output.log.total));
        if let Some(previous) = output.attachment.replace(Attachment {
            id,
            socket: socket.try_clone()?,
            replaying: replay.is_some(),
        }) {
            let _ = previous.socket.shutdown(std::net::Shutdown::Both);
        }
        drop(output);

        let mut attach_info = None;
        let mut unmute_token = None;
        if let Some((recorded, recorded_total)) = replay {
            let mut token = vec![0; 16];
            rand::thread_rng().fill_bytes(&mut token);
            // Wrapped in an APC sequence so it is inert if it ever reaches the shell.
            let token = format!("\x1b_warp-pty-unmute-{}\x1b\\", hex::encode(token)).into_bytes();
            attach_info = Some(PtyAttachInfo {
                replay_len: recorded.len(),
                unmute_token: token.clone(),
                shell_session_id: self.shell_session_id,
            });
            unmute_token = Some(token);
            let session = self.clone();
            let socket = socket.try_clone()?;
            thread::spawn(move || session.replay(id, socket, recorded, recorded_total));
        }

        let session = self.clone();
        thread::spawn(move || session.relay_input(id, socket, unmute_token));
        Ok((client_socket, attach_info))
    }

    fn replay(&self, id: u64, mut socket: UnixStream, recorded: Vec<u8>, recorded_total: usize) {
        if socket.write_all(&recorded).is_err() {
            return;
        }
        let mut output = self.output.lock();
        let since = output.log.since(recorded_total);
        if let Some(attachment) = output.attachment.as_mut().filter(|a| a.id == id) {
            attachment.replaying = false;
            if attachment.socket.write_all(&since).is_err() {
                output.attachment = None;
            }
        }
    }

    fn relay_output(&self) {
        let mut buf = vec![0; 0x1_0000];
        loop {
            let len = match (&self.leader).read(&mut buf) {
                Ok(0) => return,
                Ok(len) => len,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                // On Linux, reading the leader fails with EIO once the follower closes.
                Err(_) => return,
            };
            let mut output = self.output.lock();
            output.log.push(&buf[..len]);
            if let Some(attachment) = output.attachment.as_mut().filter(|a| !a.replaying)
                && attachment.socket.write_all(&buf[..len]).is_err()
            {
                output.attachment = None;
            }
        }
    }

    fn relay_input(&self, id: u64, mut socket: UnixStream, mut unmute_token: Option<Vec<u8>>) {
        let mut buf = vec![0; 0x1_0000];
        let mut muted_input = Vec::new();
        loop {
            let len = match socket.read(&mut buf) {
                Ok(0) => break,
                Ok(len) => len,
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            };
            let mut input = &buf[..len];
            if let Some(token) = &unmute_token {
                muted_input.extend_from_slice(input);
                let Some(token_start) = find(&muted_input, token) else {
                    let keep_from = muted_input.len().saturating_sub(token.len() - 1);
                    muted_input.drain(..keep_from);
                    continue;
                };
                muted_input.drain(..token_start + token.len());
                unmute_token = None;
                input = &muted_input;
                self.request_redraw();
            }
            if (&self.leader).write_all(input).is_err() {
                break;
            }
            muted_input.clear();
        }

        let mut output = self.output.lock();
        if output.attachment.as_ref().is_some_and(|a| a.id == id) {
            output.attachment = None;
            drop(output);
            log::info!("Client detached from session with pid {}", self.pid);
            if self.uuid.is_none() {
                self.kill();
            }
        }
    }

    /// Asks the foreground process to redraw, now that the client has caught up on
    /// the recorded output.  Full-screen programs repaint on SIGWINCH.
    fn request_redraw(&self) {
        unsafe {
            let pgrp = libc::tcgetpgrp(self.leader.as_raw_fd());
            if pgrp > 0 {
                libc::killpg(pgrp, libc::SIGWINCH);
            }
        }
    }

    fn kill(&self) {
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGKILL);
        }
    }
}

fn observed_exit_status(status: std::process::ExitStatus) -> ObservedExitStatus {
    use std::os::unix::process::ExitStatusExt;
    status
        .code()
        .map(ObservedExitStatus::Code)
        .or_else(|| status.signal().map(ObservedExitStatus::Signal))
        .unwrap_or(ObservedExitStatus::Unavailable)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// The output of a session: its first [`LOG_HEAD_CAPACITY`] bytes plus its most
/// recent [`LOG_TAIL_CAPACITY`] bytes.
// ponytail: replaying across the gap between head and tail can split an escape
// sequence; the redraw after replay repairs the screen but not older scrollback.
#[derive(Default)]
struct OutputLog {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    /// Total number of bytes ever pushed.
    total: usize,
}

impl OutputLog {
    fn push(&mut self, mut bytes: &[u8]) {
        self.total += bytes.len();
        let head_len = (LOG_HEAD_CAPACITY - self.head.len()).min(bytes.len());
        self.head.extend_from_slice(&bytes[..head_len]);
        bytes = &bytes[head_len..];
        self.tail.extend(bytes);
        let excess = self.tail.len().saturating_sub(LOG_TAIL_CAPACITY);
        self.tail.drain(..excess);
    }

    fn snapshot(&self) -> Vec<u8> {
        self.head.iter().chain(self.tail.iter()).copied().collect()
    }

    /// Returns the recorded bytes pushed after the first `offset` bytes.
    fn since(&self, offset: usize) -> Vec<u8> {
        let tail_len = (self.total - offset).min(self.tail.len());
        let head = self.head.get(offset..).unwrap_or_default();
        head.iter()
            .chain(self.tail.iter().skip(self.tail.len() - tail_len))
            .copied()
            .collect()
    }
}

/// Logs to stderr, which the client redirects to a file when spawning the daemon.
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        eprintln!(
            "[{}] {}: {}",
            record.level(),
            record.target(),
            record.args()
        );
    }

    fn flush(&self) {}
}

#[cfg(test)]
#[path = "daemon_tests.rs"]
mod tests;
