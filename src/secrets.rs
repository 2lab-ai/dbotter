use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::{Arc, RwLock};

use secrecy::SecretString;
use zeroize::{Zeroize as _, Zeroizing};

use crate::model::{CredentialMode, ProfileId, SessionCredentialIntent};

#[derive(thiserror::Error)]
pub enum SecretError {
    #[error("required secret environment variable is missing")]
    MissingEnv(String),
    #[error("required secret environment variable is empty")]
    EmptyEnv(String),
    #[error("the selected session credential intent is not available")]
    InvalidSessionIntent,
    #[error("a replacement session credential is required")]
    ReplacementRequired,
    #[error("a session credential is required")]
    SessionCredentialRequired,
    #[error("the session credential store is unavailable")]
    StoreUnavailable,
}

impl fmt::Debug for SecretError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEnv(_) => formatter.write_str("MissingEnv(<redacted>)"),
            Self::EmptyEnv(_) => formatter.write_str("EmptyEnv(<redacted>)"),
            Self::InvalidSessionIntent => formatter.write_str("InvalidSessionIntent"),
            Self::ReplacementRequired => formatter.write_str("ReplacementRequired"),
            Self::SessionCredentialRequired => formatter.write_str("SessionCredentialRequired"),
            Self::StoreUnavailable => formatter.write_str("StoreUnavailable"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnvironmentAvailability {
    Available,
    Missing,
    Empty,
}

/// A process-owned secret which zeroizes its allocation when the final Arc drops.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::secrets::SessionSecret>();
/// ```
pub struct SessionSecret(SecretString);

impl SessionSecret {
    pub fn new(value: String) -> Self {
        Self(SecretString::from(value))
    }

    pub(crate) fn inner(&self) -> &SecretString {
        &self.0
    }
}

impl fmt::Debug for SessionSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionSecret(<redacted>)")
    }
}

/// The retained form allocation for a Replace intent.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::secrets::ReplacementSecretBuffer>();
/// ```
///
/// One plaintext form buffer has exactly one owner and cannot be cloned.
///
/// ```compile_fail
/// fn requires_clone<T: Clone>() {}
/// requires_clone::<dbotter::secrets::ReplacementSecretBuffer>();
/// ```
#[derive(Default, PartialEq, Eq)]
pub struct ReplacementSecretBuffer(Zeroizing<String>);

impl ReplacementSecretBuffer {
    pub fn new(value: String) -> Self {
        Self(Zeroizing::new(value))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    #[cfg(feature = "desktop")]
    pub(crate) fn as_mut_string(&mut self) -> &mut String {
        &mut self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn copy_for_test(&self) -> Result<Arc<SessionSecret>, SecretError> {
        if self.is_empty() {
            return Err(SecretError::ReplacementRequired);
        }
        Ok(Arc::new(SessionSecret::new(self.0.to_string())))
    }

    pub fn take_for_save(&mut self) -> Result<Arc<SessionSecret>, SecretError> {
        if self.is_empty() {
            return Err(SecretError::ReplacementRequired);
        }
        let value = std::mem::take(&mut *self.0);
        Ok(Arc::new(SessionSecret::new(value)))
    }

    pub fn forget(&mut self) {
        self.0.zeroize();
        self.0.clear();
    }
}

impl fmt::Debug for ReplacementSecretBuffer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ReplacementSecretBuffer")
            .field("state", &if self.is_empty() { "empty" } else { "set" })
            .finish()
    }
}

/// An in-process mutation command. It deliberately has no Serialize implementation.
///
/// ```compile_fail
/// fn requires_serialize<T: serde::Serialize>() {}
/// requires_serialize::<dbotter::secrets::SessionSecretUpdate>();
/// ```
pub enum SessionSecretUpdate {
    Keep,
    Replace(Arc<SessionSecret>),
    Clear,
}

impl fmt::Debug for SessionSecretUpdate {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Keep => formatter.write_str("SessionSecretUpdate::Keep"),
            Self::Replace(_) => formatter.write_str("SessionSecretUpdate::Replace(<redacted>)"),
            Self::Clear => formatter.write_str("SessionSecretUpdate::Clear"),
        }
    }
}

/// Profile-keyed process-local secret storage.
///
/// Secret Arc lookup is deliberately crate-private so an external caller
/// cannot pair a saved credential with an arbitrary draft endpoint.
///
/// ```compile_fail
/// let store = dbotter::secrets::SessionSecretStore::default();
/// let profile = dbotter::model::ProfileId("saved".to_owned());
/// let _ = store.clone_for_profile(&profile);
/// ```
#[derive(Default)]
pub struct SessionSecretStore {
    values: RwLock<HashMap<ProfileId, Arc<SessionSecret>>>,
}

impl fmt::Debug for SessionSecretStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let count = self.values.read().map_or(0, |values| values.len());
        formatter
            .debug_struct("SessionSecretStore")
            .field("entry_count", &count)
            .finish()
    }
}

impl SessionSecretStore {
    pub(crate) fn clone_for_profile(
        &self,
        profile_id: &ProfileId,
    ) -> Result<Option<Arc<SessionSecret>>, SecretError> {
        self.values
            .read()
            .map_err(|_| SecretError::StoreUnavailable)
            .map(|values| values.get(profile_id).cloned())
    }

    pub fn apply(
        &self,
        profile_id: &ProfileId,
        update: SessionSecretUpdate,
    ) -> Result<(), SecretError> {
        match update {
            SessionSecretUpdate::Keep => Ok(()),
            SessionSecretUpdate::Replace(secret) => {
                self.values
                    .write()
                    .map_err(|_| SecretError::StoreUnavailable)?
                    .insert(profile_id.clone(), secret);
                Ok(())
            }
            SessionSecretUpdate::Clear => {
                self.values
                    .write()
                    .map_err(|_| SecretError::StoreUnavailable)?
                    .remove(profile_id);
                Ok(())
            }
        }
    }

    pub fn is_empty(&self) -> Result<bool, SecretError> {
        self.values
            .read()
            .map_err(|_| SecretError::StoreUnavailable)
            .map(|values| values.is_empty())
    }

    pub fn has_current(&self, profile_id: &ProfileId) -> Result<bool, SecretError> {
        self.values
            .read()
            .map_err(|_| SecretError::StoreUnavailable)
            .map(|values| values.contains_key(profile_id))
    }

    pub(crate) fn clear_all(&self) -> Result<(), SecretError> {
        self.values
            .write()
            .map_err(|_| SecretError::StoreUnavailable)?
            .clear();
        Ok(())
    }

    pub(crate) fn retain_profiles(
        &self,
        profile_ids: &HashSet<ProfileId>,
    ) -> Result<(), SecretError> {
        self.values
            .write()
            .map_err(|_| SecretError::StoreUnavailable)?
            .retain(|profile_id, _| profile_ids.contains(profile_id));
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialEditContext {
    Create,
    Edit { has_current: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIntentPolicy {
    pub allowed: Vec<SessionCredentialIntent>,
    pub default: SessionCredentialIntent,
}

pub fn session_intent_policy(
    mode: CredentialMode,
    context: CredentialEditContext,
) -> Option<SessionIntentPolicy> {
    if mode != CredentialMode::Session {
        return None;
    }
    match context {
        CredentialEditContext::Edit { has_current: true } => Some(SessionIntentPolicy {
            allowed: vec![
                SessionCredentialIntent::KeepCurrent,
                SessionCredentialIntent::Replace,
                SessionCredentialIntent::Forget,
            ],
            default: SessionCredentialIntent::KeepCurrent,
        }),
        CredentialEditContext::Create | CredentialEditContext::Edit { has_current: false } => {
            Some(SessionIntentPolicy {
                allowed: vec![
                    SessionCredentialIntent::Replace,
                    SessionCredentialIntent::Forget,
                ],
                default: SessionCredentialIntent::Replace,
            })
        }
    }
}

pub fn session_update_for_save(
    mode: CredentialMode,
    context: CredentialEditContext,
    intent: Option<SessionCredentialIntent>,
    replacement: Option<Arc<SessionSecret>>,
) -> Result<SessionSecretUpdate, SecretError> {
    let Some(policy) = session_intent_policy(mode, context) else {
        if intent.is_some() || replacement.is_some() {
            return Err(SecretError::InvalidSessionIntent);
        }
        return Ok(SessionSecretUpdate::Clear);
    };
    let intent = intent.ok_or(SecretError::InvalidSessionIntent)?;
    if !policy.allowed.contains(&intent) {
        return Err(SecretError::InvalidSessionIntent);
    }
    match intent {
        SessionCredentialIntent::KeepCurrent if replacement.is_none() => {
            Ok(SessionSecretUpdate::Keep)
        }
        SessionCredentialIntent::KeepCurrent => Err(SecretError::InvalidSessionIntent),
        SessionCredentialIntent::Replace => replacement
            .map(SessionSecretUpdate::Replace)
            .ok_or(SecretError::ReplacementRequired),
        SessionCredentialIntent::Forget if replacement.is_none() => Ok(SessionSecretUpdate::Clear),
        SessionCredentialIntent::Forget => Err(SecretError::InvalidSessionIntent),
    }
}

pub fn resolve_environment(name: &str) -> Result<Arc<SessionSecret>, SecretError> {
    let value = std::env::var(name).map_err(|_| SecretError::MissingEnv(name.to_owned()))?;
    if value.is_empty() {
        return Err(SecretError::EmptyEnv(name.to_owned()));
    }
    Ok(Arc::new(SessionSecret::new(value)))
}

pub fn probe_environment(name: &str) -> EnvironmentAvailability {
    classify_environment_value(std::env::var(name))
}

fn classify_environment_value(
    value: Result<String, std::env::VarError>,
) -> EnvironmentAvailability {
    match value {
        Ok(value) => {
            let value = Zeroizing::new(value);
            if value.is_empty() {
                EnvironmentAvailability::Empty
            } else {
                EnvironmentAvailability::Available
            }
        }
        Err(_) => EnvironmentAvailability::Missing,
    }
}

#[cfg(test)]
mod tests {
    use zeroize::{ZeroizeOnDrop, Zeroizing};

    use super::{EnvironmentAvailability, classify_environment_value};

    #[test]
    fn environment_probe_owned_value_is_zeroize_on_drop_and_returns_only_availability() {
        fn requires_zeroize_on_drop<T: ZeroizeOnDrop>() {}

        requires_zeroize_on_drop::<Zeroizing<String>>();
        let sentinel = "environment-probe-secret-sentinel";
        let available = classify_environment_value(Ok(sentinel.to_owned()));
        let empty = classify_environment_value(Ok(String::new()));
        let missing = classify_environment_value(Err(std::env::VarError::NotPresent));

        assert_eq!(available, EnvironmentAvailability::Available);
        assert_eq!(empty, EnvironmentAvailability::Empty);
        assert_eq!(missing, EnvironmentAvailability::Missing);
        assert!(!format!("{available:?}").contains(sentinel));
    }
}
