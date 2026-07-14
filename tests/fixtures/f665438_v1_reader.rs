//! Frozen copy of the f665438 version-1 read boundary.
//!
//! Keep this fixture independent of current config/model types: it represents
//! the exact wire fields and load-before-service order of the previous reader.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, PartialEq, Eq)]
pub enum FrozenReaderError {
    Io,
    Parse,
    UnsupportedVersion(u32),
}

#[derive(Deserialize)]
struct Config {
    version: u32,
    #[serde(default)]
    profiles: Vec<ConnectionProfile>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ConnectionProfile {
    id: String,
    name: String,
    driver: DriverKind,
    #[serde(default = "default_host")]
    host: String,
    port: u16,
    #[serde(default)]
    database: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    tls: TlsMode,
    #[serde(default)]
    secret_env: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum DriverKind {
    MySql,
    Redis,
    MongoDb,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
enum TlsMode {
    Disabled,
    #[default]
    Preferred,
    Required,
}

fn default_host() -> String {
    "127.0.0.1".to_owned()
}

fn load_path(path: &Path) -> Result<Config, FrozenReaderError> {
    let raw = match fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Config {
                version: 1,
                profiles: Vec::new(),
            });
        }
        Err(_) => return Err(FrozenReaderError::Io),
    };
    let config: Config = toml::from_str(&raw).map_err(|_| FrozenReaderError::Parse)?;
    if config.version != 1 {
        return Err(FrozenReaderError::UnsupportedVersion(config.version));
    }
    Ok(config)
}

pub fn load_before_service_or_network(
    path: PathBuf,
    construct_service: impl FnOnce(),
    acquire_network: impl FnOnce(),
) -> Result<(), FrozenReaderError> {
    let config = load_path(&path)?;
    let _profile_count = config.profiles.len();
    construct_service();
    acquire_network();
    Ok(())
}
