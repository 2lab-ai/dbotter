//! Bounded, nonblocking seam between the platform UI and background runtime.

use tokio::sync::mpsc::{self, error::TryRecvError, error::TrySendError};

use crate::model::{ConnectionProfile, OperationId, ProfileId, QueryLanguage};

use super::model::UiEvent;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiCommand {
    RefreshProfiles,
    UpsertProfile {
        operation_id: OperationId,
        profile: ConnectionProfile,
    },
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
}
