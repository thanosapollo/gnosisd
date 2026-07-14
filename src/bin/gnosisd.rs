use std::{path::PathBuf, process::ExitCode};

use clap::Parser;
use gnosis_core::{
    migration::{LegacyMigrationSummary, migrate_legacy_database, migrate_legacy_server},
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
    #[arg(long, value_name = "PATH", requires_all = ["migrate_legacy", "blob_root"])]
    notes_root: Option<PathBuf>,
    #[arg(long, value_name = "PATH", requires_all = ["migrate_legacy", "notes_root"])]
    blob_root: Option<PathBuf>,
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
        let summary = match (cli.notes_root, cli.blob_root) {
            (Some(notes), Some(blobs)) => {
                migrate_legacy_server(source, cli.database, notes, blobs)?
            }
            (None, None) => migrate_legacy_database(source, cli.database)?,
            _ => return Err("--notes-root and --blob-root must be used together".into()),
        };
        print_migration_summary(summary);
        return Ok(());
    }
    let socket = cli.socket.ok_or("socket is required when serving")?;
    let server = DaemonServer::bind(DaemonConfig::new(socket, cli.database)).await?;
    server.run(shutdown_signal()).await?;
    Ok(())
}

fn print_migration_summary(summary: LegacyMigrationSummary) {
    if summary.note_count() == 0 && summary.asset_count() == 0 {
        println!(
            "migrated legacy schema v{}: {} tables, {} rows",
            summary.source_schema_version(),
            summary.legacy_table_count(),
            summary.legacy_row_count()
        );
    } else {
        println!(
            "migrated legacy schema v{}: {} tables, {} rows, {} notes, {} assets",
            summary.source_schema_version(),
            summary.legacy_table_count(),
            summary.legacy_row_count(),
            summary.note_count(),
            summary.asset_count()
        );
    }
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
