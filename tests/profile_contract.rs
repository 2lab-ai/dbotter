use std::path::PathBuf;
use std::sync::Arc;

use dbotter::model::{
    ConnectionDraft, CredentialMode, DriverKind, ExecuteBatchRequest, ExecuteRequest, OperationId,
    ProfileFieldId, ProfileId, QueryLanguage, RedisTlsConfig, SessionCredentialIntent, TlsMode,
};
use dbotter::secrets::{
    CredentialEditContext, ReplacementSecretBuffer, SecretError, SessionIntentPolicy,
    SessionSecret, SessionSecretStore, SessionSecretUpdate, session_intent_policy,
    session_update_for_save,
};
use dbotter::service::{ProfileValidationError, validate_connection_draft};

#[test]
fn redis_tls_is_fail_closed_and_driver_defaults_are_exact() {
    assert_eq!(
        ConnectionDraft::for_driver(DriverKind::Redis).tls,
        TlsMode::Disabled
    );

    let mut draft = ConnectionDraft::for_driver(DriverKind::Redis);
    draft.name = "Redis".to_owned();
    draft.tls = TlsMode::Preferred;
    assert!(matches!(
        validate_connection_draft(&draft),
        Err(ProfileValidationError::Field {
            field: ProfileFieldId::RedisTlsMode,
            ..
        })
    ));

    draft.tls = TlsMode::Disabled;
    draft.redis_tls.ca_file = Some(PathBuf::from("unused.pem"));
    assert!(matches!(
        validate_connection_draft(&draft),
        Err(ProfileValidationError::Field {
            field: ProfileFieldId::RedisCaFile,
            ..
        })
    ));

    draft.tls = TlsMode::Required;
    draft.redis_tls = RedisTlsConfig::default();
    assert!(validate_connection_draft(&draft).is_ok());
}

#[test]
fn redis_required_ca_must_be_a_readable_regular_pem_certificate() {
    let directory = tempfile::tempdir().expect("tempdir");
    let mut draft = ConnectionDraft::for_driver(DriverKind::Redis);
    draft.name = "Redis TLS".to_owned();
    draft.tls = TlsMode::Required;

    draft.redis_tls.ca_file = Some(directory.path().to_owned());
    assert!(matches!(
        validate_connection_draft(&draft),
        Err(ProfileValidationError::Field {
            field: ProfileFieldId::RedisCaFile,
            ..
        })
    ));

    let invalid = directory.path().join("invalid.pem");
    std::fs::write(&invalid, "-----BEGIN CERTIFICATE-----\nnot-base64\n").expect("invalid fixture");
    draft.redis_tls.ca_file = Some(invalid);
    assert!(validate_connection_draft(&draft).is_err());

    let valid = directory.path().join("valid.pem");
    std::fs::write(
        &valid,
        "-----BEGIN CERTIFICATE-----\n\
MIIB0zCCAYWgAwIBAgIUEFN5G3AUb5d/ZC+q+YtFuMeoWvowBQYDK2VwMF8xCzAJ\n\
BgNVBAYTAnVzMRMwEQYDVQQIDApjYWxpZm9ybmlhMSEwHwYDVQQKDBhJbnRlcm5l\n\
dCBXaWRnaXRzIFB0eSBMdGQxGDAWBgNVBAMMD0F1c3RpbiBCb25hbmRlcjAeFw0y\n\
NTA3MDEwMzA4MTVaFw0zNTA2MjkwMzA4MTVaMF8xCzAJBgNVBAYTAnVzMRMwEQYD\n\
VQQIDApjYWxpZm9ybmlhMSEwHwYDVQQKDBhJbnRlcm5ldCBXaWRnaXRzIFB0eSBM\n\
dGQxGDAWBgNVBAMMD0F1c3RpbiBCb25hbmRlcjAqMAUGAytlcAMhAHfjdF5QJ4OW\n\
k/3XLlsxDcP8cwBVmB+ySWKq2JanRS8uo1MwUTAdBgNVHQ4EFgQUCw2pVpGKz2xk\n\
IjbVHYh0LnzdkW4wHwYDVR0jBBgwFoAUCw2pVpGKz2xkIjbVHYh0LnzdkW4wDwYD\n\
VR0TAQH/BAUwAwEB/zAFBgMrZXADQQBA6VMDBPz9x0b5Wvw4D+2UrLdyhzzjqtrX\n\
UQOjCTqcKdEwWvgS6ftiQlQJPDfkVDEMOAJgqRmEGvsKjvwMCPIC\n\
-----END CERTIFICATE-----\n",
    )
    .expect("valid fixture");
    draft.redis_tls.ca_file = Some(valid);
    assert!(validate_connection_draft(&draft).is_ok());
}

#[test]
fn session_intent_policy_and_save_mapping_are_total() {
    assert_eq!(
        session_intent_policy(
            CredentialMode::Session,
            CredentialEditContext::Edit { has_current: true }
        ),
        Some(SessionIntentPolicy {
            allowed: vec![
                SessionCredentialIntent::KeepCurrent,
                SessionCredentialIntent::Replace,
                SessionCredentialIntent::Forget,
            ],
            default: SessionCredentialIntent::KeepCurrent,
        })
    );
    for context in [
        CredentialEditContext::Create,
        CredentialEditContext::Edit { has_current: false },
    ] {
        let policy =
            session_intent_policy(CredentialMode::Session, context).expect("session policy exists");
        assert_eq!(policy.default, SessionCredentialIntent::Replace);
        assert!(
            !policy
                .allowed
                .contains(&SessionCredentialIntent::KeepCurrent)
        );
        assert!(policy.allowed.contains(&SessionCredentialIntent::Forget));
    }
    assert_eq!(
        session_intent_policy(CredentialMode::None, CredentialEditContext::Create),
        None
    );

    let replacement = Arc::new(SessionSecret::new("replacement".to_owned()));
    assert!(matches!(
        session_update_for_save(
            CredentialMode::Session,
            CredentialEditContext::Edit { has_current: true },
            Some(SessionCredentialIntent::KeepCurrent),
            None,
        ),
        Ok(SessionSecretUpdate::Keep)
    ));
    assert!(matches!(
        session_update_for_save(
            CredentialMode::Session,
            CredentialEditContext::Create,
            Some(SessionCredentialIntent::Replace),
            Some(replacement),
        ),
        Ok(SessionSecretUpdate::Replace(_))
    ));
    assert!(matches!(
        session_update_for_save(
            CredentialMode::Session,
            CredentialEditContext::Create,
            Some(SessionCredentialIntent::Forget),
            None,
        ),
        Ok(SessionSecretUpdate::Clear)
    ));
    assert!(matches!(
        session_update_for_save(
            CredentialMode::Environment,
            CredentialEditContext::Edit { has_current: true },
            None,
            None,
        ),
        Ok(SessionSecretUpdate::Clear)
    ));
    assert!(matches!(
        session_update_for_save(
            CredentialMode::Session,
            CredentialEditContext::Create,
            Some(SessionCredentialIntent::KeepCurrent),
            None,
        ),
        Err(SecretError::InvalidSessionIntent)
    ));
}

#[test]
fn every_mode_context_intent_replacement_combination_has_one_exact_save_result() {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ResultKind {
        Keep,
        Replace,
        Clear,
        Invalid,
    }

    let modes = [
        CredentialMode::None,
        CredentialMode::Environment,
        CredentialMode::Session,
    ];
    let contexts = [
        CredentialEditContext::Create,
        CredentialEditContext::Edit { has_current: false },
        CredentialEditContext::Edit { has_current: true },
    ];
    let intents = [
        None,
        Some(SessionCredentialIntent::KeepCurrent),
        Some(SessionCredentialIntent::Replace),
        Some(SessionCredentialIntent::Forget),
    ];
    for mode in modes {
        for context in contexts {
            for intent in intents {
                for has_replacement in [false, true] {
                    let replacement = has_replacement
                        .then(|| Arc::new(SessionSecret::new("matrix-secret".to_owned())));
                    let actual = match session_update_for_save(mode, context, intent, replacement) {
                        Ok(SessionSecretUpdate::Keep) => ResultKind::Keep,
                        Ok(SessionSecretUpdate::Replace(_)) => ResultKind::Replace,
                        Ok(SessionSecretUpdate::Clear) => ResultKind::Clear,
                        Err(_) => ResultKind::Invalid,
                    };
                    let expected = match (mode, context, intent, has_replacement) {
                        (CredentialMode::None | CredentialMode::Environment, _, None, false) => {
                            ResultKind::Clear
                        }
                        (
                            CredentialMode::Session,
                            CredentialEditContext::Edit { has_current: true },
                            Some(SessionCredentialIntent::KeepCurrent),
                            false,
                        ) => ResultKind::Keep,
                        (
                            CredentialMode::Session,
                            _,
                            Some(SessionCredentialIntent::Replace),
                            true,
                        ) => ResultKind::Replace,
                        (
                            CredentialMode::Session,
                            _,
                            Some(SessionCredentialIntent::Forget),
                            false,
                        ) => ResultKind::Clear,
                        _ => ResultKind::Invalid,
                    };
                    assert_eq!(
                        actual, expected,
                        "mode={mode:?} context={context:?} intent={intent:?} replacement={has_replacement}"
                    );
                }
            }
        }
    }
}

#[test]
fn replacement_buffer_save_moves_and_forget_clears() {
    let sentinel = "replacement-top-secret-sentinel";
    let mut buffer = ReplacementSecretBuffer::new(sentinel.to_owned());
    assert_eq!(buffer.as_str(), sentinel);
    assert!(!format!("{buffer:?}").contains(sentinel));

    let saved = buffer.take_for_save().expect("save move");
    assert!(buffer.is_empty());
    assert_eq!(Arc::strong_count(&saved), 1);
    drop(saved);

    let mut forgotten = ReplacementSecretBuffer::new(sentinel.to_owned());
    forgotten.forget();
    assert!(forgotten.is_empty());
}

#[test]
fn secret_store_is_profile_exact_and_debug_is_redacted() {
    let store = SessionSecretStore::default();
    let profile = ProfileId("profile".to_owned());
    let secret = Arc::new(SessionSecret::new("top-secret-sentinel".to_owned()));
    assert!(!format!("{secret:?}").contains("top-secret-sentinel"));
    assert!(
        !format!("{:?}", SessionSecretUpdate::Replace(secret.clone()))
            .contains("top-secret-sentinel")
    );

    store
        .apply(&profile, SessionSecretUpdate::Replace(secret.clone()))
        .expect("replace");
    assert!(store.has_current(&profile).expect("set"));
    store
        .apply(&profile, SessionSecretUpdate::Keep)
        .expect("keep");
    assert!(store.has_current(&profile).expect("kept"));
    store
        .apply(&profile, SessionSecretUpdate::Clear)
        .expect("clear");
    assert!(!store.has_current(&profile).expect("cleared"));
}

#[test]
fn final_store_arc_drop_reaches_a_zeroize_on_drop_secret_string() {
    fn requires_zeroize_on_drop<T: zeroize::ZeroizeOnDrop>() {}
    requires_zeroize_on_drop::<secrecy::SecretString>();

    let store = SessionSecretStore::default();
    let profile = ProfileId("final-arc".to_owned());
    let secret = Arc::new(SessionSecret::new("dbotter-owned-final-arc".to_owned()));
    let weak = Arc::downgrade(&secret);
    store
        .apply(&profile, SessionSecretUpdate::Replace(secret.clone()))
        .expect("store owns Arc");
    drop(secret);
    assert!(weak.upgrade().is_some());

    store
        .apply(&profile, SessionSecretUpdate::Clear)
        .expect("drop final store Arc");
    assert!(weak.upgrade().is_none());
}

#[test]
fn execute_debug_omits_user_text() {
    let request = ExecuteRequest {
        operation_id: OperationId(7),
        profile_id: ProfileId("profile".to_owned()),
        profile_generation: dbotter::model::ProfileGeneration(1),
        language: QueryLanguage::Sql,
        text: "SELECT 'top-secret-sentinel'".to_owned(),
        row_limit: 10,
        timeout: std::time::Duration::from_secs(1),
    };
    let debug = format!("{request:?}");
    assert!(!debug.contains("top-secret-sentinel"));
    assert!(debug.contains("<redacted>"));

    let batch = ExecuteBatchRequest {
        operation_id: OperationId(8),
        profile_id: ProfileId("batch-profile-sentinel".to_owned()),
        profile_generation: dbotter::model::ProfileGeneration(1),
        language: QueryLanguage::Sql,
        text: "SELECT 'batch-top-secret-sentinel'".to_owned(),
        row_limit: 10,
        timeout: std::time::Duration::from_secs(1),
    };
    let debug = format!("{batch:?}");
    assert!(!debug.contains("batch-top-secret-sentinel"));
    assert!(!debug.contains("batch-profile-sentinel"));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn draft_debug_omits_environment_name_and_ca_path() {
    let mut draft = ConnectionDraft::for_driver(DriverKind::Redis);
    draft.name = "Redis".to_owned();
    draft.credential_mode = CredentialMode::Environment;
    draft.secret_env = Some("SENTINEL_ENVIRONMENT_NAME".to_owned());
    draft.tls = TlsMode::Required;
    draft.redis_tls.ca_file = Some(PathBuf::from("/sentinel/redis-ca.pem"));
    let debug = format!("{draft:?}");
    assert!(!debug.contains("SENTINEL_ENVIRONMENT_NAME"));
    assert!(!debug.contains("/sentinel/redis-ca.pem"));
}
