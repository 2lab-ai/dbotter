use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

use crate::error::AppError;
use crate::model::{CheckReceipt, ExecOutput, ExecReceipt, ExecuteRequest, OperationId, ProfileId};
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
        #[arg(long, default_value_t = 1_000)]
        row_limit: u32,
        #[arg(long, default_value_t = 30)]
        timeout_secs: u64,
        #[arg(long, value_enum, default_value = "json")]
        format: OutputFormat,
    },
    /// Print driver availability/capabilities.
    Drivers,
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
        Command::Version { .. } | Command::ConfigContract { .. } | Command::Drivers => {
            Err(AppError::InvalidInput)
        }
    }
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
        .execute_at(
            ExecuteRequest {
                operation_id: OperationId(1),
                profile_id,
                language,
                text,
                row_limit,
                timeout,
            },
            profile_generation,
        )
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
    fn identity_and_config_contract_are_separate_exact_objects() {
        let identity = serde_json::to_value(crate::build_info::identity()).expect("identity");
        assert_eq!(identity.as_object().map(|value| value.len()), Some(6));
        let contract = serde_json::to_value(crate::config::config_contract()).expect("contract");
        assert_eq!(contract.as_object().map(|value| value.len()), Some(3));
        assert!(contract.get("package_version").is_none());
        assert!(identity.get("read_versions").is_none());
    }
}
