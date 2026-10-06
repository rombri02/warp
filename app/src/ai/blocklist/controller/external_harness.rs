// Plumbing for harnesses that run outside Warp's agent loop but render through a native
// conversation: the harness translates its own protocol into MAA response events and applies
// them here. Unlike the shared-session viewer path, the conversation stays owned by this client
// (not view-only), so task status syncing keeps working; and unlike the native MAA path there is
// no `ResponseStream` model, so the action model never executes the harness's tool calls.
use itertools::Itertools;
use uuid::Uuid;
use warp_multi_agent_api::response_event::{StreamFinished, StreamInit};
use warp_multi_agent_api::{ClientAction, ResponseEvent, response_event};
use warpui::{ModelContext, SingletonEntity};

use super::response_stream::ResponseStreamId;
use super::{BlocklistAIController, RequestInput, SessionContext};
use crate::ai::agent::conversation::{AIConversationId, ConversationStatus};
use crate::ai::blocklist::history_model::{BlocklistAIHistoryModel, UpdateHistoryError};
use crate::workspaces::user_workspaces::ResolvedTeamScope;

/// Identifiers for one harness-driven turn.
#[derive(Clone, Debug)]
pub(crate) struct ExternalHarnessTurn {
    pub stream_id: ResponseStreamId,
    pub request_id: String,
}

impl BlocklistAIController {
    /// Diverts prompts injected into the bound native conversation to an external harness.
    pub(crate) fn set_external_harness_prompt_sink(&mut self, sink: async_channel::Sender<String>) {
        self.external_harness_prompt_sink = Some(sink);
    }

    /// Opens a harness-driven turn on `conversation_id`: registers an exchange for a fresh
    /// response stream, forwards the `StreamInit` to viewers, and marks the conversation in
    /// progress. Subsequent client actions must add the user query and task messages
    /// themselves, as the MAA server does.
    pub(crate) fn begin_external_harness_turn(
        &mut self,
        conversation_id: AIConversationId,
        run_id: Option<String>,
        ctx: &mut ModelContext<Self>,
    ) -> Result<ExternalHarnessTurn, UpdateHistoryError> {
        let history = BlocklistAIHistoryModel::handle(ctx);
        let conversation_token = history
            .as_ref(ctx)
            .conversation(&conversation_id)
            .and_then(|conversation| {
                conversation
                    .server_conversation_token()
                    .map(|token| token.as_str().to_string())
            })
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let request_id = Uuid::new_v4().to_string();
        let init = StreamInit {
            conversation_id: conversation_token.clone(),
            request_id: request_id.clone(),
            run_id: run_id.unwrap_or_default(),
        };
        let stream_id = ResponseStreamId::for_shared_session(&init);

        let root_task_id = history
            .as_ref(ctx)
            .conversation(&conversation_id)
            .ok_or(UpdateHistoryError::ConversationNotFound(conversation_id))?
            .get_root_task_id()
            .clone();

        let scope = ResolvedTeamScope::from_scope(&self.team_context(ctx));
        let request_input = RequestInput::for_task(
            vec![],
            root_task_id,
            &self.active_session,
            self.get_current_response_initiator(),
            conversation_id,
            self.terminal_surface_id,
            &scope,
            ctx,
        );
        let terminal_surface_id = self.terminal_surface_id;
        history.update(ctx, |history, ctx| {
            history.set_reconstructs_inputs_from_messages_for_conversation(conversation_id, true);
            history.update_conversation_for_new_request_input(
                request_input,
                stream_id.clone(),
                terminal_surface_id,
                ctx,
            )
        })?;

        let init_event = ResponseEvent {
            r#type: Some(response_event::Type::Init(init.clone())),
        };
        self.tee_response_event_to_viewers(&init_event, conversation_id, ctx);

        history.update(ctx, |history, ctx| {
            history.initialize_output_for_response_stream(
                &stream_id,
                conversation_id,
                terminal_surface_id,
                init,
                ctx,
            );
            history.update_conversation_status(
                terminal_surface_id,
                conversation_id,
                ConversationStatus::InProgress,
                ctx,
            );
            history.set_active_conversation_id(conversation_id, terminal_surface_id, ctx);
        });

        Ok(ExternalHarnessTurn {
            stream_id,
            request_id,
        })
    }

    /// Applies harness-authored client actions to the turn's conversation and forwards them to
    /// viewers. Tool call results carried by the actions are recorded as finished so the action
    /// model never tries to execute the harness's tools locally.
    pub(crate) fn apply_external_harness_client_actions(
        &mut self,
        stream_id: &ResponseStreamId,
        actions: Vec<ClientAction>,
        ctx: &mut ModelContext<Self>,
    ) -> Result<(), UpdateHistoryError> {
        let history = BlocklistAIHistoryModel::handle(ctx);
        let Some(conversation_id) = history
            .as_ref(ctx)
            .conversation_for_response_stream(stream_id)
        else {
            log::warn!("No conversation for external harness response stream {stream_id:?}");
            return Ok(());
        };

        let event = ResponseEvent {
            r#type: Some(response_event::Type::ClientActions(
                response_event::ClientActions {
                    actions: actions.clone(),
                },
            )),
        };
        self.tee_response_event_to_viewers(&event, conversation_id, ctx);

        let skill_path_origin =
            SessionContext::from_session(self.active_session.as_ref(ctx), ctx).skill_path_origin();
        let terminal_surface_id = self.terminal_surface_id;
        history.update(ctx, |history, ctx| {
            history.apply_client_actions(
                stream_id,
                actions,
                conversation_id,
                terminal_surface_id,
                &skill_path_origin,
                ctx,
            )
        })?;

        let Some(conversation) = history.as_ref(ctx).conversation(&conversation_id) else {
            return Ok(());
        };
        let new_action_results = conversation
            .new_exchange_ids_for_response(stream_id)
            .filter_map(|exchange_id| conversation.exchange_with_id(exchange_id))
            .flat_map(|exchange| {
                exchange
                    .input
                    .iter()
                    .filter_map(|input| input.action_result().cloned())
            })
            .collect_vec();
        for result in new_action_results {
            if self
                .action_model
                .as_ref(ctx)
                .get_action_result(&result.id)
                .is_none()
            {
                self.action_model.update(ctx, |action_model, ctx| {
                    action_model.apply_finished_action_result(conversation_id, result, ctx);
                });
            }
        }
        Ok(())
    }

    /// Closes a harness-driven turn, forwarding the `StreamFinished` to viewers and settling the
    /// conversation status from its reason.
    pub(crate) fn finish_external_harness_turn(
        &mut self,
        stream_id: &ResponseStreamId,
        finished: StreamFinished,
        ctx: &mut ModelContext<Self>,
    ) {
        let Some(conversation_id) =
            BlocklistAIHistoryModel::as_ref(ctx).conversation_for_response_stream(stream_id)
        else {
            log::warn!("No conversation for external harness response stream {stream_id:?}");
            return;
        };
        let event = ResponseEvent {
            r#type: Some(response_event::Type::Finished(finished.clone())),
        };
        self.tee_response_event_to_viewers(&event, conversation_id, ctx);
        self.handle_response_stream_finished(
            stream_id,
            finished,
            conversation_id,
            /*did_request_contain_user_query*/ true,
            ctx,
        );
    }
}

#[cfg(test)]
#[path = "external_harness_tests.rs"]
mod tests;
