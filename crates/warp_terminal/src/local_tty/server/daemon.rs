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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, SystemTime};

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

/// How long a write to an attached client may block before the client is considered stuck and
/// detached. Output is relayed while holding the session's output lock, so without a bound a
/// client that stops reading would freeze the session and every request that touches it.
const ATTACHMENT_WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a killed session may ignore SIGHUP before it is killed outright.
const KILL_GRACE_PERIOD: Duration = Duration::from_secs(2);

/// Sessions no client has been attached to for this long are killed. Reattaching is meant for
/// an app restart or crash; without a limit, sessions that no pane will ever claim again (e.g.
/// the app was never reopened, or session restoration lost them) would run forever.
const ORPHANED_SESSION_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const ORPHAN_CHECK_INTERVAL: Duration = Duration::from_secs(10 * 60);

/// How long a starting daemon waits for a previous one that is shutting down.
const INSTANCE_LOCK_TIMEOUT: Duration = Duration::from_secs(2);

/// Returns the path of the socket the daemon for the current channel and user listens on.
pub fn socket_path() -> PathBuf {
    let channel = ChannelState::channel();
    if let Some(runtime_dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(runtime_dir)
            .join(format!("warp-pty-{channel}-v{PROTOCOL_VERSION}.sock"));
    }
    // macOS gives every user their own temporary directory.
    if cfg!(target_os = "macos") {
        return std::env::temp_dir().join(format!("warp-pty-{channel}-v{PROTOCOL_VERSION}.sock"));
    }
    // Elsewhere it is usually a shared `/tmp`, so keep users apart.
    let uid = unsafe { libc::geteuid() };
    std::env::temp_dir().join(format!("warp-pty-{uid}-{channel}-v{PROTOCOL_VERSION}.sock"))
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

    // A second daemon (e.g. two app processes spawning one at once) would replace the first
    // one's socket, leaving the first one's sessions running but unreachable.
    let Some(_instance_lock) = acquire_instance_lock(&socket_path.with_extension("lock")) else {
        log::info!("Another pty daemon is running for this socket; exiting.");
        return;
    };

    let _ = std::fs::remove_file(socket_path);
    // Create the socket owner-only from the start, rather than restricting it after `bind`.
    let previous_umask = unsafe { libc::umask(0o077) };
    let listener = UnixListener::bind(socket_path);
    unsafe { libc::umask(previous_umask) };
    let listener = listener.expect("should be able to bind daemon socket");
    std::fs::set_permissions(socket_path, std::fs::Permissions::from_mode(0o600))
        .expect("should be able to restrict daemon socket permissions");
    log::info!("Pty daemon listening on {}", socket_path.display());

    let daemon = Arc::new(Daemon {
        socket_path: socket_path.to_owned(),
        state: Default::default(),
    });
    let reaper = daemon.clone();
    thread::spawn(move || {
        loop {
            thread::sleep(ORPHAN_CHECK_INTERVAL);
            reaper.kill_orphaned_sessions();
        }
    });
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                // Counted before the connection is served, so that `exit_if_idle` can't shut
                // the daemon down under a client that just connected.
                daemon.state.lock().clients += 1;
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
    /// Set once the shell has been reaped, after which its pid may belong to another process.
    exited: AtomicBool,
    /// `None` for sessions that no pane can reattach to; those are killed on detach.
    uuid: Option<String>,
    shell_session_id: SessionId,
    leader: File,
    output: Mutex<Output>,
}

struct Output {
    log: OutputLog,
    attachment: Option<Attachment>,
    /// When the last client detached, while none is attached. Wall-clock time, so that time
    /// the machine spends asleep counts.
    detached_since: Option<SystemTime>,
}

impl Output {
    /// Detaches the current client, if any, closing its relay socket so that it notices.
    fn detach_client(&mut self) {
        if let Some(attachment) = self.attachment.take() {
            let _ = attachment.socket.shutdown(std::net::Shutdown::Both);
            self.detached_since = Some(SystemTime::now());
        }
    }
}

struct Attachment {
    id: u64,
    socket: UnixStream,
    /// Whether the recorded output is still being sent; live output is held back meanwhile.
    replaying: bool,
}

static NEXT_ATTACHMENT_ID: AtomicU64 = AtomicU64::new(0);

impl Daemon {
    /// Serves a client connection, which the accept loop has already counted in `clients`.
    fn serve(self: Arc<Self>, stream: UnixStream) {
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
            exited: AtomicBool::new(false),
            output: Mutex::new(Output {
                log: OutputLog::default(),
                attachment: None,
                detached_since: None,
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
            let status = child
                .wait()
                .map_or(ObservedExitStatus::Unavailable, observed_exit_status);
            session.exited.store(true, Ordering::Release);
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

    /// Kills the sessions no client has been attached to for [`ORPHANED_SESSION_TIMEOUT`].
    fn kill_orphaned_sessions(&self) {
        let sessions = self
            .state
            .lock()
            .sessions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for session in sessions {
            let orphaned = session
                .output
                .lock()
                .detached_since
                .and_then(|since| since.elapsed().ok())
                .is_some_and(|elapsed| elapsed >= ORPHANED_SESSION_TIMEOUT);
            if orphaned {
                log::info!(
                    "Killing session with pid {} left unattached for too long",
                    session.pid
                );
                session.kill();
            }
        }
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
        // Applies to every clone of `socket`: they share the underlying socket.
        socket
            .set_write_timeout(Some(ATTACHMENT_WRITE_TIMEOUT))
            .context("Failed to set relay socket write timeout")?;
        let id = NEXT_ATTACHMENT_ID.fetch_add(1, Ordering::Relaxed);

        let mut output = self.output.lock();
        let replay = replay.then(|| (output.log.snapshot(), output.log.total));
        // With nothing recorded there is nothing to replay, so input isn't muted either: the
        // client would never finish a replay of zero bytes and so never unmute.
        let has_recording = replay
            .as_ref()
            .is_some_and(|(recorded, _)| !recorded.is_empty());
        output.detach_client();
        output.attachment = Some(Attachment {
            id,
            socket: socket.try_clone()?,
            replaying: has_recording,
        });
        output.detached_since = None;
        drop(output);

        let mut attach_info = None;
        let mut unmute_token = None;
        if replay.is_some() && !has_recording {
            attach_info = Some(PtyAttachInfo {
                replay_len: 0,
                unmute_token: Vec::new(),
                shell_session_id: self.shell_session_id,
            });
        }
        if let Some((recorded, recorded_total)) = replay.filter(|_| has_recording) {
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
        let replayed = socket.write_all(&recorded).is_ok();
        let mut output = self.output.lock();
        let Output {
            log, attachment, ..
        } = &mut *output;
        let Some(attachment) = attachment.as_mut().filter(|a| a.id == id) else {
            return;
        };
        attachment.replaying = false;
        // On failure, detach rather than leave the attachment "replaying" forever, which
        // would hold back its live output.
        if !replayed
            || attachment
                .socket
                .write_all(&log.since(recorded_total))
                .is_err()
        {
            output.detach_client();
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
                log::info!(
                    "Detaching unresponsive client from session with pid {}",
                    self.pid
                );
                output.detach_client();
            }
        }
    }

    fn relay_input(
        self: &Arc<Self>,
        id: u64,
        mut socket: UnixStream,
        mut unmute_token: Option<Vec<u8>>,
    ) {
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
            output.detach_client();
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

    /// Hangs the session up the way closing a terminal window does: SIGHUP lets the shell
    /// hang up its jobs too (a SIGKILLed shell would leave background jobs running, holding the
    /// pty open), and a shell still running after [`KILL_GRACE_PERIOD`] is killed outright.
    fn kill(self: &Arc<Self>) {
        if self.exited.load(Ordering::Acquire) {
            return;
        }
        unsafe {
            let pgrp = libc::tcgetpgrp(self.leader.as_raw_fd());
            if pgrp > 0 {
                libc::killpg(pgrp, libc::SIGHUP);
            }
            libc::kill(self.pid as libc::pid_t, libc::SIGHUP);
            // Resume a stopped shell so that it can act on the SIGHUP.
            libc::kill(self.pid as libc::pid_t, libc::SIGCONT);
        }
        let session = self.clone();
        thread::spawn(move || {
            thread::sleep(KILL_GRACE_PERIOD);
            if !session.exited.load(Ordering::Acquire) {
                unsafe {
                    libc::kill(session.pid as libc::pid_t, libc::SIGKILL);
                }
            }
        });
    }
}

/// Takes the lock that keeps a single daemon per socket, waiting briefly for a daemon that is
/// shutting down. Returns `None` while another daemon holds it. The lock lasts as long as the
/// returned file stays open.
fn acquire_instance_lock(path: &Path) -> Option<File> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|err| log::warn!("Failed to open pty daemon lock file: {err:#}"))
        .ok()?;
    let start = SystemTime::now();
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Some(file);
        }
        if start.elapsed().unwrap_or_default() >= INSTANCE_LOCK_TIMEOUT {
            return None;
        }
        thread::sleep(Duration::from_millis(50));
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
