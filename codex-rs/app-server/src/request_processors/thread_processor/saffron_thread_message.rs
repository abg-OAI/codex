//! Same-host message routing owned by the app-server runtime.
//!
//! Core supplies attributed content. This adapter restores saved root settings
//! through native resume, or submits to a loaded owner at the control socket.
//! It never attaches a client listener to the peer, answers approvals, or retries
//! a possible submission. Tracked tasks finish admission if their caller leaves.

use super::*;
use crate::turn_admission::TurnAdmission;
use codex_core::SaffronMessageDelivery;
use codex_core::SaffronMessageSubmission;
use codex_core::SaffronThreadMessageHost;
use codex_core::TurnInputRequest;
use codex_core::TurnInputSubmission;
use codex_extension_api::ExtensionFuture;
use codex_protocol::models::ResponseItem;
use codex_protocol::turn_input::TurnInput;
use codex_protocol::turn_input::TurnStartOptions;

mod peer;

/// Process-owned delivery capability; Core retains only a weak reference.
pub(crate) struct MessageHost {
    /// Native lifecycle and saved configuration owner.
    processor: ThreadRequestProcessor,
    /// Includes preparation in the process shutdown admission barrier.
    admission: TurnAdmission,
}

impl MessageHost {
    /// Connects message routing to the already constructed runtime.
    pub(crate) fn new(processor: ThreadRequestProcessor, admission: TurnAdmission) -> Self {
        Self {
            processor,
            admission,
        }
    }
}

impl SaffronThreadMessageHost for MessageHost {
    fn send(
        &self,
        thread_id: ThreadId,
        message: ResponseItem,
    ) -> ExtensionFuture<'_, Result<SaffronMessageSubmission, String>> {
        Box::pin(async move {
            let permit = self.admission.admit().map_err(|error| error.message)?;
            let processor = self.processor.clone();
            let task = self.processor.background_tasks.spawn(async move {
                let _permit = permit;
                processor.deliver_saffron_message(thread_id, message).await
            });
            // Dropping the wait does not abort the tracked operation halfway
            // through registering a runtime. No second submission is attempted.
            tokio::time::timeout(Duration::from_secs(30), task).await
                .map_err(|_| "delivery is unconfirmed; the host may still submit it; do not resend automatically".to_string())?
                .map_err(|_| "delivery is unconfirmed because the host task stopped; do not resend automatically".to_string())?
        })
    }
}

impl ThreadRequestProcessor {
    /// Resolves a loaded peer first, then restores a locally available root.
    async fn deliver_saffron_message(
        &self,
        thread_id: ThreadId,
        message: ResponseItem,
    ) -> Result<SaffronMessageSubmission, String> {
        // A concurrent send may already have resumed the target. The native
        // resume loop rechecks again while holding the metadata permit.
        if self.thread_manager.get_thread(thread_id).await.is_err() {
            let socket =
                codex_app_server_transport::app_server_control_socket_path(&self.config.codex_home)
                    .map_err(|error| error.to_string())?;
            if let Some(submission) = peer::send_if_loaded(
                socket.as_path(),
                &self.config.codex_home,
                thread_id,
                &message,
            )
            .await?
            {
                return Ok(submission);
            }
        }
        let target = ThreadResumeTarget::Input(
            TurnInputRequest::new(TurnInput::ResponseItem(message)).on_start(TurnStartOptions {
                turn_trigger: Some("saffron_thread_message".to_string()),
                ..Default::default()
            }),
        );
        let params = ThreadResumeParams {
            thread_id: thread_id.to_string(),
            exclude_turns: true,
            ..Default::default()
        };
        let mut prepared = None;
        loop {
            match Box::pin(self.thread_resume_inner(
                &target,
                &params,
                None,
                None,
                Default::default(),
                &mut prepared,
            ))
            .await
            .map_err(|error| error.message)?
            {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(Some(submission)) => return accepted(submission),
                ControlFlow::Break(None) => {
                    return Err("destination did not accept message input".to_string());
                }
            }
        }
    }
}

/// Converts native admission without turning a rejection into a receipt.
fn accepted(submission: TurnInputSubmission) -> Result<SaffronMessageSubmission, String> {
    let (turn_id, status) = match submission {
        TurnInputSubmission::Started { turn_id, .. } => (turn_id, SaffronMessageDelivery::Started),
        TurnInputSubmission::Steered { turn_id, .. } => (turn_id, SaffronMessageDelivery::Steered),
        TurnInputSubmission::NotSubmitted { reason } => {
            return Err(format!("message was not submitted: {reason:?}"));
        }
    };
    Ok(SaffronMessageSubmission { turn_id, status })
}
