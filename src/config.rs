use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::ConnectionProfile;

pub const CONFIG_ENV: &str = "DBOTTER_CONFIG";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config io error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("config parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("config serialization error: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error("unsupported config version {0}")]
    UnsupportedVersion(u32),
    #[error("could not determine config path; set DBOTTER_CONFIG")]
    NoConfigPath,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    #[serde(default)]
    pub profiles: Vec<ConnectionProfile>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            version: 1,
            profiles: Vec::new(),
        }
    }
}

pub fn config_path() -> Result<PathBuf, ConfigError> {
    if let Some(path) = std::env::var_os(CONFIG_ENV).filter(|value| !value.is_empty()) {
        return Ok(path.into());
    }
    let home = std::env::var_os("HOME").ok_or(ConfigError::NoConfigPath)?;
    Ok(PathBuf::from(home).join(".config/dbotter/config.toml"))
}

pub fn load() -> Result<Config, ConfigError> {
    load_path(&config_path()?)
}

pub fn load_path(path: &Path) -> Result<Config, ConfigError> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config::default());
        }
        Err(source) => {
            return Err(ConfigError::Io {
                path: path.to_owned(),
                source,
            });
        }
    };
    let config: Config = toml::from_str(&raw)?;
    if config.version != 1 {
        return Err(ConfigError::UnsupportedVersion(config.version));
    }
    Ok(config)
}

pub fn upsert_profile_path(path: &Path, profile: ConnectionProfile) -> Result<Config, ConfigError> {
    let mut config = load_path(path)?;
    match config
        .profiles
        .iter_mut()
        .find(|candidate| candidate.id == profile.id)
    {
        Some(existing) => *existing = profile,
        None => config.profiles.push(profile),
    }
    save_path(path, &config)?;
    Ok(config)
}

pub fn save_path(path: &Path, config: &Config) -> Result<(), ConfigError> {
    let directory = path.parent().filter(|path| !path.as_os_str().is_empty());
    if let Some(directory) = directory {
        fs::create_dir_all(directory).map_err(|source| ConfigError::Io {
            path: directory.to_owned(),
            source,
        })?;
    }
    let directory = directory.unwrap_or_else(|| Path::new("."));
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let temp = directory.join(format!(
        ".dbotter-config.tmp.{}.{}",
        std::process::id(),
        stamp
    ));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&temp).map_err(|source| ConfigError::Io {
        path: temp.clone(),
        source,
    })?;
    let mut encoded = toml::to_string_pretty(config)?;
    encoded.push('\n');
    let result = (|| {
        file.write_all(encoded.as_bytes())
            .map_err(|source| ConfigError::Io {
                path: temp.clone(),
                source,
            })?;
        file.sync_all().map_err(|source| ConfigError::Io {
            path: temp.clone(),
            source,
        })?;
        drop(file);
        fs::rename(&temp, path).map_err(|source| ConfigError::Io {
            path: path.to_owned(),
            source,
        })?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_contains_secret_reference_not_secret_value() {
        let config: Config = toml::from_str(
            r#"
version = 1
[[profiles]]
id = "mysql-local"
name = "Local MySQL"
driver = "mysql"
host = "127.0.0.1"
port = 33306
username = "dbotter"
secret_env = "DBOTTER_MYSQL_PASSWORD"
"#,
        )
        .expect("fixture parses");
        let encoded = toml::to_string(&config).expect("config serializes");
        assert!(encoded.contains("DBOTTER_MYSQL_PASSWORD"));
        assert!(!encoded.contains("local-only-password"));
    }

    #[test]
    fn unsupported_version_is_typed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        fs::write(&path, "version = 2\n").expect("write fixture");
        assert!(matches!(
            load_path(&path),
            Err(ConfigError::UnsupportedVersion(2))
        ));
    }

    #[test]
    fn upsert_profile_replaces_by_id_and_writes_atomically() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("config.toml");
        let profile = ConnectionProfile {
            id: "redis-local".into(),
            name: "Redis".into(),
            driver: crate::model::DriverKind::Redis,
            host: "127.0.0.1".into(),
            port: 6379,
            database: None,
            username: None,
            tls: crate::model::TlsMode::Disabled,
            secret_env: None,
        };
        upsert_profile_path(&path, profile.clone()).expect("first upsert");
        let mut replacement = profile;
        replacement.port = 36379;
        let config = upsert_profile_path(&path, replacement).expect("replacement upsert");
        assert_eq!(config.profiles.len(), 1);
        assert_eq!(config.profiles[0].port, 36379);
        let temp_files = fs::read_dir(dir.path())
            .expect("read dir")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp."))
            .count();
        assert_eq!(temp_files, 0);
    }
}
