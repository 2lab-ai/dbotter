use dbotter::model::{
    DraftId, MySqlPublicErrorCode, OperationId, OperationKind, OperationRecipeId, ProfileFieldId,
    ProfileId, PublicCode, PublicSummary, RedisPublicErrorKind, ResultId,
};
use dbotter::public_error::{
    BusyFact, PublicOperationError, RecoveryAction, RecoveryFact, SafeContext, UnreachableRecovery,
    recovery_for, recovery_for_with_fact,
};
use std::collections::HashSet;

#[test]
fn recovery_conversion_is_total_over_closed_enums() {
    for operation in OperationKind::ALL {
        for summary in PublicSummary::ALL {
            let context = context_for(*operation);
            let result = recovery_for(*operation, *summary, PublicCode::None, &context);
            assert!(matches!(result, Ok(_) | Err(UnreachableRecovery { .. })));
            if let Ok(actions) = result {
                assert!(!actions.as_slice().is_empty());
            }
        }
    }
}

#[test]
fn independent_reachability_table_matches_every_operation_summary_pair() {
    let expected = expected_reachable_pairs();
    for operation in OperationKind::ALL {
        for summary in PublicSummary::ALL {
            let code = canonical_code(*operation, *summary);
            let context = matrix_context(*operation, *summary);
            let actual = recovery_for(*operation, *summary, code, &context).is_ok();
            assert_eq!(
                actual,
                expected.contains(&(*operation, *summary)),
                "operation={operation:?} summary={summary:?} code={code:?}"
            );
        }
    }
}

#[test]
fn canonical_validation_and_identity_rows_have_exact_actions() {
    let draft_id = DraftId(101);
    let operation_id = OperationId(102);
    let profile_id = ProfileId("canonical".to_owned());
    let draft = SafeContext::draft(draft_id, operation_id);
    let profile = SafeContext::profile(profile_id.clone(), operation_id);

    for operation in [
        OperationKind::LoadConfiguration,
        OperationKind::ReloadConfiguration,
        OperationKind::MigrateConfiguration,
    ] {
        assert_exact(
            operation,
            PublicSummary::InvalidInput,
            PublicCode::None,
            &SafeContext::global(operation_id),
            &[RecoveryAction::ReloadConfiguration],
        );
    }
    assert_exact(
        OperationKind::CreateProfile,
        PublicSummary::InvalidInput,
        PublicCode::ProfileIdConflict,
        &draft,
        &[RecoveryAction::EditDraft(
            draft_id,
            ProfileFieldId::ConnectionId,
        )],
    );
    assert_exact(
        OperationKind::UpdateProfile,
        PublicSummary::InvalidInput,
        PublicCode::Field(ProfileFieldId::Host),
        &profile,
        &[RecoveryAction::EditProfile(
            profile_id.clone(),
            ProfileFieldId::Host,
        )],
    );
    for code in [PublicCode::ProfileStale, PublicCode::ConfigExternalChange] {
        assert_exact(
            OperationKind::UpdateProfile,
            PublicSummary::InvalidInput,
            code,
            &profile,
            &[RecoveryAction::ReloadConfiguration],
        );
        assert_exact(
            OperationKind::DeleteProfile,
            PublicSummary::InvalidInput,
            code,
            &profile,
            &[RecoveryAction::ReloadConfiguration],
        );
    }
    assert_exact(
        OperationKind::DeleteProfile,
        PublicSummary::InvalidInput,
        PublicCode::None,
        &profile,
        &[RecoveryAction::DismissError(operation_id)],
    );
    assert_exact(
        OperationKind::TestDraftConnection,
        PublicSummary::InvalidInput,
        PublicCode::Field(ProfileFieldId::Port),
        &draft,
        &[RecoveryAction::EditDraft(draft_id, ProfileFieldId::Port)],
    );
    for operation in [
        OperationKind::ConnectProfile,
        OperationKind::ReconnectProfile,
    ] {
        assert_exact(
            operation,
            PublicSummary::InvalidInput,
            PublicCode::Field(ProfileFieldId::Database),
            &profile,
            &[RecoveryAction::EditProfile(
                profile_id.clone(),
                ProfileFieldId::Database,
            )],
        );
    }
    for operation in [OperationKind::ExecuteRead, OperationKind::ExecuteMutation] {
        for code in [
            PublicCode::StatementTarget,
            PublicCode::AmbiguousSqlMode,
            PublicCode::UnterminatedSqlToken,
        ] {
            assert_exact(
                operation,
                PublicSummary::InvalidInput,
                code,
                &profile,
                &[RecoveryAction::FocusEditor(profile_id.clone())],
            );
        }
        for code in [PublicCode::RowLimit, PublicCode::TimeoutInput] {
            assert_exact(
                operation,
                PublicSummary::InvalidInput,
                code,
                &profile,
                &[RecoveryAction::FocusExecuteLimits(profile_id.clone())],
            );
        }
    }
    for (operation, code, expected) in [
        (
            OperationKind::BrowseMySql,
            PublicCode::Catalog,
            RecoveryAction::ClearCatalog(profile_id.clone()),
        ),
        (
            OperationKind::BrowseRedis,
            PublicCode::RedisScan,
            RecoveryAction::RestartRedisScan(profile_id.clone()),
        ),
    ] {
        assert_exact(
            operation,
            PublicSummary::InvalidInput,
            code,
            &profile,
            &[expected],
        );
        assert_exact(
            operation,
            PublicSummary::InvalidInput,
            PublicCode::None,
            &profile,
            &[RecoveryAction::DismissError(operation_id)],
        );
    }
    assert_exact(
        OperationKind::InspectRedis,
        PublicSummary::InvalidInput,
        PublicCode::None,
        &profile,
        &[RecoveryAction::DismissError(operation_id)],
    );
    assert_exact(
        OperationKind::ExportResult,
        PublicSummary::InvalidInput,
        PublicCode::ExportDestination,
        &SafeContext::export(ResultId(103), operation_id, false),
        &[RecoveryAction::ChooseExportDestination(ResultId(103))],
    );
}

#[test]
fn canonical_credential_authentication_and_permission_rows_have_exact_actions() {
    let draft_id = DraftId(111);
    let operation_id = OperationId(112);
    let profile_id = ProfileId("credential".to_owned());
    let draft = SafeContext::draft(draft_id, operation_id);
    let profile = SafeContext::profile(profile_id.clone(), operation_id);
    let saved_operations = [
        OperationKind::ConnectProfile,
        OperationKind::ReconnectProfile,
        OperationKind::ExecuteRead,
        OperationKind::ExecuteMutation,
        OperationKind::BrowseMySql,
        OperationKind::BrowseRedis,
        OperationKind::InspectRedis,
    ];

    for (code, field) in [
        (
            PublicCode::SessionCredential,
            ProfileFieldId::SessionCredential,
        ),
        (
            PublicCode::CredentialEnvironmentName,
            ProfileFieldId::CredentialEnvironmentName,
        ),
        (PublicCode::Username, ProfileFieldId::Username),
    ] {
        assert_exact(
            OperationKind::TestDraftConnection,
            PublicSummary::CredentialRequired,
            code,
            &draft,
            &[RecoveryAction::EditDraft(draft_id, field)],
        );
        assert_exact(
            OperationKind::TestDraftConnection,
            PublicSummary::AuthenticationFailed,
            code,
            &draft,
            &[RecoveryAction::EditDraft(draft_id, field)],
        );
    }
    for operation in saved_operations {
        assert_exact(
            operation,
            PublicSummary::CredentialRequired,
            PublicCode::SessionCredential,
            &profile,
            &[
                RecoveryAction::OpenCredentialPrompt(profile_id.clone()),
                RecoveryAction::EditProfile(profile_id.clone(), ProfileFieldId::SessionCredential),
            ],
        );
        for (code, expected) in [
            (
                PublicCode::SessionCredential,
                RecoveryAction::OpenCredentialPrompt(profile_id.clone()),
            ),
            (
                PublicCode::CredentialEnvironmentName,
                RecoveryAction::EditProfile(
                    profile_id.clone(),
                    ProfileFieldId::CredentialEnvironmentName,
                ),
            ),
            (
                PublicCode::Username,
                RecoveryAction::EditProfile(profile_id.clone(), ProfileFieldId::Username),
            ),
        ] {
            assert_exact(
                operation,
                PublicSummary::AuthenticationFailed,
                code,
                &profile,
                &[expected],
            );
        }
    }
    assert_exact(
        OperationKind::ConnectProfile,
        PublicSummary::AuthenticationFailed,
        PublicCode::SessionCredential,
        &SafeContext::profile_with_recipe(profile_id.clone(), operation_id, OperationRecipeId(113)),
        &[
            RecoveryAction::OpenCredentialPrompt(profile_id.clone()),
            RecoveryAction::Retry(OperationRecipeId(113)),
        ],
    );
    assert_exact(
        OperationKind::ExecuteMutation,
        PublicSummary::AuthenticationFailed,
        PublicCode::SessionCredential,
        &SafeContext::profile_with_recipe(profile_id.clone(), operation_id, OperationRecipeId(114)),
        &[RecoveryAction::OpenCredentialPrompt(profile_id.clone())],
    );
    assert_exact(
        OperationKind::ExecuteRead,
        PublicSummary::AuthenticationFailed,
        PublicCode::SessionCredential,
        &SafeContext::profile_with_recipe(profile_id.clone(), operation_id, OperationRecipeId(115)),
        &[RecoveryAction::OpenCredentialPrompt(profile_id.clone())],
    );

    for (code, field) in [
        (PublicCode::Username, ProfileFieldId::Username),
        (PublicCode::Database, ProfileFieldId::Database),
    ] {
        assert_exact(
            OperationKind::TestDraftConnection,
            PublicSummary::PermissionDenied,
            code,
            &draft,
            &[
                RecoveryAction::EditDraft(draft_id, field),
                RecoveryAction::DismissError(operation_id),
            ],
        );
        for operation in [
            OperationKind::ConnectProfile,
            OperationKind::ReconnectProfile,
            OperationKind::BrowseMySql,
            OperationKind::BrowseRedis,
            OperationKind::InspectRedis,
        ] {
            assert_exact(
                operation,
                PublicSummary::PermissionDenied,
                code,
                &profile,
                &[
                    RecoveryAction::EditProfile(profile_id.clone(), field),
                    RecoveryAction::DismissError(operation_id),
                ],
            );
        }
    }
    for operation in [OperationKind::ExecuteRead, OperationKind::ExecuteMutation] {
        assert_exact(
            operation,
            PublicSummary::PermissionDenied,
            PublicCode::Username,
            &profile,
            &[
                RecoveryAction::FocusEditor(profile_id.clone()),
                RecoveryAction::DismissError(operation_id),
            ],
        );
    }
}

#[test]
fn canonical_transport_timeout_and_server_rejection_rows_have_exact_actions() {
    let draft_id = DraftId(121);
    let operation_id = OperationId(122);
    let profile_id = ProfileId("transport".to_owned());
    let draft = SafeContext::draft(draft_id, operation_id);
    let profile = SafeContext::profile(profile_id.clone(), operation_id);

    assert_exact(
        OperationKind::TestDraftConnection,
        PublicSummary::NetworkUnavailable,
        PublicCode::None,
        &draft,
        &[
            RecoveryAction::EditDraft(draft_id, ProfileFieldId::Host),
            RecoveryAction::DismissError(operation_id),
        ],
    );
    for operation in [
        OperationKind::ConnectProfile,
        OperationKind::ReconnectProfile,
        OperationKind::ExecuteRead,
        OperationKind::BrowseMySql,
        OperationKind::BrowseRedis,
        OperationKind::InspectRedis,
    ] {
        assert_exact(
            operation,
            PublicSummary::NetworkUnavailable,
            PublicCode::None,
            &profile,
            &[
                RecoveryAction::EditProfile(profile_id.clone(), ProfileFieldId::Host),
                RecoveryAction::Reconnect(profile_id.clone()),
            ],
        );
    }
    assert_exact(
        OperationKind::ConnectProfile,
        PublicSummary::NetworkUnavailable,
        PublicCode::None,
        &SafeContext::profile_with_recipe(profile_id.clone(), operation_id, OperationRecipeId(123)),
        &[
            RecoveryAction::EditProfile(profile_id.clone(), ProfileFieldId::Host),
            RecoveryAction::Reconnect(profile_id.clone()),
            RecoveryAction::Retry(OperationRecipeId(123)),
        ],
    );
    assert_exact(
        OperationKind::ExecuteMutation,
        PublicSummary::NetworkUnavailable,
        PublicCode::None,
        &SafeContext::profile_with_recipe(profile_id.clone(), operation_id, OperationRecipeId(124)),
        &[
            RecoveryAction::EditProfile(profile_id.clone(), ProfileFieldId::Host),
            RecoveryAction::Reconnect(profile_id.clone()),
        ],
    );
    assert_exact(
        OperationKind::ExecuteRead,
        PublicSummary::NetworkUnavailable,
        PublicCode::None,
        &SafeContext::profile_with_recipe(profile_id.clone(), operation_id, OperationRecipeId(126)),
        &[
            RecoveryAction::EditProfile(profile_id.clone(), ProfileFieldId::Host),
            RecoveryAction::Reconnect(profile_id.clone()),
        ],
    );

    for (code, field) in [
        (
            PublicCode::RedisTlsCaInvalidPem,
            ProfileFieldId::RedisCaFile,
        ),
        (
            PublicCode::RedisTlsCaUntrustedIssuer,
            ProfileFieldId::RedisCaFile,
        ),
        (PublicCode::TlsHostnameMismatch, ProfileFieldId::Host),
    ] {
        assert_exact(
            OperationKind::TestDraftConnection,
            PublicSummary::TlsVerificationFailed,
            code,
            &draft,
            &[RecoveryAction::EditDraft(draft_id, field)],
        );
        for operation in [
            OperationKind::ConnectProfile,
            OperationKind::ReconnectProfile,
            OperationKind::ExecuteRead,
            OperationKind::ExecuteMutation,
            OperationKind::BrowseMySql,
            OperationKind::BrowseRedis,
            OperationKind::InspectRedis,
        ] {
            assert_exact(
                operation,
                PublicSummary::TlsVerificationFailed,
                code,
                &profile,
                &[RecoveryAction::EditProfile(profile_id.clone(), field)],
            );
        }
    }

    assert_exact(
        OperationKind::TestDraftConnection,
        PublicSummary::OperationTimedOut,
        PublicCode::None,
        &draft,
        &[
            RecoveryAction::EditDraft(draft_id, ProfileFieldId::Host),
            RecoveryAction::DismissError(operation_id),
        ],
    );
    for operation in [
        OperationKind::ConnectProfile,
        OperationKind::ReconnectProfile,
    ] {
        assert_exact(
            operation,
            PublicSummary::OperationTimedOut,
            PublicCode::None,
            &profile,
            &[
                RecoveryAction::EditProfile(profile_id.clone(), ProfileFieldId::Host),
                RecoveryAction::Reconnect(profile_id.clone()),
                RecoveryAction::DismissError(operation_id),
            ],
        );
    }
    for operation in [OperationKind::ExecuteRead, OperationKind::ExecuteMutation] {
        assert_exact(
            operation,
            PublicSummary::OperationTimedOut,
            PublicCode::None,
            &profile,
            &[
                RecoveryAction::FocusExecuteLimits(profile_id.clone()),
                RecoveryAction::Reconnect(profile_id.clone()),
            ],
        );
    }
    for operation in [
        OperationKind::BrowseMySql,
        OperationKind::BrowseRedis,
        OperationKind::InspectRedis,
    ] {
        assert_exact(
            operation,
            PublicSummary::OperationTimedOut,
            PublicCode::None,
            &profile,
            &[
                RecoveryAction::Reconnect(profile_id.clone()),
                RecoveryAction::DismissError(operation_id),
            ],
        );
        assert_exact(
            operation,
            PublicSummary::OperationTimedOut,
            PublicCode::None,
            &SafeContext::profile_with_recipe(
                profile_id.clone(),
                operation_id,
                OperationRecipeId(125),
            ),
            &[
                RecoveryAction::Retry(OperationRecipeId(125)),
                RecoveryAction::DismissError(operation_id),
            ],
        );
    }

    for operation in [OperationKind::ExecuteRead, OperationKind::ExecuteMutation] {
        assert_exact(
            operation,
            PublicSummary::SyntaxRejected,
            PublicCode::None,
            &profile,
            &[RecoveryAction::FocusEditor(profile_id.clone())],
        );
        assert_exact(
            operation,
            PublicSummary::ConstraintRejected,
            PublicCode::None,
            &profile,
            &[
                RecoveryAction::FocusEditor(profile_id.clone()),
                RecoveryAction::DismissError(operation_id),
            ],
        );
    }
}

#[test]
fn canonical_lifecycle_config_export_and_internal_rows_have_exact_actions() {
    let draft_id = DraftId(131);
    let operation_id = OperationId(132);
    let active_id = OperationId(133);
    let profile_id = ProfileId("lifecycle".to_owned());
    let result_id = ResultId(134);
    let draft = SafeContext::draft(draft_id, operation_id);
    let profile = SafeContext::profile(profile_id.clone(), operation_id);

    assert_exact(
        OperationKind::TestDraftConnection,
        PublicSummary::UnsupportedFeature,
        PublicCode::RedisTlsPreferredLegacy,
        &draft,
        &[RecoveryAction::EditDraft(
            draft_id,
            ProfileFieldId::RedisTlsMode,
        )],
    );
    assert_exact(
        OperationKind::ConnectProfile,
        PublicSummary::UnsupportedFeature,
        PublicCode::RedisTlsPreferredLegacy,
        &profile,
        &[RecoveryAction::EditProfile(
            profile_id.clone(),
            ProfileFieldId::RedisTlsMode,
        )],
    );
    for operation in [OperationKind::ExecuteRead, OperationKind::ExecuteMutation] {
        assert_exact(
            operation,
            PublicSummary::UnsupportedFeature,
            PublicCode::PreparedStatementUnsupported,
            &profile,
            &[
                RecoveryAction::FocusEditor(profile_id.clone()),
                RecoveryAction::DismissError(operation_id),
            ],
        );
    }
    for operation in [
        OperationKind::ExecuteRead,
        OperationKind::ExecuteMutation,
        OperationKind::BrowseMySql,
        OperationKind::BrowseRedis,
        OperationKind::InspectRedis,
    ] {
        assert_exact(
            operation,
            PublicSummary::UnsupportedFeature,
            PublicCode::None,
            &profile,
            &[RecoveryAction::DismissError(operation_id)],
        );
    }

    assert_exact(
        OperationKind::TestDraftConnection,
        PublicSummary::OperationCancelled,
        PublicCode::None,
        &draft,
        &[RecoveryAction::DismissError(operation_id)],
    );
    assert!(matches!(
        recovery_for(
            OperationKind::LoadConfiguration,
            PublicSummary::ResourceBusy,
            PublicCode::None,
            &SafeContext::global(operation_id),
        ),
        Err(UnreachableRecovery {
            operation: OperationKind::LoadConfiguration,
            summary: PublicSummary::ResourceBusy,
            ..
        })
    ));
    for operation in [
        OperationKind::ConnectProfile,
        OperationKind::ReconnectProfile,
        OperationKind::ExecuteRead,
        OperationKind::ExecuteMutation,
        OperationKind::BrowseMySql,
        OperationKind::BrowseRedis,
        OperationKind::InspectRedis,
    ] {
        assert_exact(
            operation,
            PublicSummary::OperationCancelled,
            PublicCode::None,
            &profile,
            &[
                RecoveryAction::Reconnect(profile_id.clone()),
                RecoveryAction::DismissError(operation_id),
            ],
        );
    }
    assert!(matches!(
        recovery_for(
            OperationKind::DisconnectProfile,
            PublicSummary::OperationCancelled,
            PublicCode::None,
            &profile,
        ),
        Err(UnreachableRecovery {
            operation: OperationKind::DisconnectProfile,
            summary: PublicSummary::OperationCancelled,
            ..
        })
    ));
    for (committed, first) in [
        (false, RecoveryAction::ChooseExportDestination(result_id)),
        (true, RecoveryAction::RevealExportDestination(result_id)),
    ] {
        assert_exact(
            OperationKind::ExportResult,
            PublicSummary::OperationCancelled,
            PublicCode::None,
            &SafeContext::export(result_id, operation_id, committed),
            &[first, RecoveryAction::DismissError(operation_id)],
        );
    }

    assert_exact(
        OperationKind::TestDraftConnection,
        PublicSummary::ResourceBusy,
        PublicCode::None,
        &draft,
        &[RecoveryAction::DismissError(operation_id)],
    );
    assert_eq!(
        recovery_for_with_fact(
            OperationKind::TestDraftConnection,
            PublicSummary::ResourceBusy,
            PublicCode::None,
            &draft,
            RecoveryFact::Busy(BusyFact::KnownActive(active_id)),
        )
        .expect("known active draft busy row")
        .as_slice(),
        &[
            RecoveryAction::CancelOperation(active_id),
            RecoveryAction::DismissError(operation_id),
        ]
    );
    assert_eq!(
        recovery_for_with_fact(
            OperationKind::TestDraftConnection,
            PublicSummary::ResourceBusy,
            PublicCode::None,
            &draft,
            RecoveryFact::Busy(BusyFact::UnknownActive),
        )
        .expect("unknown active draft busy row")
        .as_slice(),
        &[RecoveryAction::DismissError(operation_id)]
    );
    assert_exact(
        OperationKind::ExecuteRead,
        PublicSummary::ResourceBusy,
        PublicCode::None,
        &SafeContext::profile_with_active(profile_id.clone(), operation_id, active_id),
        &[
            RecoveryAction::CancelOperation(active_id),
            RecoveryAction::DismissError(operation_id),
        ],
    );
    assert_exact(
        OperationKind::ExecuteRead,
        PublicSummary::ResourceBusy,
        PublicCode::None,
        &profile,
        &[RecoveryAction::DismissError(operation_id)],
    );

    for (operation, context) in [
        (
            OperationKind::ReloadConfiguration,
            SafeContext::global(operation_id),
        ),
        (OperationKind::CreateProfile, draft.clone()),
        (OperationKind::UpdateProfile, profile.clone()),
        (OperationKind::DeleteProfile, profile.clone()),
    ] {
        assert_exact(
            operation,
            PublicSummary::ResourceStale,
            PublicCode::None,
            &context,
            &[RecoveryAction::ReloadConfiguration],
        );
    }
    for operation in [
        OperationKind::ConnectProfile,
        OperationKind::ReconnectProfile,
        OperationKind::BrowseMySql,
        OperationKind::BrowseRedis,
        OperationKind::InspectRedis,
    ] {
        assert_exact(
            operation,
            PublicSummary::ResourceStale,
            PublicCode::None,
            &SafeContext::profile_with_recipe(
                profile_id.clone(),
                operation_id,
                OperationRecipeId(135),
            ),
            &[RecoveryAction::Retry(OperationRecipeId(135))],
        );
    }
    for operation in [OperationKind::ExecuteRead, OperationKind::ExecuteMutation] {
        assert_exact(
            operation,
            PublicSummary::ResourceStale,
            PublicCode::None,
            &profile,
            &[
                RecoveryAction::ReloadConfiguration,
                RecoveryAction::FocusEditor(profile_id.clone()),
            ],
        );
    }

    for summary in [
        PublicSummary::ConfigWriteNotCommitted,
        PublicSummary::CommittedDurabilityUnknown,
    ] {
        for operation in [
            OperationKind::CreateProfile,
            OperationKind::UpdateProfile,
            OperationKind::DeleteProfile,
        ] {
            let context = if operation == OperationKind::CreateProfile {
                &draft
            } else {
                &profile
            };
            assert_exact(
                operation,
                summary,
                PublicCode::MigrationBackupAvailable,
                context,
                &[RecoveryAction::ReloadConfiguration],
            );
        }
        assert_exact(
            OperationKind::MigrateConfiguration,
            summary,
            PublicCode::None,
            &SafeContext::global(operation_id),
            &[RecoveryAction::ReloadConfiguration],
        );
        assert_exact(
            OperationKind::MigrateConfiguration,
            summary,
            PublicCode::MigrationBackupAvailable,
            &SafeContext::global(operation_id),
            &[
                RecoveryAction::ReloadConfiguration,
                RecoveryAction::RevealMigrationBackup,
            ],
        );
    }
    assert_exact(
        OperationKind::ExportResult,
        PublicSummary::CommittedDurabilityUnknown,
        PublicCode::ExportDestinationCommitted,
        &SafeContext::export(result_id, operation_id, true),
        &[RecoveryAction::RevealExportDestination(result_id)],
    );
    for (committed, expected) in [
        (
            false,
            vec![RecoveryAction::ChooseExportDestination(result_id)],
        ),
        (
            true,
            vec![
                RecoveryAction::ChooseExportDestination(result_id),
                RecoveryAction::RevealExportDestination(result_id),
            ],
        ),
    ] {
        assert_exact(
            OperationKind::ExportResult,
            PublicSummary::ExportFailed,
            PublicCode::None,
            &SafeContext::export(result_id, operation_id, committed),
            &expected,
        );
    }

    for operation in OperationKind::ALL {
        let context = context_for(*operation);
        let mut expected = vec![
            RecoveryAction::RestartApplication,
            RecoveryAction::DismissError(OperationId(2)),
        ];
        if matches!(
            operation,
            OperationKind::LoadConfiguration
                | OperationKind::ReloadConfiguration
                | OperationKind::MigrateConfiguration
                | OperationKind::CreateProfile
                | OperationKind::UpdateProfile
                | OperationKind::DeleteProfile
        ) {
            expected.push(RecoveryAction::ReloadConfiguration);
        }
        assert_exact(
            *operation,
            PublicSummary::InternalFailure,
            PublicCode::None,
            &context,
            &expected,
        );
    }
}

#[test]
fn create_collision_and_draft_tls_recovery_never_cross_identity_domains() {
    let draft_id = DraftId(31);
    let operation_id = OperationId(41);
    let create = SafeContext::draft(draft_id, operation_id);
    let collision = recovery_for(
        OperationKind::CreateProfile,
        PublicSummary::InvalidInput,
        PublicCode::ProfileIdConflict,
        &create,
    )
    .expect("collision is reachable");
    assert_eq!(
        collision.as_slice(),
        &[RecoveryAction::EditDraft(
            draft_id,
            ProfileFieldId::ConnectionId
        )]
    );

    for (code, field) in [
        (
            PublicCode::RedisTlsCaInvalidPem,
            ProfileFieldId::RedisCaFile,
        ),
        (
            PublicCode::RedisTlsCaUntrustedIssuer,
            ProfileFieldId::RedisCaFile,
        ),
        (PublicCode::TlsHostnameMismatch, ProfileFieldId::Host),
    ] {
        let actions = recovery_for(
            OperationKind::TestDraftConnection,
            PublicSummary::TlsVerificationFailed,
            code,
            &create,
        )
        .expect("draft TLS recovery");
        assert_eq!(
            actions.as_slice(),
            &[RecoveryAction::EditDraft(draft_id, field)]
        );
        assert!(actions.as_slice().iter().all(|action| !matches!(
            action,
            RecoveryAction::EditProfile(_, _)
                | RecoveryAction::OpenCredentialPrompt(_)
                | RecoveryAction::Retry(_)
        )));
    }
}

#[test]
fn saved_tls_recovery_uses_profile_identity_and_no_fallback() {
    let profile = ProfileId("saved".to_owned());
    let context = SafeContext::profile(profile.clone(), OperationId(52));
    let ca = recovery_for(
        OperationKind::ConnectProfile,
        PublicSummary::TlsVerificationFailed,
        PublicCode::RedisTlsCaInvalidPem,
        &context,
    )
    .expect("saved CA recovery");
    assert_eq!(
        ca.as_slice(),
        &[RecoveryAction::EditProfile(
            profile.clone(),
            ProfileFieldId::RedisCaFile
        )]
    );
    let host = recovery_for(
        OperationKind::ConnectProfile,
        PublicSummary::TlsVerificationFailed,
        PublicCode::TlsHostnameMismatch,
        &context,
    )
    .expect("saved hostname recovery");
    assert_eq!(
        host.as_slice(),
        &[RecoveryAction::EditProfile(profile, ProfileFieldId::Host)]
    );
}

#[test]
fn every_draft_and_create_recovery_action_stays_in_its_originating_identity_domain() {
    let draft_id = DraftId(71);
    let operation_id = OperationId(72);
    for operation in [
        OperationKind::TestDraftConnection,
        OperationKind::CreateProfile,
    ] {
        for summary in PublicSummary::ALL {
            let code = canonical_code(operation, *summary);
            let context = SafeContext::draft(draft_id, operation_id);
            let Ok(actions) = recovery_for(operation, *summary, code, &context) else {
                continue;
            };
            for action in actions.as_slice() {
                match action {
                    RecoveryAction::EditDraft(actual, _) => assert_eq!(*actual, draft_id),
                    RecoveryAction::CancelOperation(_)
                    | RecoveryAction::ReloadConfiguration
                    | RecoveryAction::RevealMigrationBackup
                    | RecoveryAction::RestartApplication => {}
                    RecoveryAction::DismissError(actual) => assert_eq!(*actual, operation_id),
                    RecoveryAction::OpenCredentialPrompt(_)
                    | RecoveryAction::EditProfile(_, _)
                    | RecoveryAction::Retry(_)
                    | RecoveryAction::FocusEditor(_)
                    | RecoveryAction::FocusExecuteLimits(_)
                    | RecoveryAction::Reconnect(_)
                    | RecoveryAction::ClearCatalog(_)
                    | RecoveryAction::RestartRedisScan(_)
                    | RecoveryAction::ChooseExportDestination(_)
                    | RecoveryAction::RevealExportDestination(_) => {
                        panic!("cross-domain action: {operation:?}/{summary:?}/{action:?}")
                    }
                }
            }
        }
    }
}

#[test]
fn public_error_wire_shape_has_only_the_frozen_five_fields() {
    let error = PublicOperationError::new(
        OperationKind::CreateProfile,
        PublicSummary::InvalidInput,
        PublicCode::ProfileIdConflict,
        &SafeContext::draft(DraftId(80), OperationId(81)),
    )
    .expect("reachable public error");
    let value = serde_json::to_value(error).expect("public error serializes");
    let object = value.as_object().expect("public error object");
    assert_eq!(object.len(), 5);
    assert!(object.contains_key("operation"));
    assert!(object.contains_key("category"));
    assert!(object.contains_key("code"));
    assert!(object.contains_key("summary"));
    assert!(object.contains_key("recovery"));
    assert!(!object.contains_key("operation_id"));
}

#[test]
fn migration_backup_reveal_is_migrate_only_for_both_config_outcomes() {
    for summary in [
        PublicSummary::ConfigWriteNotCommitted,
        PublicSummary::CommittedDurabilityUnknown,
    ] {
        let migrate = recovery_for(
            OperationKind::MigrateConfiguration,
            summary,
            PublicCode::MigrationBackupAvailable,
            &SafeContext::global(OperationId(90)),
        )
        .expect("migrate recovery");
        assert_eq!(
            migrate.as_slice(),
            &[
                RecoveryAction::ReloadConfiguration,
                RecoveryAction::RevealMigrationBackup,
            ]
        );

        for operation in [
            OperationKind::CreateProfile,
            OperationKind::UpdateProfile,
            OperationKind::DeleteProfile,
        ] {
            let context = match operation {
                OperationKind::CreateProfile => SafeContext::draft(DraftId(91), OperationId(92)),
                _ => SafeContext::profile(ProfileId("saved".to_owned()), OperationId(92)),
            };
            let actions = recovery_for(
                operation,
                summary,
                PublicCode::MigrationBackupAvailable,
                &context,
            )
            .expect("config mutation recovery");
            assert_eq!(
                actions.as_slice(),
                &[RecoveryAction::ReloadConfiguration],
                "operation={operation:?} summary={summary:?}"
            );
        }
    }
}

#[test]
fn scanner_and_typed_backend_codes_are_closed_and_validated() {
    let mysql = MySqlPublicErrorCode::new(1064, "42000").expect("valid MySQL code");
    assert_eq!(mysql.errno(), 1064);
    assert_eq!(mysql.sql_state(), "42000");
    assert!(MySqlPublicErrorCode::new(0, "42000").is_err());
    assert!(MySqlPublicErrorCode::new(1064, "42-00").is_err());
    assert!(MySqlPublicErrorCode::new(1064, "TOO-LONG").is_err());

    assert_eq!(
        RedisPublicErrorKind::try_from(redis::ErrorKind::AuthenticationFailed),
        Ok(RedisPublicErrorKind::AuthenticationFailed)
    );
    assert_eq!(
        RedisPublicErrorKind::try_from(redis::ErrorKind::Server(redis::ServerErrorKind::NoPerm,)),
        Ok(RedisPublicErrorKind::NoPerm)
    );

    for code in [
        PublicCode::AmbiguousSqlMode,
        PublicCode::UnterminatedSqlToken,
        PublicCode::MySql(mysql),
        PublicCode::Redis(RedisPublicErrorKind::AuthenticationFailed),
    ] {
        let encoded = serde_json::to_string(&code).expect("typed code serializes");
        assert!(!encoded.contains("backend message sentinel"));
    }

    let context = SafeContext::profile(ProfileId("mysql".to_owned()), OperationId(95));
    for code in [
        PublicCode::AmbiguousSqlMode,
        PublicCode::UnterminatedSqlToken,
    ] {
        assert_eq!(
            recovery_for(
                OperationKind::ExecuteRead,
                PublicSummary::InvalidInput,
                code,
                &context,
            )
            .expect("scanner recovery")
            .as_slice(),
            &[RecoveryAction::FocusEditor(ProfileId("mysql".to_owned()))]
        );
    }
}

fn assert_exact(
    operation: OperationKind,
    summary: PublicSummary,
    code: PublicCode,
    context: &SafeContext,
    expected: &[RecoveryAction],
) {
    let actual = recovery_for(operation, summary, code, context)
        .unwrap_or_else(|error| panic!("canonical row unexpectedly unreachable: {error:?}"));
    assert_eq!(
        actual.as_slice(),
        expected,
        "operation={operation:?} summary={summary:?} code={code:?}"
    );
}

fn context_for(operation: OperationKind) -> SafeContext {
    match operation {
        OperationKind::CreateProfile | OperationKind::TestDraftConnection => {
            SafeContext::draft(DraftId(1), OperationId(2))
        }
        OperationKind::ExportResult => {
            SafeContext::export(dbotter::model::ResultId(3), OperationId(2), false)
        }
        OperationKind::LoadConfiguration
        | OperationKind::ReloadConfiguration
        | OperationKind::MigrateConfiguration
        | OperationKind::ShutdownRuntime => SafeContext::global(OperationId(2)),
        _ => SafeContext::profile(ProfileId("profile".to_owned()), OperationId(2)),
    }
}

fn matrix_context(operation: OperationKind, summary: PublicSummary) -> SafeContext {
    if summary == PublicSummary::ResourceStale
        && matches!(
            operation,
            OperationKind::ConnectProfile
                | OperationKind::ReconnectProfile
                | OperationKind::BrowseMySql
                | OperationKind::BrowseRedis
                | OperationKind::InspectRedis
        )
    {
        return SafeContext::profile_with_recipe(
            ProfileId("profile".to_owned()),
            OperationId(2),
            dbotter::model::OperationRecipeId(3),
        );
    }
    context_for(operation)
}

fn canonical_code(operation: OperationKind, summary: PublicSummary) -> PublicCode {
    match summary {
        PublicSummary::InvalidInput => match operation {
            OperationKind::CreateProfile => PublicCode::ProfileIdConflict,
            OperationKind::TestDraftConnection
            | OperationKind::UpdateProfile
            | OperationKind::ConnectProfile
            | OperationKind::ReconnectProfile => PublicCode::Field(ProfileFieldId::Host),
            OperationKind::ExecuteRead | OperationKind::ExecuteMutation => {
                PublicCode::StatementTarget
            }
            OperationKind::BrowseMySql => PublicCode::Catalog,
            OperationKind::BrowseRedis => PublicCode::RedisScan,
            OperationKind::ExportResult => PublicCode::ExportDestination,
            _ => PublicCode::None,
        },
        PublicSummary::CredentialRequired | PublicSummary::AuthenticationFailed => {
            PublicCode::SessionCredential
        }
        PublicSummary::PermissionDenied => PublicCode::Username,
        PublicSummary::TlsVerificationFailed => PublicCode::RedisTlsCaInvalidPem,
        PublicSummary::UnsupportedFeature => match operation {
            OperationKind::TestDraftConnection | OperationKind::ConnectProfile => {
                PublicCode::RedisTlsPreferredLegacy
            }
            OperationKind::ExecuteRead | OperationKind::ExecuteMutation => {
                PublicCode::PreparedStatementUnsupported
            }
            _ => PublicCode::None,
        },
        _ => PublicCode::None,
    }
}

fn expected_reachable_pairs() -> HashSet<(OperationKind, PublicSummary)> {
    let mut expected = HashSet::new();
    let mut add = |summary, operations: &[OperationKind]| {
        expected.extend(
            operations
                .iter()
                .copied()
                .map(|operation| (operation, summary)),
        );
    };
    add(
        PublicSummary::InvalidInput,
        &[
            OperationKind::LoadConfiguration,
            OperationKind::ReloadConfiguration,
            OperationKind::MigrateConfiguration,
            OperationKind::CreateProfile,
            OperationKind::UpdateProfile,
            OperationKind::DeleteProfile,
            OperationKind::TestDraftConnection,
            OperationKind::ConnectProfile,
            OperationKind::ReconnectProfile,
            OperationKind::ExecuteRead,
            OperationKind::ExecuteMutation,
            OperationKind::BrowseMySql,
            OperationKind::BrowseRedis,
            OperationKind::InspectRedis,
            OperationKind::ExportResult,
        ],
    );
    let credential_operations = [
        OperationKind::TestDraftConnection,
        OperationKind::ConnectProfile,
        OperationKind::ReconnectProfile,
        OperationKind::ExecuteRead,
        OperationKind::ExecuteMutation,
        OperationKind::BrowseMySql,
        OperationKind::BrowseRedis,
        OperationKind::InspectRedis,
    ];
    add(PublicSummary::CredentialRequired, &credential_operations);
    add(PublicSummary::AuthenticationFailed, &credential_operations);
    add(PublicSummary::PermissionDenied, &credential_operations);
    add(PublicSummary::NetworkUnavailable, &credential_operations);
    add(PublicSummary::TlsVerificationFailed, &credential_operations);
    add(PublicSummary::OperationTimedOut, &credential_operations);
    add(
        PublicSummary::SyntaxRejected,
        &[OperationKind::ExecuteRead, OperationKind::ExecuteMutation],
    );
    add(
        PublicSummary::ConstraintRejected,
        &[OperationKind::ExecuteRead, OperationKind::ExecuteMutation],
    );
    add(
        PublicSummary::UnsupportedFeature,
        &[
            OperationKind::TestDraftConnection,
            OperationKind::ConnectProfile,
            OperationKind::ExecuteRead,
            OperationKind::ExecuteMutation,
            OperationKind::BrowseMySql,
            OperationKind::BrowseRedis,
            OperationKind::InspectRedis,
        ],
    );
    add(
        PublicSummary::OperationCancelled,
        &[
            OperationKind::TestDraftConnection,
            OperationKind::ConnectProfile,
            OperationKind::ReconnectProfile,
            OperationKind::ExecuteRead,
            OperationKind::ExecuteMutation,
            OperationKind::BrowseMySql,
            OperationKind::BrowseRedis,
            OperationKind::InspectRedis,
            OperationKind::ExportResult,
        ],
    );
    add(
        PublicSummary::ResourceBusy,
        &OperationKind::ALL[1..OperationKind::ALL.len() - 1],
    );
    add(
        PublicSummary::ResourceStale,
        &[
            OperationKind::ReloadConfiguration,
            OperationKind::CreateProfile,
            OperationKind::UpdateProfile,
            OperationKind::DeleteProfile,
            OperationKind::ConnectProfile,
            OperationKind::ReconnectProfile,
            OperationKind::ExecuteRead,
            OperationKind::ExecuteMutation,
            OperationKind::BrowseMySql,
            OperationKind::BrowseRedis,
            OperationKind::InspectRedis,
        ],
    );
    let config_mutations = [
        OperationKind::MigrateConfiguration,
        OperationKind::CreateProfile,
        OperationKind::UpdateProfile,
        OperationKind::DeleteProfile,
    ];
    add(PublicSummary::ConfigWriteNotCommitted, &config_mutations);
    add(
        PublicSummary::CommittedDurabilityUnknown,
        &[
            OperationKind::MigrateConfiguration,
            OperationKind::CreateProfile,
            OperationKind::UpdateProfile,
            OperationKind::DeleteProfile,
            OperationKind::ExportResult,
        ],
    );
    add(PublicSummary::ExportFailed, &[OperationKind::ExportResult]);
    add(PublicSummary::InternalFailure, OperationKind::ALL);
    expected
}
