use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use gnosis_core::{
    migration::{LegacyMigrationSummary, migrate_legacy_database},
    protocol::{DaemonConfig, DaemonServer},
};

#[derive(Debug, Parser)]
#[command(name = "gnosisd")]
struct Cli {
    #[arg(long, value_name = "PATH", required_unless_present = "migrate_legacy")]
    socket: Option<PathBuf>,
    #[arg(long, value_name = "PATH")]
    database: PathBuf,
    #[arg(long, value_name = "PATH", conflicts_with = "socket")]
    migrate_legacy: Option<PathBuf>,
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
    if let Some(source) = cli.migrate_legacy {
        let summary = migrate_legacy_database(source, cli.database)?;
        print_migration_summary(summary);
        return Ok(());
    }
    let socket = cli.socket.ok_or("socket is required when serving")?;
    let server = DaemonServer::bind(DaemonConfig::new(socket, cli.database)).await?;
    server.run(shutdown_signal()).await?;
    Ok(())
}

fn print_migration_summary(summary: LegacyMigrationSummary) {
    println!(
        "migrated legacy schema v{}: {} tables, {} rows",
        summary.source_schema_version(),
        summary.legacy_table_count(),
        summary.legacy_row_count()
    );
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
