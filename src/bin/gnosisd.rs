use std::{net::SocketAddr, path::PathBuf, process::ExitCode};

use clap::Parser;
use gnosis_core::protocol::{DaemonConfig, DaemonServer};

#[derive(Debug, Parser)]
#[command(name = "gnosisd")]
struct Cli {
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,
    #[arg(long, value_name = "PATH")]
    database: PathBuf,
    #[arg(long, value_name = "PATH")]
    blob_root: Option<PathBuf>,
    #[arg(long, value_name = "BACKUP")]
    restore_from: Option<PathBuf>,
    #[arg(long, value_name = "ADDRESS", default_value = "127.0.0.1:7842")]
    http_listen: SocketAddr,
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
    let mut config = DaemonConfig::new(cli.socket, cli.database);
    config.http_addr = Some(cli.http_listen);
    if let Some(blob_root) = cli.blob_root {
        config.blob_path = blob_root;
    }
    if let Some(backup) = cli.restore_from {
        let rollback = DaemonServer::restore_backup(config, backup).await?;
        println!(
            "restore complete; rollback preserved at {}",
            rollback.display()
        );
        return Ok(());
    }
    let server = DaemonServer::bind(config).await?;
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
