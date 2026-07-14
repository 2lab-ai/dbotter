use secrecy::SecretString;

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("required secret environment variable is missing: {0}")]
    MissingEnv(String),
    #[error("required secret environment variable is empty: {0}")]
    EmptyEnv(String),
}

pub fn resolve(secret_env: Option<&str>) -> Result<Option<SecretString>, SecretError> {
    let Some(name) = secret_env else {
        return Ok(None);
    };
    let value = std::env::var(name).map_err(|_| SecretError::MissingEnv(name.to_owned()))?;
    if value.is_empty() {
        return Err(SecretError::EmptyEnv(name.to_owned()));
    }
    Ok(Some(SecretString::from(value)))
}
