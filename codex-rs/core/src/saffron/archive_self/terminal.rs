//! Ends sampling and tool execution without turning self-archival into an abort.
//!
//! The signal belongs to a turn, not the thread. Revocation reopens tool admission
//! only after cleanup, so accepted steering can continue. A later turn gets a
//! fresh signal.

use std::future::Future;
use std::sync::OnceLock;

use codex_protocol::ResponseItemId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use tokio_util::sync::CancellationToken;

use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;

#[derive(Default)]
struct TerminalArchive {
    requested: CancellationToken,
    final_message: OnceLock<Option<String>>,
}

pub(super) fn request(turn: &TurnContext, final_message: Option<String>) {
    let terminal = turn.extension_data.get_or_init(TerminalArchive::default);
    let _ = terminal.final_message.set(final_message);
    terminal.requested.cancel();
}

pub(super) fn requested(turn: &TurnContext) -> bool {
    turn.extension_data
        .get::<TerminalArchive>()
        .is_some_and(|terminal| terminal.requested.is_cancelled())
}

/// Refuses dispatch after terminal acceptance, including calls from Code Mode.
pub(crate) fn admit_tool(
    turn: &TurnContext,
) -> Result<(), crate::function_tool::FunctionCallError> {
    if requested(turn) {
        return Err(crate::function_tool::FunctionCallError::RespondToModel(
            "The turn is ending for self-archival; no further tools may start.".into(),
        ));
    }
    Ok(())
}

/// Returns `None` after terminal acceptance and cleanup, otherwise sampling's result.
/// The execution token is a child of the real turn token: stopping tools must not
/// cancel the turn itself, whose successful completion authorizes host archival.
pub(crate) async fn run_sampling<T>(
    session: &Session,
    turn: &TurnContext,
    execution: CancellationToken,
    sampling: impl Future<Output = CodexResult<T>>,
) -> CodexResult<Option<T>> {
    let terminal = turn.extension_data.get_or_init(TerminalArchive::default);
    tokio::pin!(sampling);
    let result = tokio::select! {
        biased;
        _ = terminal.requested.cancelled() => None,
        result = &mut sampling => Some(result),
    };
    if !terminal.requested.is_cancelled() {
        return result
            .expect("sampling completed without a terminal request")
            .map(Some);
    }
    execution.cancel();
    // Termination runs alongside the sampling drain: a nested archive call
    // must be allowed to return before its containing cell can close. Cleanup
    // also runs when sampling completed in the same poll as terminal acceptance.
    let (result, ()) = tokio::join!(
        async {
            match result {
                Some(result) => result,
                None => sampling.await,
            }
        },
        session.services.code_mode_service.interrupt_active_cells(),
    );
    match result {
        Ok(_) => Ok(None),
        Err(error) if matches!(error.details(), CodexErrorDetails::TurnAborted) => Ok(None),
        Err(error) => Err(error),
    }
}

/// Reopens admission only after the previous sampling step and cells have drained.
/// Accepted steering revokes archival but still deserves a model response.
pub(crate) fn resume_if_revoked(session: &Session, turn: &TurnContext) -> bool {
    if session
        .services
        .thread_extension_data
        .get::<super::PendingArchive>()
        .is_some_and(|state| state.matches(&turn.sub_id))
    {
        return false;
    }
    turn.extension_data.remove::<TerminalArchive>();
    true
}

/// Emits the supplied closing text only while this turn still owns archival.
pub(crate) async fn complete(
    session: &Session,
    step: &StepContext,
    cancellation: &CancellationToken,
) -> CodexResult<Option<String>> {
    if cancellation.is_cancelled() || session.is_interrupted() {
        return Err(CodexErr::TurnAborted);
    }
    let turn = &step.turn;
    if !session
        .services
        .thread_extension_data
        .get::<super::PendingArchive>()
        .is_some_and(|state| state.matches(&turn.sub_id))
    {
        return Ok(None);
    }
    let message = turn
        .extension_data
        .get::<TerminalArchive>()
        .and_then(|terminal| terminal.final_message.get().cloned().flatten());
    if let Some(text) = &message {
        session
            .record_response_item_and_emit_turn_item(
                turn,
                &step.settings.model_info,
                ResponseItem::Message {
                    id: Some(ResponseItemId::new("msg")),
                    role: "assistant".into(),
                    content: vec![ContentItem::OutputText { text: text.clone() }],
                    phase: Some(MessagePhase::FinalAnswer),
                    internal_chat_message_metadata_passthrough: None,
                },
            )
            .await;
    }
    Ok(message)
}
