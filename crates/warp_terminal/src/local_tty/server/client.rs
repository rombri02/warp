use std::collections::HashMap;
use std::os::unix::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use parking_lot::Mutex;

use super::{api, protocol};
use crate::event::ObservedExitStatus;
use crate::local_tty::{PtyOptions, PtySpawnResult};

/// A client for communicating with the terminal server.
///
/// This owns the client side file descriptor for the Unix domain socket which
/// we use to communicate with the terminal server.  This structure should be
/// passed around inside an [`Arc`](std::sync::Arc) so that it can be accessed
/// from various locations within the codebase.
pub struct TerminalServerClient {
    /// The file descriptor for the Unix domain socket that is connected to the
    /// terminal server.
    ///
    /// This is stored inside a Mutex so that the client can guarantee ownership
    /// of the socket across a send/receive pair (to avoid interference from
    /// other threads).
    socket_fd: Mutex<OwnedFd>,
    /// The set of process IDs of terminated children which have not yet been
    /// processed by the pty event loops.
    terminated_children: Arc<Mutex<HashMap<u32, ObservedExitStatus>>>,
    /// Once set, requests to kill children are ignored, so that a pty daemon
    /// keeps their sessions alive after this process exits.
    is_detaching: AtomicBool,
}

impl TerminalServerClient {
    /// Constructs a new terminal server client which communicates with the
    /// server via the provided Unix domain socket file descriptor and holds
    /// onto a list of terminated child process IDs.
    pub fn new(
        client_fd: OwnedFd,
        terminated_children: Arc<Mutex<HashMap<u32, ObservedExitStatus>>>,
    ) -> Self {
        Self {
            socket_fd: Mutex::new(client_fd),
            terminated_children,
            is_detaching: AtomicBool::new(false),
        }
    }

    /// Asks the server to spawn a pty, returning the pty leader file descriptor
    /// and other metadata upon success.
    pub fn spawn_pty(&self, options: PtyOptions) -> Result<PtySpawnResult> {
        // Lock access to the socket to ensure that nothing interferes with our
        // request/response handshake.
        let fd = self.socket_fd.lock();

        protocol::send_message(
            fd.as_fd(),
            api::Message::SpawnShellRequest {
                options: Box::new(options),
            },
            &[],
        )?;

        let result = protocol::receive_message(fd.as_fd())?;
        match result {
            Some(api::Message::SpawnShellResponse {
                spawn_result: api::Result::Ok(spawn_result),
            }) => Ok(spawn_result),
            Some(api::Message::SpawnShellResponse {
                spawn_result: api::Result::Err(message),
            }) => {
                bail!("Terminal server failed to spawn a shell: {message}");
            }
            Some(_) => {
                bail!(
                    "Got response message other than SpawnShellResponse after sending a SpawnShellRequest message!"
                );
            }
            None => {
                bail!("Received error reading message back from terminal server");
            }
        }
    }

    /// Asks the server to terminate and clean up its child process with the
    /// given process ID.
    pub fn kill_child(&self, pid: u32) -> Result<()> {
        if self.is_detaching.load(Ordering::Relaxed) || self.child_termination_status(pid).is_some()
        {
            return Ok(());
        }

        // Lock access to the socket to ensure that nothing interferes with our
        // request/response handshake.
        let fd = self.socket_fd.lock();

        if let Err(error) =
            protocol::send_message(fd.as_fd(), api::Message::KillChildRequest { pid }, &[])
        {
            if error.downcast_ref::<nix::Error>() == Some(&nix::Error::EPIPE) {
                log::warn!(
                    "Received EPIPE when trying to kill child shell process; assuming the terminal server has terminated."
                );
                return Ok(());
            } else {
                return Err(error);
            }
        }

        let result = protocol::receive_message(fd.as_fd())?;
        match result {
            Some(api::Message::KillChildResponse { error_msg }) => match error_msg {
                Some(error_msg) => Err(anyhow::anyhow!(error_msg)),
                None => Ok(()),
            },
            Some(_) => {
                bail!(
                    "Got response message other than KillChildResponse after sending a KillChildRequest message!"
                );
            }
            None => {
                bail!("Received error reading message back from terminal server");
            }
        }
    }

    /// Stops honoring requests to kill children, leaving them to the pty daemon.
    pub fn detach(&self) {
        self.is_detaching.store(true, Ordering::Relaxed);
    }

    /// Asks the pty daemon whether a session for the given pane UUID is still running.
    pub fn has_session(&self, session_uuid: String) -> Result<bool> {
        match self.request(api::Message::HasSessionRequest { session_uuid })? {
            api::Message::HasSessionResponse { exists } => Ok(exists),
            _ => bail!("Got unexpected response to a HasSessionRequest message!"),
        }
    }

    /// Asks the pty daemon to kill every session no client is attached to,
    /// except those for the given pane UUIDs.
    pub fn retain_sessions(&self, session_uuids: Vec<String>) -> Result<()> {
        match self.request(api::Message::RetainSessionsRequest { session_uuids })? {
            api::Message::RetainSessionsResponse => Ok(()),
            _ => bail!("Got unexpected response to a RetainSessionsRequest message!"),
        }
    }

    fn request(&self, message: api::Message) -> Result<api::Message> {
        // Lock access to the socket to ensure that nothing interferes with our
        // request/response handshake.
        let fd = self.socket_fd.lock();
        protocol::send_message(fd.as_fd(), message, &[])?;
        protocol::receive_message(fd.as_fd())?
            .ok_or_else(|| anyhow::anyhow!("Terminal server closed the connection"))
    }

    /// Returns and consumes the recorded exit status for the given process ID.
    pub fn child_termination_status(&self, pid: u32) -> Option<ObservedExitStatus> {
        self.terminated_children.lock().remove(&pid)
    }
}
