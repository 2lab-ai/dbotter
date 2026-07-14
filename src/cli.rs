use std::time::Duration;

use clap::{Parser, Subcommand, ValueEnum};

use crate::error::AppError;
use crate::model::{CheckReceipt, ExecReceipt, ExecuteRequest, OperationId, ProfileId};
use crate::service::ApplicationService;

#[derive(Debug, Parser)]
#[command(name = "dbotter", version, about = "Local Rust database client")]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Open the native desktop client.
    Gui,
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum OutputFormat {
    Json,
}

pub async fn run(cli: Cli) -> Result<(), AppError> {
    match cli.command.unwrap_or(Command::Gui) {
        Command::Gui => run_gui(),
        Command::Check {
            profile,
            format,
            timeout_secs,
        } => {
            let service = ApplicationService::load()?;
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
                return Err(AppError::InvalidInput(
                    "row-limit must be between 1 and 10000".to_owned(),
                ));
            }
            let service = ApplicationService::load()?;
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
        Command::Drivers => print_json(&crate::drivers::descriptors()),
    }
}

async fn check(
    service: &ApplicationService,
    profile_id: &str,
    format: OutputFormat,
    timeout: Duration,
) -> Result<(), AppError> {
    let outcome = service
        .check(OperationId(1), ProfileId(profile_id.to_owned()), timeout)
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
    let outcome = service
        .execute(ExecuteRequest {
            operation_id: OperationId(1),
            profile_id,
            language,
            text,
            row_limit,
            timeout,
        })
        .await?;
    let receipt = ExecReceipt {
        status: "ok",
        operation_id: outcome.operation_id,
        profile_id: outcome.profile_id.0,
        driver: outcome.driver,
        endpoint: outcome.endpoint,
        result: outcome.result,
    };
    match format {
        OutputFormat::Json => print_json(&receipt),
    }
}

fn print_json(value: &impl serde::Serialize) -> Result<(), AppError> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}

#[cfg(feature = "desktop")]
fn run_gui() -> Result<(), AppError> {
    crate::ui::run()
}

#[cfg(not(feature = "desktop"))]
fn run_gui() -> Result<(), AppError> {
    Err(AppError::DesktopDisabled)
}
