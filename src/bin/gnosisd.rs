use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use gnosis_core::protocol::{DaemonConfig, DaemonServer};

#[derive(Debug, Parser)]
#[command(name = "gnosisd")]
struct Cli {
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,
    #[arg(long, value_name = "PATH")]
    database: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("gnosisd: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let server = DaemonServer::bind(DaemonConfig::new(cli.socket, cli.database)).await?;
    server.run(shutdown_signal()).await?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let Ok(mut terminate) = signal(SignalKind::terminate()) else {
            let _ = tokio::signal::ctrl_c().await;
            return;
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = terminate.recv() => {}
        }
    }

    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
