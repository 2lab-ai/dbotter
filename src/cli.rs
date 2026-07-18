use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use base64::Engine as _;
use clap::{Parser, Subcommand, ValueEnum};

use crate::error::AppError;
use crate::model::{
    CatalogPageToken, CatalogRequest, CheckReceipt, DEFAULT_CATALOG_PAGE_SIZE,
    DEFAULT_CATALOG_TIMEOUT, DEFAULT_EXECUTE_ROWS, DEFAULT_REDIS_SCAN_COUNT, ExecOutput,
    ExecReceipt, ExecuteRequest, OperationId, ProfileId, RedisKeyFilter, RedisKeyId,
    RedisKeyInspectRequest, RedisScanRequest, RequestIdentity,
};
use crate::service::ApplicationService;

#[derive(Parser)]
#[command(
    name = "dbotter",
    version = crate::build_info::version_with_build(),
    about = "Local Rust database client"
)]
pub struct Cli {
    /// Use exactly this configuration file.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the native desktop client.
    Gui,
    /// Print the exact six-field binary identity.
    Version {
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
    /// Print the independent three-field config compatibility contract.
    ConfigContract {
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
    /// Prove the exact private workspace retention limits and their +1 rejection.
    WorkspaceContract {
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
    /// Connect to a profile and ping its server.
    Check {
        #[arg(long)]
        profile: String,
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
        #[arg(long, default_value_t = 10)]
        timeout_secs: u64,
    },
    /// Execute one SQL statement or Redis command.
    Exec {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        text: String,
        #[arg(long, default_value_t = DEFAULT_EXECUTE_ROWS)]
        row_limit: u32,
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
    /// Print driver availability/capabilities.
    Drivers,
    /// Browse one typed backend resource.
    Browse {
        #[command(subcommand)]
        backend: BrowseCommand,
    },
    /// Inspect one typed backend resource.
    Inspect {
        #[command(subcommand)]
        backend: InspectCommand,
    },
}

#[derive(Subcommand)]
enum BrowseCommand {
    /// Browse the MySQL catalog lazily.
    #[command(name = "mysql")]
    MySql {
        #[command(subcommand)]
        resource: MySqlBrowseCommand,
    },
    /// Browse the Redis keyspace with SCAN semantics.
    Redis {
        #[command(subcommand)]
        resource: RedisBrowseCommand,
    },
}

#[derive(Subcommand)]
enum MySqlBrowseCommand {
    Schemas {
        #[arg(long)]
        profile: String,
        #[arg(long, default_value_t = DEFAULT_CATALOG_PAGE_SIZE)]
        page_size: u16,
        #[arg(long)]
        page_token: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
    Relations {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        schema: String,
        #[arg(long, default_value_t = DEFAULT_CATALOG_PAGE_SIZE)]
        page_size: u16,
        #[arg(long)]
        page_token: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
    Columns {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        schema: String,
        #[arg(long)]
        relation: String,
        #[arg(long, default_value_t = DEFAULT_CATALOG_PAGE_SIZE)]
        page_size: u16,
        #[arg(long)]
        page_token: Option<String>,
        #[arg(long)]
        prefix: Option<String>,
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
}

#[derive(Subcommand)]
enum RedisBrowseCommand {
    Keys {
        #[arg(long)]
        profile: String,
        #[arg(long, value_enum, default_value = "literal-prefix")]
        filter_mode: RedisFilterMode,
        #[arg(long, default_value = "")]
        filter: String,
        #[arg(long, default_value_t = 0)]
        cursor: u64,
        #[arg(long, default_value_t = DEFAULT_REDIS_SCAN_COUNT)]
        count: u32,
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
}

#[derive(Subcommand)]
enum InspectCommand {
    Redis {
        #[command(subcommand)]
        resource: RedisInspectCommand,
    },
}

#[derive(Subcommand)]
enum RedisInspectCommand {
    Key {
        #[arg(long)]
        profile: String,
        #[arg(long)]
        key_base64: String,
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum RedisFilterMode {
    LiteralPrefix,
    Glob,
}

#[derive(Clone, Copy, ValueEnum)]
enum OutputFormat {
    Json,
}

impl Cli {
    pub fn resolve_config_path(
        &self,
        environment: Option<&OsStr>,
        home: Option<&OsStr>,
    ) -> Result<PathBuf, crate::config::ConfigError> {
        crate::config::resolve_config_path(self.config.as_deref(), environment, home)
    }
}

pub async fn run(cli: Cli) -> Result<(), AppError> {
    let Cli { config, command } = cli;
    match command.unwrap_or(Command::Gui) {
        Command::Version { format } => match format {
            OutputFormat::Json => print_json(&crate::build_info::identity()),
        },
        Command::ConfigContract { format } => match format {
            OutputFormat::Json => print_json(&crate::config::config_contract()),
        },
        Command::WorkspaceContract { format } => match format {
            OutputFormat::Json => print_json(&crate::workspace::workspace_contract()),
        },
        Command::Drivers => print_json(&crate::drivers::descriptors()),
        command => {
            let config_path = crate::config::resolve_config_path(
                config.as_deref(),
                std::env::var_os(crate::config::CONFIG_ENV).as_deref(),
                std::env::var_os("HOME").as_deref(),
            )?;
            run_with_config(command, config_path).await
        }
    }
}

async fn run_with_config(command: Command, config_path: PathBuf) -> Result<(), AppError> {
    match command {
        Command::Gui => run_gui(config_path).await,
        Command::Check {
            profile,
            format,
            timeout_secs,
        } => {
            let service = ApplicationService::load_path(config_path)?;
            check(
                &service,
                &profile,
                format,
                Duration::from_secs(timeout_secs),
            )
            .await
        }
        Command::Exec {
            profile,
            text,
            row_limit,
            timeout_secs,
            format,
        } => {
            if row_limit == 0 || row_limit > 10_000 {
                return Err(AppError::InvalidInput);
            }
            let service = ApplicationService::load_path(config_path)?;
            execute(
                &service,
                &profile,
                text,
                row_limit,
                Duration::from_secs(timeout_secs),
                format,
            )
            .await
        }
        Command::Browse { backend } => {
            let service = ApplicationService::load_path(config_path)?;
            browse(&service, backend).await
        }
        Command::Inspect { backend } => {
            let service = ApplicationService::load_path(config_path)?;
            inspect(&service, backend).await
        }
        Command::Version { .. }
        | Command::ConfigContract { .. }
        | Command::WorkspaceContract { .. }
        | Command::Drivers => Err(AppError::InvalidInput),
    }
}

async fn browse(service: &ApplicationService, command: BrowseCommand) -> Result<(), AppError> {
    match command {
        BrowseCommand::MySql { resource } => {
            let (request, format) = match resource {
                MySqlBrowseCommand::Schemas {
                    profile,
                    page_size,
                    page_token,
                    prefix,
                    format,
                } => {
                    let identity = request_identity(service, &profile).await?;
                    let request = CatalogRequest::Schemas {
                        identity,
                        prefix,
                        page_token: page_token.map(CatalogPageToken),
                        page_size,
                        timeout: DEFAULT_CATALOG_TIMEOUT,
                    };
                    (request, format)
                }
                MySqlBrowseCommand::Relations {
                    profile,
                    schema,
                    page_size,
                    page_token,
                    prefix,
                    format,
                } => {
                    let identity = request_identity(service, &profile).await?;
                    let request = CatalogRequest::Relations {
                        identity,
                        schema,
                        prefix,
                        page_token: page_token.map(CatalogPageToken),
                        page_size,
                        timeout: DEFAULT_CATALOG_TIMEOUT,
                    };
                    (request, format)
                }
                MySqlBrowseCommand::Columns {
                    profile,
                    schema,
                    relation,
                    page_size,
                    page_token,
                    prefix,
                    format,
                } => {
                    let identity = request_identity(service, &profile).await?;
                    let request = CatalogRequest::Columns {
                        identity,
                        schema,
                        relation,
                        prefix,
                        page_token: page_token.map(CatalogPageToken),
                        page_size,
                        timeout: DEFAULT_CATALOG_TIMEOUT,
                    };
                    (request, format)
                }
            };
            let page = service.load_catalog_page(request).await?;
            match format {
                OutputFormat::Json => print_json(&page),
            }
        }
        BrowseCommand::Redis {
            resource:
                RedisBrowseCommand::Keys {
                    profile,
                    filter_mode,
                    filter,
                    cursor,
                    count,
                    format,
                },
        } => {
            let identity = request_identity(service, &profile).await?;
            let filter = match filter_mode {
                RedisFilterMode::LiteralPrefix => RedisKeyFilter::LiteralPrefix(filter),
                RedisFilterMode::Glob => RedisKeyFilter::Glob(filter),
            };
            let page = service
                .scan_redis_keys(RedisScanRequest {
                    identity,
                    filter,
                    cursor,
                    count_hint: count,
                    timeout: DEFAULT_CATALOG_TIMEOUT,
                })
                .await?;
            match format {
                OutputFormat::Json => print_json(&page),
            }
        }
    }
}

async fn inspect(service: &ApplicationService, command: InspectCommand) -> Result<(), AppError> {
    match command {
        InspectCommand::Redis {
            resource:
                RedisInspectCommand::Key {
                    profile,
                    key_base64,
                    format,
                },
        } => {
            let identity = request_identity(service, &profile).await?;
            let key = base64::engine::general_purpose::STANDARD
                .decode(key_base64.as_bytes())
                .map_err(|_| AppError::InvalidInput)?;
            let preview = service
                .inspect_redis_key(RedisKeyInspectRequest {
                    identity,
                    key: RedisKeyId(key),
                    timeout: DEFAULT_CATALOG_TIMEOUT,
                })
                .await?;
            match format {
                OutputFormat::Json => print_json(&preview),
            }
        }
    }
}

async fn request_identity(
    service: &ApplicationService,
    profile: &str,
) -> Result<RequestIdentity, AppError> {
    let profile_id = ProfileId(profile.to_owned());
    let generation = service.profile_generation(&profile_id).await?;
    Ok(RequestIdentity::new(profile_id, generation, OperationId(1)))
}

async fn check(
    service: &ApplicationService,
    profile_id: &str,
    format: OutputFormat,
    timeout: Duration,
) -> Result<(), AppError> {
    let profile_id = ProfileId(profile_id.to_owned());
    let profile_generation = service.profile_generation(&profile_id).await?;
    let outcome = service
        .check_at(OperationId(1), profile_id, profile_generation, timeout)
        .await?;
    let receipt = CheckReceipt {
        status: "ok",
        operation_id: outcome.operation_id,
        profile_id: outcome.profile_id.0,
        driver: outcome.driver,
        endpoint: outcome.endpoint,
        elapsed_ms: outcome.elapsed_ms,
    };
    match format {
        OutputFormat::Json => print_json(&receipt),
    }
}

async fn execute(
    service: &ApplicationService,
    profile_id: &str,
    text: String,
    row_limit: u32,
    timeout: Duration,
    format: OutputFormat,
) -> Result<(), AppError> {
    let profile_id = ProfileId(profile_id.to_owned());
    let language = service.language_for(&profile_id).await?;
    let profile_generation = service.profile_generation(&profile_id).await?;
    let outcome = service
        .execute_at(ExecuteRequest {
            operation_id: OperationId(1),
            profile_id,
            profile_generation,
            language,
            text,
            row_limit,
            timeout,
        })
        .await?;
    let receipt = ExecReceipt::from_result(
        "ok",
        outcome.operation_id,
        outcome.profile_id,
        outcome.driver,
        &outcome.result,
    );
    let output = ExecOutput {
        receipt,
        result: outcome.result,
    };
    match format {
        OutputFormat::Json => print_json(&output),
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<(), AppError> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

#[cfg(feature = "desktop")]
async fn run_gui(config_path: PathBuf) -> Result<(), AppError> {
    crate::ui::run(config_path).await
}

#[cfg(not(feature = "desktop"))]
async fn run_gui(_config_path: PathBuf) -> Result<(), AppError> {
    Err(AppError::DesktopDisabled)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_config_wins_over_environment_and_home() {
        let cli = Cli::try_parse_from(["dbotter", "--config", "/explicit/config.toml", "drivers"])
            .expect("CLI parses");
        assert_eq!(
            cli.resolve_config_path(
                Some(OsStr::new("/environment/config.toml")),
                Some(OsStr::new("/home/example")),
            )
            .expect("path"),
            std::path::Path::new("/explicit/config.toml")
        );
    }

    #[test]
    fn p3_headless_resource_forms_parse_exactly() {
        for arguments in [
            vec![
                "dbotter",
                "--config",
                "/tmp/config.toml",
                "browse",
                "mysql",
                "schemas",
                "--profile",
                "mysql-local",
                "--page-size",
                "50",
                "--format",
                "json",
            ],
            vec![
                "dbotter",
                "--config",
                "/tmp/config.toml",
                "browse",
                "mysql",
                "relations",
                "--profile",
                "mysql-local",
                "--schema",
                "dbotter",
                "--page-size",
                "50",
                "--page-token",
                "opaque",
                "--format",
                "json",
            ],
            vec![
                "dbotter",
                "--config",
                "/tmp/config.toml",
                "browse",
                "mysql",
                "columns",
                "--profile",
                "mysql-local",
                "--schema",
                "dbotter",
                "--relation",
                "receipt",
                "--page-size",
                "50",
                "--format",
                "json",
            ],
            vec![
                "dbotter",
                "--config",
                "/tmp/config.toml",
                "browse",
                "redis",
                "keys",
                "--profile",
                "redis-local",
                "--filter-mode",
                "literal-prefix",
                "--filter",
                "receipt:",
                "--cursor",
                "0",
                "--count",
                "100",
                "--format",
                "json",
            ],
            vec![
                "dbotter",
                "--config",
                "/tmp/config.toml",
                "inspect",
                "redis",
                "key",
                "--profile",
                "redis-local",
                "--key-base64",
                "cmVjZWlwdDptYXJrZXI=",
                "--format",
                "json",
            ],
        ] {
            Cli::try_parse_from(arguments).expect("frozen headless form parses");
        }
    }

    #[test]
    fn identity_and_config_contract_are_separate_exact_objects() {
        let identity = serde_json::to_value(crate::build_info::identity()).expect("identity");
        assert_eq!(identity.as_object().map(|value| value.len()), Some(6));
        let contract = serde_json::to_value(crate::config::config_contract()).expect("contract");
        assert_eq!(contract.as_object().map(|value| value.len()), Some(3));
        assert!(contract.get("package_version").is_none());
        assert!(identity.get("read_versions").is_none());
    }
}
