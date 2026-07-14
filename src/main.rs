use std::process::ExitCode;

use clap::Parser as _;

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "dbotter=info".into()),
        )
        .with_target(false)
        .init();

    match dbotter::cli::run(dbotter::cli::Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {}", error.public_message());
            ExitCode::FAILURE
        }
    }
}
