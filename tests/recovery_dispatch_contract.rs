use std::convert::Infallible;

use dbotter::model::{
    DraftId, OperationId, OperationKind, OperationRecipeId, ProfileFieldId, ProfileId, PublicCode,
    PublicSummary, ResultId,
};
use dbotter::public_error::{
    BusyFact, RecoveryAction, RecoveryCommand, RecoveryCommandDispatcher, RecoveryFact,
    SafeContext, dispatch_recovery, recovery_for, recovery_for_with_fact,
};

#[derive(Default)]
struct Recorder {
    commands: Vec<RecoveryCommand>,
}

#[test]
fn known_active_draft_busy_row_dispatches_cancel_then_rejected_dismiss() {
    let rejected = OperationId(70);
    let active = OperationId(71);
    let actions = recovery_for_with_fact(
        OperationKind::TestDraftConnection,
        PublicSummary::ResourceBusy,
        PublicCode::None,
        &SafeContext::draft(DraftId(72), rejected),
        RecoveryFact::Busy(BusyFact::KnownActive(active)),
    )
    .expect("known-active busy row");
    let mut recorder = Recorder::default();

    for action in actions.as_slice().iter().cloned() {
        dispatch_recovery(action, &mut recorder).expect("infallible dispatch");
    }

    assert_eq!(
        recorder.commands,
        vec![
            RecoveryCommand::CancelRunningOperation(active),
            RecoveryCommand::DismissOperationError(rejected),
        ]
    );
}

#[test]
fn cancelled_disconnect_is_unreachable_and_dispatches_nothing() {
    let recorder = Recorder::default();
    let result = recovery_for(
        OperationKind::DisconnectProfile,
        PublicSummary::OperationCancelled,
        PublicCode::None,
        &SafeContext::profile(ProfileId("disconnect".to_owned()), OperationId(80)),
    );

    assert!(result.is_err());
    assert!(recorder.commands.is_empty());
}

#[test]
fn startup_load_busy_is_unreachable_and_dispatches_nothing() {
    let recorder = Recorder::default();
    let result = recovery_for(
        OperationKind::LoadConfiguration,
        PublicSummary::ResourceBusy,
        PublicCode::None,
        &SafeContext::global(OperationId(81)),
    );

    assert!(result.is_err());
    assert!(recorder.commands.is_empty());
}

impl RecoveryCommandDispatcher for Recorder {
    type Error = Infallible;

    fn dispatch(&mut self, command: RecoveryCommand) -> Result<(), Self::Error> {
        self.commands.push(command);
        Ok(())
    }
}

#[test]
fn every_recovery_action_dispatches_one_exact_safe_id_command() {
    let profile = ProfileId("safe-profile".to_owned());
    let cases = [
        (
            RecoveryAction::OpenCredentialPrompt(profile.clone()),
            RecoveryCommand::OpenCredentialEditor(profile.clone()),
        ),
        (
            RecoveryAction::EditDraft(DraftId(1), ProfileFieldId::Host),
            RecoveryCommand::FocusDraftField(DraftId(1), ProfileFieldId::Host),
        ),
        (
            RecoveryAction::EditProfile(profile.clone(), ProfileFieldId::Port),
            RecoveryCommand::FocusProfileField(profile.clone(), ProfileFieldId::Port),
        ),
        (
            RecoveryAction::Retry(OperationRecipeId(2)),
            RecoveryCommand::RetryRecipe(OperationRecipeId(2)),
        ),
        (
            RecoveryAction::FocusEditor(profile.clone()),
            RecoveryCommand::FocusStatementEditor(profile.clone()),
        ),
        (
            RecoveryAction::FocusExecuteLimits(profile.clone()),
            RecoveryCommand::FocusExecutionLimits(profile.clone()),
        ),
        (
            RecoveryAction::ReloadConfiguration,
            RecoveryCommand::ReloadConfiguredPath,
        ),
        (
            RecoveryAction::Reconnect(profile.clone()),
            RecoveryCommand::ReconnectProfile(profile.clone()),
        ),
        (
            RecoveryAction::CancelOperation(OperationId(3)),
            RecoveryCommand::CancelRunningOperation(OperationId(3)),
        ),
        (
            RecoveryAction::ClearCatalog(profile.clone()),
            RecoveryCommand::ClearProfileCatalog(profile.clone()),
        ),
        (
            RecoveryAction::RestartRedisScan(profile.clone()),
            RecoveryCommand::RestartProfileRedisScan(profile.clone()),
        ),
        (
            RecoveryAction::ChooseExportDestination(ResultId(4)),
            RecoveryCommand::ChooseResultExportDestination(ResultId(4)),
        ),
        (
            RecoveryAction::RevealExportDestination(ResultId(5)),
            RecoveryCommand::RevealResultExportDestination(ResultId(5)),
        ),
        (
            RecoveryAction::RevealMigrationBackup,
            RecoveryCommand::RevealConfiguredMigrationBackup,
        ),
        (
            RecoveryAction::RestartApplication,
            RecoveryCommand::RestartApplication,
        ),
        (
            RecoveryAction::DismissError(OperationId(6)),
            RecoveryCommand::DismissOperationError(OperationId(6)),
        ),
    ];
    let mut recorder = Recorder::default();

    for (action, expected) in cases {
        dispatch_recovery(action, &mut recorder).expect("recording dispatch is infallible");
        assert_eq!(recorder.commands.last(), Some(&expected));
    }
    assert_eq!(recorder.commands.len(), 16);
}
