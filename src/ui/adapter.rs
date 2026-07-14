//! Bounded, nonblocking seam between the platform UI and background runtime.

use tokio::sync::mpsc::{self, error::TryRecvError, error::TrySendError};

use std::fmt;

use crate::model::{OperationId, ProfileId, QueryLanguage};
use crate::service::{CreateProfileRequest, UpdateProfileRequest};

use super::model::UiEvent;

/// Sensitive command payloads use a redacted Debug and cannot be serialized.
///
/// ```compile_fail
/// # #[cfg(feature = "desktop")]
/// fn check() {
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::ui::UiCommand>();
/// # }
/// ```
pub enum UiCommand {
    RefreshProfiles,
    CreateProfile(CreateProfileRequest),
    UpdateProfile(UpdateProfileRequest),
    TestConnection {
        operation_id: OperationId,
        profile_id: ProfileId,
    },
    Execute {
        operation_id: OperationId,
        profile_id: ProfileId,
        language: QueryLanguage,
        text: String,
        row_limit: u32,
        timeout_ms: u64,
    },
}

impl fmt::Debug for UiCommand {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RefreshProfiles => formatter.write_str("UiCommand::RefreshProfiles"),
            Self::CreateProfile(request) => formatter
                .debug_tuple("UiCommand::CreateProfile")
                .field(request)
                .finish(),
            Self::UpdateProfile(request) => formatter
                .debug_tuple("UiCommand::UpdateProfile")
                .field(request)
                .finish(),
            Self::TestConnection {
                operation_id,
                profile_id,
            } => formatter
                .debug_struct("UiCommand::TestConnection")
                .field("operation_id", operation_id)
                .field("profile_id", profile_id)
                .finish(),
            Self::Execute {
                operation_id,
                profile_id,
                language,
                row_limit,
                timeout_ms,
                ..
            } => formatter
                .debug_struct("UiCommand::Execute")
                .field("operation_id", operation_id)
                .field("profile_id", profile_id)
                .field("language", language)
                .field("text", &"<redacted>")
                .field("row_limit", row_limit)
                .field("timeout_ms", timeout_ms)
                .finish(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitError {
    Busy,
    Disconnected,
}

pub struct UiPort {
    command_tx: mpsc::Sender<UiCommand>,
    event_rx: mpsc::Receiver<UiEvent>,
}

impl UiPort {
    pub fn try_submit(&self, command: UiCommand) -> Result<(), SubmitError> {
        self.command_tx
            .try_send(command)
            .map_err(|error| match error {
                TrySendError::Full(_) => SubmitError::Busy,
                TrySendError::Closed(_) => SubmitError::Disconnected,
            })
    }

    pub fn try_submit_with(&self, build: impl FnOnce() -> UiCommand) -> Result<(), SubmitError> {
        let permit = self.command_tx.try_reserve().map_err(|error| match error {
            TrySendError::Full(()) => SubmitError::Busy,
            TrySendError::Closed(()) => SubmitError::Disconnected,
        })?;
        permit.send(build());
        Ok(())
    }

    pub fn drain_events(&mut self, limit: usize) -> Vec<UiEvent> {
        let mut events = Vec::with_capacity(limit.min(32));
        for _ in 0..limit {
            match self.event_rx.try_recv() {
                Ok(event) => events.push(event),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
            }
        }
        events
    }
}

pub struct ServicePort {
    command_rx: mpsc::Receiver<UiCommand>,
    event_tx: mpsc::Sender<UiEvent>,
}

impl ServicePort {
    pub async fn next_command(&mut self) -> Option<UiCommand> {
        self.command_rx.recv().await
    }

    pub async fn emit(&self, event: UiEvent) -> Result<(), UiEvent> {
        self.event_tx.send(event).await.map_err(|error| error.0)
    }

    #[cfg(test)]
    pub fn try_next_command(&mut self) -> Option<UiCommand> {
        self.command_rx.try_recv().ok()
    }
}

#[must_use]
pub fn bounded_ports(capacity: usize) -> (UiPort, ServicePort) {
    let capacity = capacity.max(1);
    let (command_tx, command_rx) = mpsc::channel(capacity);
    let (event_tx, event_rx) = mpsc::channel(capacity);
    (
        UiPort {
            command_tx,
            event_rx,
        },
        ServicePort {
            command_rx,
            event_tx,
        },
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{SubmitError, UiCommand, bounded_ports};

    #[test]
    fn full_command_channel_is_busy_instead_of_blocking() {
        let (ui, _service) = bounded_ports(1);
        assert_eq!(ui.try_submit(UiCommand::RefreshProfiles), Ok(()));
        assert_eq!(
            ui.try_submit(UiCommand::RefreshProfiles),
            Err(SubmitError::Busy)
        );
    }

    #[test]
    fn full_channel_does_not_build_or_move_a_sensitive_command() {
        let (ui, _service) = bounded_ports(1);
        assert_eq!(ui.try_submit(UiCommand::RefreshProfiles), Ok(()));
        let built = Arc::new(AtomicBool::new(false));
        let marker = built.clone();
        assert_eq!(
            ui.try_submit_with(move || {
                marker.store(true, Ordering::SeqCst);
                UiCommand::RefreshProfiles
            }),
            Err(SubmitError::Busy)
        );
        assert!(!built.load(Ordering::SeqCst));
    }
}
