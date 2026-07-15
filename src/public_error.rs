use serde::Serialize;

use crate::model::{
    DraftId, OperationId, OperationKind, OperationRecipeId, ProfileFieldId, ProfileId, PublicCode,
    PublicSummary, ResultId,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCategory {
    Validation,
    Authentication,
    Permission,
    Network,
    Tls,
    Timeout,
    Syntax,
    Constraint,
    Unsupported,
    Cancelled,
    Busy,
    Stale,
    Io,
    Internal,
}

impl From<PublicSummary> for ErrorCategory {
    fn from(summary: PublicSummary) -> Self {
        match summary {
            PublicSummary::InvalidInput | PublicSummary::CredentialRequired => Self::Validation,
            PublicSummary::AuthenticationFailed => Self::Authentication,
            PublicSummary::PermissionDenied => Self::Permission,
            PublicSummary::NetworkUnavailable => Self::Network,
            PublicSummary::TlsVerificationFailed => Self::Tls,
            PublicSummary::OperationTimedOut => Self::Timeout,
            PublicSummary::SyntaxRejected => Self::Syntax,
            PublicSummary::ConstraintRejected => Self::Constraint,
            PublicSummary::UnsupportedFeature => Self::Unsupported,
            PublicSummary::OperationCancelled => Self::Cancelled,
            PublicSummary::ResourceBusy => Self::Busy,
            PublicSummary::ResourceStale => Self::Stale,
            PublicSummary::ConfigWriteNotCommitted
            | PublicSummary::CommittedDurabilityUnknown
            | PublicSummary::ExportFailed => Self::Io,
            PublicSummary::InternalFailure => Self::Internal,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum RecoveryAction {
    OpenCredentialPrompt(ProfileId),
    EditDraft(DraftId, ProfileFieldId),
    EditProfile(ProfileId, ProfileFieldId),
    Retry(OperationRecipeId),
    FocusEditor(ProfileId),
    FocusExecuteLimits(ProfileId),
    ReloadConfiguration,
    Reconnect(ProfileId),
    CancelOperation(OperationId),
    ClearCatalog(ProfileId),
    RestartRedisScan(ProfileId),
    ChooseExportDestination(ResultId),
    RevealExportDestination(ResultId),
    RevealMigrationBackup,
    RestartApplication,
    DismissError(OperationId),
}

/// A typed, safe-id-only command emitted by a recovery button.
///
/// This is the P1 dispatch boundary: it performs no network or filesystem I/O,
/// and a platform/controller adapter must explicitly consume the command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryCommand {
    OpenCredentialEditor(ProfileId),
    FocusDraftField(DraftId, ProfileFieldId),
    FocusProfileField(ProfileId, ProfileFieldId),
    RetryRecipe(OperationRecipeId),
    FocusStatementEditor(ProfileId),
    FocusExecutionLimits(ProfileId),
    ReloadConfiguredPath,
    ReconnectProfile(ProfileId),
    CancelRunningOperation(OperationId),
    ClearProfileCatalog(ProfileId),
    RestartProfileRedisScan(ProfileId),
    ChooseResultExportDestination(ResultId),
    RevealResultExportDestination(ResultId),
    RevealConfiguredMigrationBackup,
    RestartApplication,
    DismissOperationError(OperationId),
}

pub trait RecoveryCommandDispatcher {
    type Error;

    fn dispatch(&mut self, command: RecoveryCommand) -> Result<(), Self::Error>;
}

pub fn dispatch_recovery<D: RecoveryCommandDispatcher>(
    action: RecoveryAction,
    dispatcher: &mut D,
) -> Result<(), D::Error> {
    let command = match action {
        RecoveryAction::OpenCredentialPrompt(profile_id) => {
            RecoveryCommand::OpenCredentialEditor(profile_id)
        }
        RecoveryAction::EditDraft(draft_id, field) => {
            RecoveryCommand::FocusDraftField(draft_id, field)
        }
        RecoveryAction::EditProfile(profile_id, field) => {
            RecoveryCommand::FocusProfileField(profile_id, field)
        }
        RecoveryAction::Retry(recipe_id) => RecoveryCommand::RetryRecipe(recipe_id),
        RecoveryAction::FocusEditor(profile_id) => {
            RecoveryCommand::FocusStatementEditor(profile_id)
        }
        RecoveryAction::FocusExecuteLimits(profile_id) => {
            RecoveryCommand::FocusExecutionLimits(profile_id)
        }
        RecoveryAction::ReloadConfiguration => RecoveryCommand::ReloadConfiguredPath,
        RecoveryAction::Reconnect(profile_id) => RecoveryCommand::ReconnectProfile(profile_id),
        RecoveryAction::CancelOperation(operation_id) => {
            RecoveryCommand::CancelRunningOperation(operation_id)
        }
        RecoveryAction::ClearCatalog(profile_id) => {
            RecoveryCommand::ClearProfileCatalog(profile_id)
        }
        RecoveryAction::RestartRedisScan(profile_id) => {
            RecoveryCommand::RestartProfileRedisScan(profile_id)
        }
        RecoveryAction::ChooseExportDestination(result_id) => {
            RecoveryCommand::ChooseResultExportDestination(result_id)
        }
        RecoveryAction::RevealExportDestination(result_id) => {
            RecoveryCommand::RevealResultExportDestination(result_id)
        }
        RecoveryAction::RevealMigrationBackup => RecoveryCommand::RevealConfiguredMigrationBackup,
        RecoveryAction::RestartApplication => RecoveryCommand::RestartApplication,
        RecoveryAction::DismissError(operation_id) => {
            RecoveryCommand::DismissOperationError(operation_id)
        }
    };
    dispatcher.dispatch(command)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct NonEmpty<T>(Vec<T>);

impl<T> NonEmpty<T> {
    pub fn new(head: T) -> Self {
        Self(vec![head])
    }

    pub fn push(&mut self, value: T) {
        self.0.push(value);
    }

    pub fn as_slice(&self) -> &[T] {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafeContext {
    Global {
        operation_id: OperationId,
    },
    Draft {
        draft_id: DraftId,
        operation_id: OperationId,
    },
    Profile {
        profile_id: ProfileId,
        operation_id: OperationId,
        active_operation: Option<OperationId>,
        recipe_id: Option<OperationRecipeId>,
    },
    Export {
        result_id: ResultId,
        operation_id: OperationId,
        destination_committed: bool,
    },
}

/// Runtime-only knowledge for the ResourceBusy row. Keeping this fact
/// separate preserves the frozen Draft identity `(DraftId, OperationId)`
/// while still allowing a controller that knows the active operation to
/// expose an exact Cancel action.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyFact {
    KnownActive(OperationId),
    UnknownActive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryFact {
    None,
    Busy(BusyFact),
}

impl SafeContext {
    pub const fn global(operation_id: OperationId) -> Self {
        Self::Global { operation_id }
    }

    pub const fn draft(draft_id: DraftId, operation_id: OperationId) -> Self {
        Self::Draft {
            draft_id,
            operation_id,
        }
    }

    pub fn profile(profile_id: ProfileId, operation_id: OperationId) -> Self {
        Self::Profile {
            profile_id,
            operation_id,
            active_operation: None,
            recipe_id: None,
        }
    }

    pub fn profile_with_recipe(
        profile_id: ProfileId,
        operation_id: OperationId,
        recipe_id: OperationRecipeId,
    ) -> Self {
        Self::Profile {
            profile_id,
            operation_id,
            active_operation: None,
            recipe_id: Some(recipe_id),
        }
    }

    pub fn profile_with_active(
        profile_id: ProfileId,
        operation_id: OperationId,
        active_operation: OperationId,
    ) -> Self {
        Self::Profile {
            profile_id,
            operation_id,
            active_operation: Some(active_operation),
            recipe_id: None,
        }
    }

    pub const fn export(
        result_id: ResultId,
        operation_id: OperationId,
        destination_committed: bool,
    ) -> Self {
        Self::Export {
            result_id,
            operation_id,
            destination_committed,
        }
    }

    const fn operation_id(&self) -> OperationId {
        match self {
            Self::Global { operation_id }
            | Self::Draft { operation_id, .. }
            | Self::Profile { operation_id, .. }
            | Self::Export { operation_id, .. } => *operation_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unreachable public recovery pair: {operation:?}/{summary:?}/{code:?}")]
pub struct UnreachableRecovery {
    pub operation: OperationKind,
    pub summary: PublicSummary,
    pub code: PublicCode,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PublicOperationError {
    pub operation: OperationKind,
    pub category: ErrorCategory,
    pub code: PublicCode,
    pub summary: PublicSummary,
    pub recovery: NonEmpty<RecoveryAction>,
}

impl std::fmt::Display for PublicOperationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.summary.fmt(formatter)
    }
}

impl std::error::Error for PublicOperationError {}

impl PublicOperationError {
    pub fn new(
        operation: OperationKind,
        summary: PublicSummary,
        code: PublicCode,
        context: &SafeContext,
    ) -> Result<Self, UnreachableRecovery> {
        let recovery = recovery_for(operation, summary, code, context)?;
        Ok(Self {
            operation,
            category: summary.into(),
            code,
            summary,
            recovery,
        })
    }

    /// Builds the requested public error, falling back to a safe internal row
    /// if a newly introduced service pair has not yet been added to the
    /// recovery matrix. This boundary never exposes the underlying error.
    pub fn new_or_internal(
        operation: OperationKind,
        summary: PublicSummary,
        code: PublicCode,
        context: &SafeContext,
    ) -> Self {
        Self::new(operation, summary, code, context).unwrap_or_else(|_| {
            let operation_id = context.operation_id();
            let mut recovery = NonEmpty::new(RecoveryAction::RestartApplication);
            recovery.push(RecoveryAction::DismissError(operation_id));
            Self {
                operation,
                category: ErrorCategory::Internal,
                code: PublicCode::None,
                summary: PublicSummary::InternalFailure,
                recovery,
            }
        })
    }
}

pub fn recovery_for(
    operation: OperationKind,
    summary: PublicSummary,
    code: PublicCode,
    context: &SafeContext,
) -> Result<NonEmpty<RecoveryAction>, UnreachableRecovery> {
    recovery_for_with_fact(operation, summary, code, context, RecoveryFact::None)
}

pub fn recovery_for_with_fact(
    operation: OperationKind,
    summary: PublicSummary,
    code: PublicCode,
    context: &SafeContext,
    fact: RecoveryFact,
) -> Result<NonEmpty<RecoveryAction>, UnreachableRecovery> {
    if !context_matches(operation, context) {
        return unreachable(operation, summary, code);
    }
    if !matches!(fact, RecoveryFact::None) && summary != PublicSummary::ResourceBusy {
        return unreachable(operation, summary, code);
    }
    let result = match summary {
        PublicSummary::InvalidInput => invalid_input(operation, code, context),
        PublicSummary::CredentialRequired => credential_required(operation, code, context),
        PublicSummary::AuthenticationFailed => authentication_failed(operation, code, context),
        PublicSummary::PermissionDenied => permission_denied(operation, code, context),
        PublicSummary::NetworkUnavailable => network_unavailable(operation, context),
        PublicSummary::TlsVerificationFailed => tls_failed(operation, code, context),
        PublicSummary::OperationTimedOut => timed_out(operation, context),
        PublicSummary::SyntaxRejected => execute_only(operation, context, false),
        PublicSummary::ConstraintRejected => execute_only(operation, context, true),
        PublicSummary::UnsupportedFeature => unsupported(operation, code, context),
        PublicSummary::OperationCancelled => cancelled(operation, context),
        PublicSummary::ResourceBusy => busy(operation, context, fact),
        PublicSummary::ResourceStale => stale(operation, context),
        PublicSummary::ConfigWriteNotCommitted => config_not_committed(operation, code, context),
        PublicSummary::CommittedDurabilityUnknown => durability_unknown(operation, code, context),
        PublicSummary::ExportFailed => export_failed(operation, context),
        PublicSummary::InternalFailure => internal(operation, context),
    };
    result.ok_or(UnreachableRecovery {
        operation,
        summary,
        code,
    })
}

fn invalid_input(
    operation: OperationKind,
    code: PublicCode,
    context: &SafeContext,
) -> Option<NonEmpty<RecoveryAction>> {
    match (operation, context) {
        (
            OperationKind::LoadConfiguration
            | OperationKind::ReloadConfiguration
            | OperationKind::MigrateConfiguration,
            SafeContext::Global { .. },
        ) => one(RecoveryAction::ReloadConfiguration),
        (OperationKind::CreateProfile, SafeContext::Draft { draft_id, .. }) => draft_field(code)
            .map(|field| NonEmpty::new(RecoveryAction::EditDraft(*draft_id, field))),
        (OperationKind::TestDraftConnection, SafeContext::Draft { draft_id, .. }) => {
            draft_field(code)
                .map(|field| NonEmpty::new(RecoveryAction::EditDraft(*draft_id, field)))
        }
        (OperationKind::UpdateProfile, SafeContext::Profile { profile_id, .. }) => match code {
            PublicCode::ProfileStale | PublicCode::ConfigExternalChange => {
                one(RecoveryAction::ReloadConfiguration)
            }
            _ => profile_field(code)
                .map(|field| NonEmpty::new(RecoveryAction::EditProfile(profile_id.clone(), field))),
        },
        (OperationKind::DeleteProfile, SafeContext::Profile { operation_id, .. }) => match code {
            PublicCode::ProfileStale | PublicCode::ConfigExternalChange => {
                one(RecoveryAction::ReloadConfiguration)
            }
            PublicCode::None => one(RecoveryAction::DismissError(*operation_id)),
            _ => None,
        },
        (
            OperationKind::ConnectProfile | OperationKind::ReconnectProfile,
            SafeContext::Profile { profile_id, .. },
        ) => profile_field(code)
            .map(|field| NonEmpty::new(RecoveryAction::EditProfile(profile_id.clone(), field))),
        (
            OperationKind::ExecuteRead | OperationKind::ExecuteMutation,
            SafeContext::Profile { profile_id, .. },
        ) => match code {
            PublicCode::RowLimit | PublicCode::TimeoutInput => {
                one(RecoveryAction::FocusExecuteLimits(profile_id.clone()))
            }
            PublicCode::StatementTarget
            | PublicCode::AmbiguousSqlMode
            | PublicCode::UnterminatedSqlToken
            | PublicCode::None => one(RecoveryAction::FocusEditor(profile_id.clone())),
            _ => None,
        },
        (
            OperationKind::BrowseMySql,
            SafeContext::Profile {
                profile_id,
                operation_id,
                ..
            },
        ) => match code {
            PublicCode::Catalog => one(RecoveryAction::ClearCatalog(profile_id.clone())),
            PublicCode::None => one(RecoveryAction::DismissError(*operation_id)),
            _ => None,
        },
        (
            OperationKind::BrowseRedis,
            SafeContext::Profile {
                profile_id,
                operation_id,
                ..
            },
        ) => match code {
            PublicCode::RedisScan => one(RecoveryAction::RestartRedisScan(profile_id.clone())),
            PublicCode::None => one(RecoveryAction::DismissError(*operation_id)),
            _ => None,
        },
        (OperationKind::InspectRedis, SafeContext::Profile { operation_id, .. }) => {
            one(RecoveryAction::DismissError(*operation_id))
        }
        (OperationKind::ExportResult, SafeContext::Export { result_id, .. }) => {
            one(RecoveryAction::ChooseExportDestination(*result_id))
        }
        _ => None,
    }
}

fn credential_required(
    operation: OperationKind,
    code: PublicCode,
    context: &SafeContext,
) -> Option<NonEmpty<RecoveryAction>> {
    match (operation, context) {
        (OperationKind::TestDraftConnection, SafeContext::Draft { draft_id, .. }) => {
            credential_field(code)
                .map(|field| NonEmpty::new(RecoveryAction::EditDraft(*draft_id, field)))
        }
        (kind, SafeContext::Profile { profile_id, .. }) if is_credential_operation(kind) => {
            let mut actions =
                NonEmpty::new(RecoveryAction::OpenCredentialPrompt(profile_id.clone()));
            actions.push(RecoveryAction::EditProfile(
                profile_id.clone(),
                ProfileFieldId::SessionCredential,
            ));
            Some(actions)
        }
        _ => None,
    }
}

fn authentication_failed(
    operation: OperationKind,
    code: PublicCode,
    context: &SafeContext,
) -> Option<NonEmpty<RecoveryAction>> {
    match (operation, context) {
        (OperationKind::TestDraftConnection, SafeContext::Draft { draft_id, .. }) => {
            credential_field(code)
                .map(|field| NonEmpty::new(RecoveryAction::EditDraft(*draft_id, field)))
        }
        (
            kind,
            SafeContext::Profile {
                profile_id,
                recipe_id,
                ..
            },
        ) if is_credential_operation(kind) => {
            let first = match code {
                PublicCode::SessionCredential => {
                    RecoveryAction::OpenCredentialPrompt(profile_id.clone())
                }
                PublicCode::CredentialEnvironmentName => RecoveryAction::EditProfile(
                    profile_id.clone(),
                    ProfileFieldId::CredentialEnvironmentName,
                ),
                PublicCode::Username => {
                    RecoveryAction::EditProfile(profile_id.clone(), ProfileFieldId::Username)
                }
                _ => return None,
            };
            let mut actions = NonEmpty::new(first);
            if let Some(recipe_id) = recipe_id.filter(|_| is_idempotent(kind)) {
                actions.push(RecoveryAction::Retry(recipe_id));
            }
            Some(actions)
        }
        _ => None,
    }
}

fn permission_denied(
    operation: OperationKind,
    code: PublicCode,
    context: &SafeContext,
) -> Option<NonEmpty<RecoveryAction>> {
    let field = permission_field(code)?;
    match (operation, context) {
        (
            OperationKind::TestDraftConnection,
            SafeContext::Draft {
                draft_id,
                operation_id,
                ..
            },
        ) => {
            let mut actions = NonEmpty::new(RecoveryAction::EditDraft(*draft_id, field));
            actions.push(RecoveryAction::DismissError(*operation_id));
            Some(actions)
        }
        (
            OperationKind::ConnectProfile
            | OperationKind::ReconnectProfile
            | OperationKind::BrowseMySql
            | OperationKind::BrowseRedis
            | OperationKind::InspectRedis,
            SafeContext::Profile {
                profile_id,
                operation_id,
                ..
            },
        ) => {
            let mut actions = NonEmpty::new(RecoveryAction::EditProfile(profile_id.clone(), field));
            actions.push(RecoveryAction::DismissError(*operation_id));
            Some(actions)
        }
        (
            OperationKind::ExecuteRead | OperationKind::ExecuteMutation,
            SafeContext::Profile {
                profile_id,
                operation_id,
                ..
            },
        ) => {
            let mut actions = NonEmpty::new(RecoveryAction::FocusEditor(profile_id.clone()));
            actions.push(RecoveryAction::DismissError(*operation_id));
            Some(actions)
        }
        _ => None,
    }
}

fn network_unavailable(
    operation: OperationKind,
    context: &SafeContext,
) -> Option<NonEmpty<RecoveryAction>> {
    match (operation, context) {
        (
            OperationKind::TestDraftConnection,
            SafeContext::Draft {
                draft_id,
                operation_id,
                ..
            },
        ) => {
            let mut actions =
                NonEmpty::new(RecoveryAction::EditDraft(*draft_id, ProfileFieldId::Host));
            actions.push(RecoveryAction::DismissError(*operation_id));
            Some(actions)
        }
        (
            kind,
            SafeContext::Profile {
                profile_id,
                recipe_id,
                ..
            },
        ) if is_saved_network(kind) && kind != OperationKind::DisconnectProfile => {
            let mut actions = NonEmpty::new(RecoveryAction::EditProfile(
                profile_id.clone(),
                ProfileFieldId::Host,
            ));
            actions.push(RecoveryAction::Reconnect(profile_id.clone()));
            if kind != OperationKind::ExecuteMutation
                && let Some(recipe_id) = recipe_id.filter(|_| is_idempotent(kind))
            {
                actions.push(RecoveryAction::Retry(recipe_id));
            }
            Some(actions)
        }
        _ => None,
    }
}

fn tls_failed(
    operation: OperationKind,
    code: PublicCode,
    context: &SafeContext,
) -> Option<NonEmpty<RecoveryAction>> {
    let field = match code {
        PublicCode::RedisTlsCaInvalidPem | PublicCode::RedisTlsCaUntrustedIssuer => {
            ProfileFieldId::RedisCaFile
        }
        PublicCode::TlsHostnameMismatch => ProfileFieldId::Host,
        _ => return None,
    };
    match (operation, context) {
        (OperationKind::TestDraftConnection, SafeContext::Draft { draft_id, .. }) => {
            one(RecoveryAction::EditDraft(*draft_id, field))
        }
        (kind, SafeContext::Profile { profile_id, .. }) if is_credential_operation(kind) => {
            one(RecoveryAction::EditProfile(profile_id.clone(), field))
        }
        _ => None,
    }
}

fn timed_out(operation: OperationKind, context: &SafeContext) -> Option<NonEmpty<RecoveryAction>> {
    match (operation, context) {
        (
            OperationKind::TestDraftConnection,
            SafeContext::Draft {
                draft_id,
                operation_id,
                ..
            },
        ) => {
            let mut actions =
                NonEmpty::new(RecoveryAction::EditDraft(*draft_id, ProfileFieldId::Host));
            actions.push(RecoveryAction::DismissError(*operation_id));
            Some(actions)
        }
        (
            OperationKind::ConnectProfile | OperationKind::ReconnectProfile,
            SafeContext::Profile {
                profile_id,
                operation_id,
                ..
            },
        ) => {
            let mut actions = NonEmpty::new(RecoveryAction::EditProfile(
                profile_id.clone(),
                ProfileFieldId::Host,
            ));
            actions.push(RecoveryAction::Reconnect(profile_id.clone()));
            actions.push(RecoveryAction::DismissError(*operation_id));
            Some(actions)
        }
        (
            OperationKind::ExecuteRead | OperationKind::ExecuteMutation,
            SafeContext::Profile { profile_id, .. },
        ) => {
            let mut actions = NonEmpty::new(RecoveryAction::FocusExecuteLimits(profile_id.clone()));
            actions.push(RecoveryAction::Reconnect(profile_id.clone()));
            Some(actions)
        }
        (
            OperationKind::BrowseMySql | OperationKind::BrowseRedis | OperationKind::InspectRedis,
            SafeContext::Profile {
                profile_id,
                operation_id,
                recipe_id,
                ..
            },
        ) => {
            let mut actions = match recipe_id {
                Some(recipe_id) => NonEmpty::new(RecoveryAction::Retry(*recipe_id)),
                None => NonEmpty::new(RecoveryAction::Reconnect(profile_id.clone())),
            };
            actions.push(RecoveryAction::DismissError(*operation_id));
            Some(actions)
        }
        _ => None,
    }
}

fn execute_only(
    operation: OperationKind,
    context: &SafeContext,
    dismiss: bool,
) -> Option<NonEmpty<RecoveryAction>> {
    match (operation, context) {
        (
            OperationKind::ExecuteRead | OperationKind::ExecuteMutation,
            SafeContext::Profile {
                profile_id,
                operation_id,
                ..
            },
        ) => {
            let mut actions = NonEmpty::new(RecoveryAction::FocusEditor(profile_id.clone()));
            if dismiss {
                actions.push(RecoveryAction::DismissError(*operation_id));
            }
            Some(actions)
        }
        _ => None,
    }
}

fn unsupported(
    operation: OperationKind,
    code: PublicCode,
    context: &SafeContext,
) -> Option<NonEmpty<RecoveryAction>> {
    match (operation, context, code) {
        (
            OperationKind::TestDraftConnection,
            SafeContext::Draft { draft_id, .. },
            PublicCode::RedisTlsPreferredLegacy,
        ) => one(RecoveryAction::EditDraft(
            *draft_id,
            ProfileFieldId::RedisTlsMode,
        )),
        (
            OperationKind::ConnectProfile,
            SafeContext::Profile { profile_id, .. },
            PublicCode::RedisTlsPreferredLegacy,
        ) => one(RecoveryAction::EditProfile(
            profile_id.clone(),
            ProfileFieldId::RedisTlsMode,
        )),
        (
            OperationKind::ExecuteRead | OperationKind::ExecuteMutation,
            SafeContext::Profile {
                profile_id,
                operation_id,
                ..
            },
            PublicCode::PreparedStatementUnsupported,
        ) => {
            let mut actions = NonEmpty::new(RecoveryAction::FocusEditor(profile_id.clone()));
            actions.push(RecoveryAction::DismissError(*operation_id));
            Some(actions)
        }
        (
            OperationKind::ExecuteRead
            | OperationKind::ExecuteMutation
            | OperationKind::BrowseMySql
            | OperationKind::BrowseRedis
            | OperationKind::InspectRedis,
            SafeContext::Profile { operation_id, .. },
            _,
        ) => one(RecoveryAction::DismissError(*operation_id)),
        _ => None,
    }
}

fn cancelled(operation: OperationKind, context: &SafeContext) -> Option<NonEmpty<RecoveryAction>> {
    match (operation, context) {
        (OperationKind::TestDraftConnection, SafeContext::Draft { operation_id, .. }) => {
            one(RecoveryAction::DismissError(*operation_id))
        }
        (
            kind,
            SafeContext::Profile {
                profile_id,
                operation_id,
                ..
            },
        ) if is_saved_network(kind) && kind != OperationKind::DisconnectProfile => {
            let mut actions = NonEmpty::new(RecoveryAction::Reconnect(profile_id.clone()));
            actions.push(RecoveryAction::DismissError(*operation_id));
            Some(actions)
        }
        (
            OperationKind::ExportResult,
            SafeContext::Export {
                result_id,
                operation_id,
                destination_committed,
            },
        ) => {
            let first = if *destination_committed {
                RecoveryAction::RevealExportDestination(*result_id)
            } else {
                RecoveryAction::ChooseExportDestination(*result_id)
            };
            let mut actions = NonEmpty::new(first);
            actions.push(RecoveryAction::DismissError(*operation_id));
            Some(actions)
        }
        _ => None,
    }
}

fn busy(
    operation: OperationKind,
    context: &SafeContext,
    fact: RecoveryFact,
) -> Option<NonEmpty<RecoveryAction>> {
    if matches!(
        operation,
        OperationKind::LoadConfiguration | OperationKind::ShutdownRuntime
    ) {
        return None;
    }
    let (context_active, rejected) = match context {
        SafeContext::Profile {
            active_operation,
            operation_id,
            ..
        } => (*active_operation, *operation_id),
        SafeContext::Global { operation_id }
        | SafeContext::Draft { operation_id, .. }
        | SafeContext::Export { operation_id, .. } => (None, *operation_id),
    };
    let active = match fact {
        RecoveryFact::None => context_active,
        RecoveryFact::Busy(BusyFact::KnownActive(operation_id)) => Some(operation_id),
        RecoveryFact::Busy(BusyFact::UnknownActive) => None,
    };
    let mut actions = match active {
        Some(active) => NonEmpty::new(RecoveryAction::CancelOperation(active)),
        None => NonEmpty::new(RecoveryAction::DismissError(rejected)),
    };
    if active.is_some() {
        actions.push(RecoveryAction::DismissError(rejected));
    }
    Some(actions)
}

fn stale(operation: OperationKind, context: &SafeContext) -> Option<NonEmpty<RecoveryAction>> {
    match (operation, context) {
        (
            OperationKind::ReloadConfiguration
            | OperationKind::CreateProfile
            | OperationKind::UpdateProfile
            | OperationKind::DeleteProfile,
            _,
        ) => one(RecoveryAction::ReloadConfiguration),
        (
            OperationKind::ConnectProfile
            | OperationKind::ReconnectProfile
            | OperationKind::BrowseMySql
            | OperationKind::BrowseRedis
            | OperationKind::InspectRedis,
            SafeContext::Profile {
                recipe_id: Some(recipe_id),
                ..
            },
        ) => one(RecoveryAction::Retry(*recipe_id)),
        (
            OperationKind::ExecuteRead | OperationKind::ExecuteMutation,
            SafeContext::Profile { profile_id, .. },
        ) => {
            let mut actions = NonEmpty::new(RecoveryAction::ReloadConfiguration);
            actions.push(RecoveryAction::FocusEditor(profile_id.clone()));
            Some(actions)
        }
        _ => None,
    }
}

fn config_not_committed(
    operation: OperationKind,
    code: PublicCode,
    _context: &SafeContext,
) -> Option<NonEmpty<RecoveryAction>> {
    if !matches!(
        operation,
        OperationKind::MigrateConfiguration
            | OperationKind::CreateProfile
            | OperationKind::UpdateProfile
            | OperationKind::DeleteProfile
    ) {
        return None;
    }
    let mut actions = NonEmpty::new(RecoveryAction::ReloadConfiguration);
    if operation == OperationKind::MigrateConfiguration
        && code == PublicCode::MigrationBackupAvailable
    {
        actions.push(RecoveryAction::RevealMigrationBackup);
    }
    Some(actions)
}

fn durability_unknown(
    operation: OperationKind,
    code: PublicCode,
    context: &SafeContext,
) -> Option<NonEmpty<RecoveryAction>> {
    if matches!(
        operation,
        OperationKind::MigrateConfiguration
            | OperationKind::CreateProfile
            | OperationKind::UpdateProfile
            | OperationKind::DeleteProfile
    ) {
        let mut actions = NonEmpty::new(RecoveryAction::ReloadConfiguration);
        if operation == OperationKind::MigrateConfiguration
            && code == PublicCode::MigrationBackupAvailable
        {
            actions.push(RecoveryAction::RevealMigrationBackup);
        }
        return Some(actions);
    }
    match (operation, context) {
        (OperationKind::ExportResult, SafeContext::Export { result_id, .. }) => {
            one(RecoveryAction::RevealExportDestination(*result_id))
        }
        _ => None,
    }
}

fn export_failed(
    operation: OperationKind,
    context: &SafeContext,
) -> Option<NonEmpty<RecoveryAction>> {
    match (operation, context) {
        (
            OperationKind::ExportResult,
            SafeContext::Export {
                result_id,
                destination_committed,
                ..
            },
        ) => {
            let mut actions = NonEmpty::new(RecoveryAction::ChooseExportDestination(*result_id));
            if *destination_committed {
                actions.push(RecoveryAction::RevealExportDestination(*result_id));
            }
            Some(actions)
        }
        _ => None,
    }
}

fn internal(operation: OperationKind, context: &SafeContext) -> Option<NonEmpty<RecoveryAction>> {
    let operation_id = context.operation_id();
    let mut actions = NonEmpty::new(RecoveryAction::RestartApplication);
    actions.push(RecoveryAction::DismissError(operation_id));
    if matches!(
        operation,
        OperationKind::LoadConfiguration
            | OperationKind::ReloadConfiguration
            | OperationKind::MigrateConfiguration
            | OperationKind::CreateProfile
            | OperationKind::UpdateProfile
            | OperationKind::DeleteProfile
    ) {
        actions.push(RecoveryAction::ReloadConfiguration);
    }
    Some(actions)
}

fn context_matches(operation: OperationKind, context: &SafeContext) -> bool {
    match operation {
        OperationKind::LoadConfiguration
        | OperationKind::ReloadConfiguration
        | OperationKind::MigrateConfiguration
        | OperationKind::ShutdownRuntime => matches!(context, SafeContext::Global { .. }),
        OperationKind::CreateProfile | OperationKind::TestDraftConnection => {
            matches!(context, SafeContext::Draft { .. })
        }
        OperationKind::ExportResult => matches!(context, SafeContext::Export { .. }),
        OperationKind::UpdateProfile
        | OperationKind::DeleteProfile
        | OperationKind::ConnectProfile
        | OperationKind::DisconnectProfile
        | OperationKind::ReconnectProfile
        | OperationKind::ExecuteRead
        | OperationKind::ExecuteMutation
        | OperationKind::BrowseMySql
        | OperationKind::BrowseRedis
        | OperationKind::InspectRedis => matches!(context, SafeContext::Profile { .. }),
    }
}

fn draft_field(code: PublicCode) -> Option<ProfileFieldId> {
    match code {
        PublicCode::ProfileIdConflict => Some(ProfileFieldId::ConnectionId),
        PublicCode::Field(field) => Some(field),
        PublicCode::SessionCredential => Some(ProfileFieldId::SessionCredential),
        PublicCode::CredentialEnvironmentName => Some(ProfileFieldId::CredentialEnvironmentName),
        PublicCode::Username => Some(ProfileFieldId::Username),
        PublicCode::Database => Some(ProfileFieldId::Database),
        PublicCode::RedisTlsPreferredLegacy => Some(ProfileFieldId::RedisTlsMode),
        PublicCode::RedisTlsCaInvalidPem | PublicCode::RedisTlsCaUntrustedIssuer => {
            Some(ProfileFieldId::RedisCaFile)
        }
        PublicCode::TlsHostnameMismatch => Some(ProfileFieldId::Host),
        _ => None,
    }
}

fn profile_field(code: PublicCode) -> Option<ProfileFieldId> {
    match code {
        PublicCode::Field(field) => Some(field),
        PublicCode::SessionCredential => Some(ProfileFieldId::SessionCredential),
        PublicCode::CredentialEnvironmentName => Some(ProfileFieldId::CredentialEnvironmentName),
        PublicCode::Username => Some(ProfileFieldId::Username),
        PublicCode::Database => Some(ProfileFieldId::Database),
        PublicCode::RedisTlsPreferredLegacy => Some(ProfileFieldId::RedisTlsMode),
        PublicCode::RedisTlsCaInvalidPem | PublicCode::RedisTlsCaUntrustedIssuer => {
            Some(ProfileFieldId::RedisCaFile)
        }
        PublicCode::TlsHostnameMismatch => Some(ProfileFieldId::Host),
        _ => None,
    }
}

fn credential_field(code: PublicCode) -> Option<ProfileFieldId> {
    match code {
        PublicCode::SessionCredential => Some(ProfileFieldId::SessionCredential),
        PublicCode::CredentialEnvironmentName => Some(ProfileFieldId::CredentialEnvironmentName),
        PublicCode::Username => Some(ProfileFieldId::Username),
        _ => None,
    }
}

fn permission_field(code: PublicCode) -> Option<ProfileFieldId> {
    match code {
        PublicCode::Username => Some(ProfileFieldId::Username),
        PublicCode::Database => Some(ProfileFieldId::Database),
        _ => None,
    }
}

fn is_saved_network(operation: OperationKind) -> bool {
    matches!(
        operation,
        OperationKind::ConnectProfile
            | OperationKind::DisconnectProfile
            | OperationKind::ReconnectProfile
            | OperationKind::ExecuteRead
            | OperationKind::ExecuteMutation
            | OperationKind::BrowseMySql
            | OperationKind::BrowseRedis
            | OperationKind::InspectRedis
    )
}

fn is_credential_operation(operation: OperationKind) -> bool {
    matches!(
        operation,
        OperationKind::ConnectProfile
            | OperationKind::ReconnectProfile
            | OperationKind::ExecuteRead
            | OperationKind::ExecuteMutation
            | OperationKind::BrowseMySql
            | OperationKind::BrowseRedis
            | OperationKind::InspectRedis
    )
}

fn is_idempotent(operation: OperationKind) -> bool {
    matches!(
        operation,
        OperationKind::ConnectProfile
            | OperationKind::ReconnectProfile
            | OperationKind::BrowseMySql
            | OperationKind::BrowseRedis
            | OperationKind::InspectRedis
    )
}

fn one(action: RecoveryAction) -> Option<NonEmpty<RecoveryAction>> {
    Some(NonEmpty::new(action))
}

fn unreachable<T>(
    operation: OperationKind,
    summary: PublicSummary,
    code: PublicCode,
) -> Result<T, UnreachableRecovery> {
    Err(UnreachableRecovery {
        operation,
        summary,
        code,
    })
}
