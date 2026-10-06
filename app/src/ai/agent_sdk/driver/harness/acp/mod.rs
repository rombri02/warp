//! Driving third-party harnesses over the Agent Client Protocol.
//!
//! The harness CLI runs as a child process of the driver rather than as a command in the
//! terminal session. Its `session/update` stream is translated into MAA client actions and
//! applied to the driver's native conversation, so viewers and the task status model see an
//! ordinary Warp agent turn.
//!
//! Local testing: `oz agent run --harness codex --harness-transport acp --share team:view
//! --idle-on-complete 15m --prompt "..."`, or point `--acp-agent` at
//! `node app/src/ai/agent_sdk/driver/harness/acp/testdata/fake_agent.mjs` for a scripted agent.
mod connection;
mod launch;
mod mapping;
mod protocol;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, anyhow};
use async_trait::async_trait;
use futures::channel::oneshot;
use futures::{FutureExt, pin_mut, select};
pub(crate) use launch::AcpLaunchSpec;
use parking_lot::Mutex;
use serde_json::Value;
use uuid::Uuid;
use warp_cli::agent::Harness;
use warp_core::command::ExitCode;
use warp_managed_secrets::ManagedSecretValue;
use warp_multi_agent_api::response_event::{StreamFinished, stream_finished};
use warpui::r#async::Timer;
use warpui::r#async::executor::Background;
use warpui::{ModelHandle, ModelSpawner};

use self::connection::{AcpConnection, InboundNotification, RpcError};
use self::mapping::{AcpTurnMapper, TurnEvent};
use self::protocol::{
    CancelParams, ClientCapabilities, ClientInfo, ContentBlock, FsCapabilities, InitializeParams,
    InitializeResponse, McpServer, NewSessionParams, NewSessionResponse, PROTOCOL_VERSION,
    PermissionOptionKind, PermissionOutcome, PromptParams, PromptResponse, ReadTextFileParams,
    ReadTextFileResponse, RequestPermissionParams, RequestPermissionResponse, SessionUpdateParams,
    StopReason, WriteTextFileParams,
};
use super::super::terminal::{CommandHandle, TerminalDriver};
use super::super::{AgentDriver, AgentDriverError};
use super::child_process::{HarnessProcess, HarnessProcessLaunch};
use super::claude_code::prepare_claude_environment_config;
use super::codex::prepare_codex_environment_config;
use super::gemini::prepare_gemini_environment_config;
use super::harness_persistence::{HarnessPersistence, PersistenceOutcome};
use super::{
    HarnessCleanupDisposition, HarnessRunner, HarnessTransportMode, JSONMCPServer, ResumePayload,
    SavePoint, ThirdPartyHarness, harness_kind, validate_cli_installed,
};
use crate::ai::agent_sdk::setup_observability::{OzRunTimelineEvent, SetupClientEventReporter};
use crate::ai::ambient_agents::AmbientAgentTaskId;
use crate::ai::ambient_agents::task::HarnessModelConfig;
use crate::ai::blocklist::{ExternalHarnessTurn, ResponseStreamId};
use crate::ai::mcp::JSONTransportType;
use crate::server::server_api::ServerApi;
use crate::terminal::CLIAgent;

/// Bound on the agent's `initialize` and `session/new` handshakes.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
/// How long to wait for the agent to exit after its stdin is closed before killing it.
const AGENT_EXIT_GRACE: Duration = Duration::from_secs(10);

/// A third-party harness driven over ACP. `harness` is the harness identity (which decides
/// bootstrap, auth checks, and server-side bookkeeping); `launch` is the ACP adapter process.
pub(crate) struct AcpHarness {
    harness: Harness,
    launch: AcpLaunchSpec,
}

impl AcpHarness {
    pub(crate) fn new(harness: Harness, launch: AcpLaunchSpec) -> Self {
        Self { harness, launch }
    }

    /// The terminal-based harness with the same identity, whose per-harness knowledge (install
    /// docs, auth checks) applies regardless of transport.
    fn pty_harness(&self) -> Option<Box<dyn ThirdPartyHarness>> {
        match harness_kind(self.harness, &HarnessTransportMode::Pty) {
            Ok(super::HarnessKind::ThirdParty(harness)) => Some(harness),
            Ok(super::HarnessKind::Oz | super::HarnessKind::Unsupported(_)) | Err(_) => None,
        }
    }

    /// Writes the harness's own config files (trust, auth, system prompt) the way the terminal
    /// transport does, so the ACP adapter finds the same environment its CLI would.
    #[allow(clippy::too_many_arguments)]
    fn prepare_harness_environment(
        &self,
        system_prompt: Option<&str>,
        workspace_root: &Path,
        harness_working_dir: &Path,
        resolved_env_vars: &HashMap<OsString, OsString>,
        resolved_secrets: &HashMap<String, ManagedSecretValue>,
        third_party_harness_model_config: Option<&HarnessModelConfig>,
    ) -> Result<()> {
        match self.harness {
            Harness::Claude => prepare_claude_environment_config(
                workspace_root,
                harness_working_dir,
                resolved_env_vars,
            ),
            // MCP servers are handed to the agent in `session/new` rather than written into
            // `config.toml`, so they are not registered twice.
            Harness::Codex => prepare_codex_environment_config(
                workspace_root,
                harness_working_dir,
                system_prompt,
                resolved_env_vars,
                resolved_secrets,
                &HashMap::new(),
                third_party_harness_model_config,
            ),
            Harness::Gemini => {
                prepare_gemini_environment_config(harness_working_dir, system_prompt)
            }
            Harness::Oz | Harness::OpenCode | Harness::Unknown => Ok(()),
        }
    }
}

#[cfg_attr(not(target_family = "wasm"), async_trait)]
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
impl ThirdPartyHarness for AcpHarness {
    fn harness(&self) -> Harness {
        self.harness
    }

    fn cli_agent(&self) -> CLIAgent {
        match self.harness {
            Harness::Claude => CLIAgent::Claude,
            Harness::Codex => CLIAgent::Codex,
            Harness::Gemini => CLIAgent::Gemini,
            Harness::Oz | Harness::OpenCode | Harness::Unknown => CLIAgent::Unknown,
        }
    }

    fn install_docs_url(&self) -> Option<&'static str> {
        self.pty_harness()
            .and_then(|harness| harness.install_docs_url())
    }

    fn validate(&self) -> Result<(), AgentDriverError> {
        validate_cli_installed(&self.launch.program, self.install_docs_url())
    }

    fn auth_check_command(&self) -> Option<String> {
        self.pty_harness()
            .and_then(|harness| harness.auth_check_command())
    }

    fn drives_cli_agent_session(&self) -> bool {
        false
    }

    fn build_runner(
        &self,
        prompt: &str,
        system_prompt: Option<&str>,
        resumption_prompt: Option<&str>,
        context: Option<&str>,
        workspace_root: &Path,
        harness_working_dir: &Path,
        task_id: Option<AmbientAgentTaskId>,
        _server_api: Arc<ServerApi>,
        terminal_driver: ModelHandle<TerminalDriver>,
        _resume: Option<ResumePayload>,
        resolved_env_vars: &HashMap<OsString, OsString>,
        resolved_secrets: &HashMap<String, ManagedSecretValue>,
        resolved_mcp_servers: &HashMap<String, JSONMCPServer>,
        third_party_harness_model_config: Option<&HarnessModelConfig>,
    ) -> Result<Box<dyn HarnessRunner>, AgentDriverError> {
        self.prepare_harness_environment(
            system_prompt,
            workspace_root,
            harness_working_dir,
            resolved_env_vars,
            resolved_secrets,
            third_party_harness_model_config,
        )
        .map_err(|error| AgentDriverError::HarnessConfigSetupFailed {
            harness: self.harness.to_string(),
            error,
        })?;

        // ACP has no system-prompt channel of its own, so every server-composed block that the
        // harness bootstrap did not already place is folded into the user turn ahead of the
        // prompt itself.
        let system_prompt_in_turn = match self.harness {
            Harness::Claude | Harness::Oz | Harness::OpenCode | Harness::Unknown => system_prompt,
            Harness::Codex | Harness::Gemini => None,
        };
        let prompt_text = [
            resumption_prompt,
            context,
            system_prompt_in_turn,
            Some(prompt),
        ]
        .into_iter()
        .flatten()
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
        Ok(Box::new(AcpHarnessRunner {
            harness: self.harness,
            launch: self.launch.clone(),
            user_prompt: prompt.to_owned(),
            prompt_text,
            harness_working_dir: harness_working_dir.to_path_buf(),
            task_id,
            terminal_driver,
            env: resolved_env_vars.clone(),
            mcp_servers: resolved_mcp_servers
                .iter()
                .map(|(name, server)| mcp_server_for_acp(name, server))
                .collect(),
            persistence: HarnessPersistence::default(),
            agent: Mutex::new(None),
        }))
    }
}

/// The running agent process and the protocol session on top of it.
struct RunningAgent {
    process: Arc<HarnessProcess>,
    connection: Arc<AcpConnection>,
    session_id: Option<String>,
}

pub(crate) struct AcpHarnessRunner {
    harness: Harness,
    launch: AcpLaunchSpec,
    /// The prompt as the user wrote it, echoed into the conversation.
    user_prompt: String,
    /// The full text sent to the agent, including server-composed context.
    prompt_text: String,
    harness_working_dir: PathBuf,
    task_id: Option<AmbientAgentTaskId>,
    terminal_driver: ModelHandle<TerminalDriver>,
    env: HashMap<OsString, OsString>,
    mcp_servers: Vec<McpServer>,
    persistence: HarnessPersistence,
    agent: Mutex<Option<RunningAgent>>,
}

impl AcpHarnessRunner {
    fn setup_failed(&self, reason: String) -> AgentDriverError {
        AgentDriverError::HarnessSetupFailed {
            harness: self.harness.to_string(),
            reason,
        }
    }

    fn running_agent(&self) -> Option<(Arc<HarnessProcess>, Arc<AcpConnection>, Option<String>)> {
        self.agent.lock().as_ref().map(|agent| {
            (
                agent.process.clone(),
                agent.connection.clone(),
                agent.session_id.clone(),
            )
        })
    }
}

#[cfg_attr(not(target_family = "wasm"), async_trait)]
#[cfg_attr(target_family = "wasm", async_trait(?Send))]
impl HarnessRunner for AcpHarnessRunner {
    fn harness_name(&self) -> &str {
        &self.launch.program
    }

    fn persistence(&self) -> &HarnessPersistence {
        &self.persistence
    }

    async fn start(
        &self,
        foreground: &ModelSpawner<AgentDriver>,
        setup_events: &SetupClientEventReporter,
    ) -> Result<CommandHandle, AgentDriverError> {
        let (background, idle_on_complete): (Arc<Background>, Option<Duration>) = foreground
            .spawn(|me, ctx| (ctx.background_executor(), me.idle_on_complete))
            .await
            .map_err(|_| AgentDriverError::InvalidRuntimeState)?;

        let (process, stdio) = HarnessProcess::spawn(
            HarnessProcessLaunch {
                program: &self.launch.program,
                args: &self.launch.args,
                cwd: &self.harness_working_dir,
                env: &self.env,
            },
            &background,
        )
        .map_err(|error| self.setup_failed(format!("{error:#}")))?;
        let process = Arc::new(process);
        let (connection, notifications) = AcpConnection::new(
            stdio.stdout,
            stdio.stdin,
            Arc::new(handle_agent_request),
            &background,
        );
        let connection = Arc::new(connection);
        *self.agent.lock() = Some(RunningAgent {
            process: process.clone(),
            connection: connection.clone(),
            session_id: None,
        });

        let handshake_connection = connection.clone();
        let handshake = async move {
            let connection = handshake_connection;
            let initialize: InitializeResponse = connection
                .request(
                    protocol::METHOD_INITIALIZE,
                    InitializeParams {
                        protocol_version: PROTOCOL_VERSION,
                        client_capabilities: ClientCapabilities {
                            fs: FsCapabilities {
                                read_text_file: true,
                                write_text_file: true,
                            },
                            terminal: false,
                        },
                        client_info: ClientInfo {
                            name: "warp".to_owned(),
                            title: "Warp".to_owned(),
                            version: env!("CARGO_PKG_VERSION").to_owned(),
                        },
                    },
                )
                .await
                .context("initialize failed")?;
            log::info!(
                "ACP agent initialized: protocol v{} agent={} auth_methods={}",
                initialize.protocol_version,
                initialize
                    .agent_info
                    .as_ref()
                    .map(|info| format!("{} {}", info.name, info.version))
                    .unwrap_or_else(|| "unknown".to_owned()),
                initialize.auth_methods.len(),
            );
            let session: NewSessionResponse = connection
                .request(
                    protocol::METHOD_SESSION_NEW,
                    NewSessionParams {
                        cwd: self.harness_working_dir.display().to_string(),
                        mcp_servers: self.mcp_servers.clone(),
                    },
                )
                .await
                .context("session/new failed")?;
            Ok::<_, anyhow::Error>(session.session_id)
        }
        .fuse();
        pin_mut!(handshake);
        let session_id = select! {
            result = handshake => result,
            _ = Timer::after(HANDSHAKE_TIMEOUT).fuse() => {
                Err(anyhow!("the agent did not complete its handshake within {HANDSHAKE_TIMEOUT:?}"))
            }
        };
        let session_id = match session_id {
            Ok(session_id) => session_id,
            Err(error) => {
                process.kill().await;
                return Err(self.setup_failed(format!("{error:#}")));
            }
        };
        log::info!("ACP session {session_id} created");
        if let Some(agent) = self.agent.lock().as_mut() {
            agent.session_id = Some(session_id.clone());
        }

        let run_id = self.task_id.map(|id| id.to_string());
        let turn = begin_turn(foreground, &self.terminal_driver, run_id.clone()).await?;
        let (follow_up_tx, follow_up_rx) = async_channel::unbounded::<String>();
        let terminal_driver = self.terminal_driver.clone();
        foreground
            .spawn(move |_, ctx| {
                let terminal = terminal_driver.as_ref(ctx).terminal_view().clone();
                terminal.update(ctx, |terminal, ctx| {
                    terminal.ai_controller().update(ctx, |controller, _| {
                        controller.set_external_harness_prompt_sink(follow_up_tx);
                    });
                });
            })
            .await
            .map_err(|_| AgentDriverError::InvalidRuntimeState)?;

        let mut mapper = AcpTurnMapper::new(Uuid::new_v4().to_string(), turn.request_id.clone());
        apply_actions(
            foreground,
            &self.terminal_driver,
            &turn.stream_id,
            mapper.initial_actions(&self.user_prompt),
        )
        .await?;

        setup_events
            .post_timeline_event(OzRunTimelineEvent::AgentStarted)
            .await;

        let (exit_tx, exit_rx) = oneshot::channel::<Result<ExitCode, AgentDriverError>>();
        let turn_driver = TurnDriver {
            process,
            connection,
            notifications,
            follow_ups: follow_up_rx,
            foreground: foreground.clone(),
            terminal_driver: self.terminal_driver.clone(),
            run_id,
            turn: Mutex::new(turn),
            session_id,
            prompt_text: self.prompt_text.clone(),
            idle_on_complete,
        };
        background
            .spawn(async move {
                let outcome = turn_driver.run(&mut mapper).await;
                let _ = exit_tx.send(Ok(outcome));
            })
            .detach();

        Ok(CommandHandle::for_detached_process(exit_rx))
    }

    async fn save_conversation(
        &self,
        _save_point: SavePoint,
        _foreground: &ModelSpawner<AgentDriver>,
    ) -> PersistenceOutcome {
        // The native conversation is streamed live to viewers; server-side persistence of ACP
        // runs is handled separately from the harness transcript upload path.
        PersistenceOutcome::skipped()
    }

    async fn exit(&self, _foreground: &ModelSpawner<AgentDriver>) -> Result<()> {
        let Some((_, connection, session_id)) = self.running_agent() else {
            return Ok(());
        };
        if let Some(session_id) = session_id {
            connection
                .notify(protocol::METHOD_SESSION_CANCEL, CancelParams { session_id })
                .await?;
        }
        connection.close().await;
        Ok(())
    }

    async fn exit_followup(&self, _foreground: &ModelSpawner<AgentDriver>) -> Result<()> {
        if let Some((process, _, _)) = self.running_agent() {
            process.kill().await;
        }
        Ok(())
    }

    async fn cleanup(
        &self,
        _cleanup_disposition: HarnessCleanupDisposition,
        _foreground: &ModelSpawner<AgentDriver>,
    ) -> Result<()> {
        self.agent.lock().take();
        Ok(())
    }
}

/// Runs the initial prompt turn, then any follow-up prompts that arrive during the idle window,
/// and finally settles the conversation and the agent process.
///
/// A prompt turn spans one or more MAA request streams: every time the agent reports results
/// for tool calls announced in the current stream, that stream is finished and a new one is
/// opened, as the MAA server does for its own tool round trips.
struct TurnDriver {
    process: Arc<HarnessProcess>,
    connection: Arc<AcpConnection>,
    notifications: async_channel::Receiver<InboundNotification>,
    /// Follow-up prompts injected into the conversation (e.g. by shared-session viewers).
    follow_ups: async_channel::Receiver<String>,
    foreground: ModelSpawner<AgentDriver>,
    terminal_driver: ModelHandle<TerminalDriver>,
    run_id: Option<String>,
    /// The currently open request stream.
    turn: Mutex<ExternalHarnessTurn>,
    session_id: String,
    prompt_text: String,
    /// How long to keep the agent (and with it the shared session) alive after the turn ends.
    idle_on_complete: Option<Duration>,
}

impl TurnDriver {
    async fn run(self, mapper: &mut AcpTurnMapper) -> ExitCode {
        let mut turn_result = self.run_turn(mapper, self.prompt_text.clone()).await;

        if let Some(idle) = self.idle_on_complete {
            log::info!("ACP turn finished; accepting follow-ups for {idle:?} of idle time");
            loop {
                let idle_timer = Timer::after(idle).fuse();
                pin_mut!(idle_timer);
                select! {
                    prompt = self.follow_ups.recv().fuse() => match prompt {
                        Ok(prompt) => {
                            log::info!("Running ACP follow-up turn");
                            turn_result = self.run_follow_up(mapper, prompt).await;
                        }
                        Err(_) => break,
                    },
                    _ = idle_timer => {
                        log::info!("ACP idle window elapsed; shutting the agent down");
                        break;
                    }
                }
            }
        }
        self.connection.close().await;
        let exit_code = match self.process.wait_for_exit(AGENT_EXIT_GRACE).await {
            Ok(exit_code) => exit_code,
            Err(error) => {
                log::warn!("Failed to wait for ACP agent exit: {error:#}");
                ExitCode::from(1)
            }
        };
        log::info!(
            "ACP agent `{}` (pid {}) exited with {exit_code}",
            self.process.program(),
            self.process.pid()
        );
        match turn_result {
            Ok(StopReason::EndTurn | StopReason::Cancelled) => exit_code,
            Ok(_) | Err(_) if exit_code.was_successful() => ExitCode::from(1),
            Ok(_) | Err(_) => exit_code,
        }
    }

    /// Opens a new request stream for a follow-up prompt, echoes it, and runs the turn.
    async fn run_follow_up(
        &self,
        mapper: &mut AcpTurnMapper,
        prompt: String,
    ) -> Result<StopReason> {
        let next = begin_turn(&self.foreground, &self.terminal_driver, self.run_id.clone())
            .await
            .map_err(|error| anyhow!("{error}"))?;
        mapper.start_segment(next.request_id.clone());
        *self.turn.lock() = next;
        let echo = vec![mapper.user_query_action(&prompt)];
        self.apply_events(mapper, vec![TurnEvent::Actions(echo)])
            .await?;
        self.run_turn(mapper, prompt).await
    }

    /// Runs one prompt turn to completion and finishes its last request stream.
    async fn run_turn(&self, mapper: &mut AcpTurnMapper, prompt: String) -> Result<StopReason> {
        let turn_result = self.prompt(mapper, prompt).await;
        let finished = match &turn_result {
            Ok(stop_reason) => stream_finished_for(*stop_reason),
            Err(error) => {
                log::error!("ACP turn failed: {error:#}");
                StreamFinished {
                    reason: Some(stream_finished::Reason::InternalError(
                        stream_finished::InternalError {
                            message: format!("{error:#}"),
                        },
                    )),
                    ..Default::default()
                }
            }
        };
        let finish_events = mapper.finish_events();
        if let Err(error) = self.apply_events(mapper, finish_events).await {
            log::warn!("Failed to settle ACP tool calls: {error}");
        }
        let stream_id = self.turn.lock().stream_id.clone();
        if finish_turn(&self.foreground, &self.terminal_driver, stream_id, finished)
            .await
            .is_err()
        {
            log::warn!("Agent driver dropped before the ACP turn could be finished");
        }
        turn_result
    }

    /// Sends the prompt and applies every update the agent streams before responding.
    async fn prompt(&self, mapper: &mut AcpTurnMapper, prompt: String) -> Result<StopReason> {
        let prompt_fut = self
            .connection
            .request::<_, PromptResponse>(
                protocol::METHOD_SESSION_PROMPT,
                PromptParams {
                    session_id: self.session_id.clone(),
                    prompt: vec![ContentBlock::Text { text: prompt }],
                },
            )
            .fuse();
        pin_mut!(prompt_fut);
        loop {
            select! {
                notification = self.notifications.recv().fuse() => match notification {
                    Ok(notification) => self.handle_notification(mapper, notification).await?,
                    // The agent closed its stdout; the pending prompt resolves with the error.
                    Err(_) => break,
                },
                response = prompt_fut => {
                    let response = response?;
                    while let Ok(notification) = self.notifications.try_recv() {
                        self.handle_notification(mapper, notification).await?;
                    }
                    return Ok(response.stop_reason);
                }
            }
        }
        prompt_fut.await.map(|response| response.stop_reason)
    }

    async fn handle_notification(
        &self,
        mapper: &mut AcpTurnMapper,
        notification: InboundNotification,
    ) -> Result<()> {
        if notification.method != protocol::METHOD_SESSION_UPDATE {
            log::debug!("Ignoring ACP notification {}", notification.method);
            return Ok(());
        }
        let params: SessionUpdateParams = match serde_json::from_value(notification.params) {
            Ok(params) => params,
            Err(error) => {
                log::warn!("Ignoring unparseable ACP session/update: {error}");
                return Ok(());
            }
        };
        let events = mapper.map_update(params.update);
        self.apply_events(mapper, events).await
    }

    async fn apply_events(&self, mapper: &mut AcpTurnMapper, events: Vec<TurnEvent>) -> Result<()> {
        for event in events {
            match event {
                TurnEvent::Actions(actions) => {
                    let stream_id = self.turn.lock().stream_id.clone();
                    apply_actions(&self.foreground, &self.terminal_driver, &stream_id, actions)
                        .await
                        .map_err(|error| anyhow!("{error}"))?;
                }
                TurnEvent::SegmentBoundary => {
                    let stream_id = self.turn.lock().stream_id.clone();
                    finish_turn(
                        &self.foreground,
                        &self.terminal_driver,
                        stream_id,
                        stream_finished_for(StopReason::EndTurn),
                    )
                    .await
                    .map_err(|error| anyhow!("{error}"))?;
                    let next =
                        begin_turn(&self.foreground, &self.terminal_driver, self.run_id.clone())
                            .await
                            .map_err(|error| anyhow!("{error}"))?;
                    mapper.start_segment(next.request_id.clone());
                    *self.turn.lock() = next;
                }
            }
        }
        Ok(())
    }
}

/// Opens a request stream on the driver's bound native conversation.
async fn begin_turn(
    foreground: &ModelSpawner<AgentDriver>,
    terminal_driver: &ModelHandle<TerminalDriver>,
    run_id: Option<String>,
) -> Result<ExternalHarnessTurn, AgentDriverError> {
    let terminal_driver = terminal_driver.clone();
    foreground
        .spawn(move |_, ctx| {
            let terminal = terminal_driver.as_ref(ctx).terminal_view().clone();
            terminal.update(ctx, |terminal, ctx| {
                terminal.ai_controller().update(ctx, |controller, ctx| {
                    let conversation_id = controller
                        .native_prompt_conversation_id()
                        .unwrap_or_else(|| controller.bind_native_prompt_conversation(None, ctx));
                    controller.begin_external_harness_turn(conversation_id, run_id, ctx)
                })
            })
        })
        .await
        .map_err(|_| AgentDriverError::InvalidRuntimeState)?
        .map_err(|error| AgentDriverError::HarnessSetupFailed {
            harness: "acp".to_owned(),
            reason: format!("could not open conversation turn: {error}"),
        })
}

async fn finish_turn(
    foreground: &ModelSpawner<AgentDriver>,
    terminal_driver: &ModelHandle<TerminalDriver>,
    stream_id: ResponseStreamId,
    finished: StreamFinished,
) -> Result<(), AgentDriverError> {
    let terminal_driver = terminal_driver.clone();
    foreground
        .spawn(move |_, ctx| {
            let terminal = terminal_driver.as_ref(ctx).terminal_view().clone();
            terminal.update(ctx, |terminal, ctx| {
                terminal.ai_controller().update(ctx, |controller, ctx| {
                    controller.finish_external_harness_turn(&stream_id, finished, ctx);
                });
            });
        })
        .await
        .map_err(|_| AgentDriverError::InvalidRuntimeState)
}

async fn apply_actions(
    foreground: &ModelSpawner<AgentDriver>,
    terminal_driver: &ModelHandle<TerminalDriver>,
    stream_id: &ResponseStreamId,
    actions: Vec<warp_multi_agent_api::ClientAction>,
) -> Result<(), AgentDriverError> {
    if actions.is_empty() {
        return Ok(());
    }
    let terminal_driver = terminal_driver.clone();
    let stream_id = stream_id.clone();
    foreground
        .spawn(move |_, ctx| {
            let terminal = terminal_driver.as_ref(ctx).terminal_view().clone();
            terminal.update(ctx, |terminal, ctx| {
                terminal.ai_controller().update(ctx, |controller, ctx| {
                    controller.apply_external_harness_client_actions(&stream_id, actions, ctx)
                })
            })
        })
        .await
        .map_err(|_| AgentDriverError::InvalidRuntimeState)?
        .map_err(|error| {
            log::warn!("Failed to apply ACP client actions: {error}");
            AgentDriverError::InvalidRuntimeState
        })
}

fn stream_finished_for(stop_reason: StopReason) -> StreamFinished {
    let reason = match stop_reason {
        StopReason::EndTurn | StopReason::Cancelled => {
            stream_finished::Reason::Done(stream_finished::Done {})
        }
        StopReason::MaxTokens => {
            stream_finished::Reason::MaxTokenLimit(stream_finished::ReachedMaxTokenLimit {})
        }
        StopReason::MaxTurnRequests | StopReason::Refusal | StopReason::Unknown => {
            stream_finished::Reason::InternalError(stream_finished::InternalError {
                message: format!("ACP agent stopped: {stop_reason:?}"),
            })
        }
    };
    StreamFinished {
        reason: Some(reason),
        ..Default::default()
    }
}

/// Answers the requests an ACP agent may make of its client. Cloud runs are unattended, so
/// permission prompts are granted with the broadest "allow" option offered.
fn handle_agent_request(method: &str, params: Value) -> Result<Value, RpcError> {
    match method {
        protocol::METHOD_REQUEST_PERMISSION => {
            let params: RequestPermissionParams =
                serde_json::from_value(params).map_err(RpcError::internal)?;
            let chosen = params
                .options
                .iter()
                .find(|option| option.kind == PermissionOptionKind::AllowAlways)
                .or_else(|| {
                    params
                        .options
                        .iter()
                        .find(|option| option.kind == PermissionOptionKind::AllowOnce)
                })
                .or_else(|| params.options.first());
            let outcome = match chosen {
                Some(option) => {
                    log::info!(
                        "Auto-approving ACP permission request with `{}`",
                        option.name
                    );
                    PermissionOutcome::Selected {
                        option_id: option.option_id.clone(),
                    }
                }
                None => PermissionOutcome::Cancelled,
            };
            serde_json::to_value(RequestPermissionResponse { outcome }).map_err(RpcError::internal)
        }
        protocol::METHOD_FS_READ_TEXT_FILE => {
            let params: ReadTextFileParams =
                serde_json::from_value(params).map_err(RpcError::internal)?;
            let content = std::fs::read_to_string(&params.path)
                .with_context(|| format!("reading {}", params.path))
                .map_err(RpcError::internal)?;
            let content = match (params.line, params.limit) {
                (None, None) => content,
                (line, limit) => {
                    let start = line.unwrap_or(1).saturating_sub(1) as usize;
                    let lines = content.lines().skip(start);
                    match limit {
                        Some(limit) => lines.take(limit as usize).collect::<Vec<_>>().join("\n"),
                        None => lines.collect::<Vec<_>>().join("\n"),
                    }
                }
            };
            serde_json::to_value(ReadTextFileResponse { content }).map_err(RpcError::internal)
        }
        protocol::METHOD_FS_WRITE_TEXT_FILE => {
            let params: WriteTextFileParams =
                serde_json::from_value(params).map_err(RpcError::internal)?;
            if let Some(parent) = Path::new(&params.path).parent() {
                std::fs::create_dir_all(parent).map_err(RpcError::internal)?;
            }
            std::fs::write(&params.path, params.content)
                .with_context(|| format!("writing {}", params.path))
                .map_err(RpcError::internal)?;
            Ok(Value::Null)
        }
        other => Err(RpcError::method_not_found(other)),
    }
}

fn mcp_server_for_acp(name: &str, server: &JSONMCPServer) -> McpServer {
    match &server.transport_type {
        JSONTransportType::CLIServer {
            command, args, env, ..
        } => McpServer::Stdio {
            name: name.to_owned(),
            command: command.clone(),
            args: args.clone(),
            env: env
                .iter()
                .map(|(name, value)| protocol::EnvVariable {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
        },
        JSONTransportType::SSEServer { url, headers } => McpServer::Remote {
            kind: "http",
            name: name.to_owned(),
            url: url.clone(),
            headers: headers
                .iter()
                .map(|(name, value)| protocol::HttpHeader {
                    name: name.clone(),
                    value: value.clone(),
                })
                .collect(),
        },
    }
}
