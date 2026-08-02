use std::{
    io::{self, Write},
    path::PathBuf,
    process::ExitCode,
};

use chrono::Utc;
use clap::{Parser, Subcommand};
use gnosis_core::protocol::{
    ClientError, ErrorCode, PROTOCOL_VERSION, ProtocolError, Request, RequestEnvelope,
    ResponseOutcome, ResponseResult, request,
};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(name = "gnosisd-admin")]
struct Cli {
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    SystemInfo,
    DeviceCreate {
        name: String,
        #[arg(long = "scope", required = true)]
        scopes: Vec<String>,
    },
    DeviceList,
    DeviceRevoke {
        device_id: String,
    },
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    Repair {
        #[arg(long, default_value_t = 100, value_name = "N")]
        batch_size: u16,
    },
    Backup {
        #[command(subcommand)]
        command: BackupCommand,
    },
}

#[derive(Debug, Subcommand)]
enum BackupCommand {
    Create {
        #[arg(required_unless_present = "directory", conflicts_with = "directory")]
        destination: Option<PathBuf>,
        #[arg(long, value_name = "PATH")]
        directory: Option<PathBuf>,
    },
    Verify {
        path: PathBuf,
    },
    Prune {
        #[arg(long, value_name = "PATH")]
        directory: PathBuf,
        #[arg(long, default_value_t = 14)]
        keep: u16,
    },
    RestoreValidate {
        path: PathBuf,
        #[arg(long, value_name = "PATH")]
        temporary_directory: PathBuf,
    },
}

#[derive(Debug, Error)]
enum CliError {
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("backup path is not valid UTF-8")]
    Path,
    #[error("could not encode daemon response")]
    Encode(#[source] serde_json::Error),
    #[error("could not write daemon response")]
    WriteOutput(#[source] io::Error),
}

#[derive(Debug, Error)]
enum RunError {
    #[error(transparent)]
    Client(#[from] CliError),
    #[error("protocol request failed")]
    Protocol(ProtocolError),
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(RunError::Protocol(error)) => {
            eprintln!("{}: {}", error_code(error.code), error.message);
            ExitCode::FAILURE
        }
        Err(RunError::Client(error)) => {
            eprintln!("gnosisd-admin: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<(), RunError> {
    let envelope = RequestEnvelope {
        version: PROTOCOL_VERSION,
        id: correlation_id(),
        request: command_request(cli.command)?,
    };
    let response = request(cli.socket, &envelope)
        .await
        .map_err(CliError::from)?;
    match response.outcome {
        ResponseOutcome::Success { result } => write_result(&result).map_err(Into::into),
        ResponseOutcome::Failure { error } => Err(RunError::Protocol(error)),
    }
}

fn command_request(command: Command) -> Result<Request, CliError> {
    match command {
        Command::SystemInfo => Ok(Request::SystemInfo {}),
        Command::DeviceCreate { name, scopes } => Ok(Request::DeviceCreate { name, scopes }),
        Command::DeviceList => Ok(Request::DeviceList {}),
        Command::DeviceRevoke { device_id } => Ok(Request::DeviceRevoke { device_id }),
        Command::Admin { command } => admin_request(command),
    }
}

fn admin_request(command: AdminCommand) -> Result<Request, CliError> {
    match command {
        AdminCommand::Repair { batch_size } => Ok(Request::Repair { batch_size }),
        AdminCommand::Backup { command } => match command {
            BackupCommand::Create {
                destination,
                directory,
            } => Ok(Request::BackupCreate {
                destination: utf8_path(backup_destination(destination, directory)?)?,
            }),
            BackupCommand::Verify { path } => Ok(Request::BackupVerify {
                path: utf8_path(path)?,
            }),
            BackupCommand::Prune { directory, keep } => Ok(Request::BackupPrune {
                directory: utf8_path(directory)?,
                keep,
            }),
            BackupCommand::RestoreValidate {
                path,
                temporary_directory,
            } => Ok(Request::BackupRestoreValidate {
                path: utf8_path(path)?,
                temporary_directory: utf8_path(temporary_directory)?,
            }),
        },
    }
}

fn backup_destination(
    destination: Option<PathBuf>,
    directory: Option<PathBuf>,
) -> Result<PathBuf, CliError> {
    match (destination, directory) {
        (Some(path), None) => Ok(path),
        (None, Some(directory)) => {
            Ok(directory.join(format!("gnosis-{}", Utc::now().format("%Y%m%dT%H%M%SZ"))))
        }
        _ => Err(CliError::Path),
    }
}

fn utf8_path(path: PathBuf) -> Result<String, CliError> {
    path.into_os_string()
        .into_string()
        .map_err(|_| CliError::Path)
}

fn correlation_id() -> String {
    format!("gnosisd-admin-{}", Uuid::now_v7())
}

fn write_result(result: &ResponseResult) -> Result<(), CliError> {
    match result {
        ResponseResult::SystemInfo(value) => write_json(value),
        ResponseResult::DeviceCreated(value) => write_json(value),
        ResponseResult::Devices(value) => write_json(value),
        ResponseResult::Device(value) => write_json(value),
        ResponseResult::Backup(value) => write_json(value),
        ResponseResult::BackupsPruned(value) => write_json(value),
        ResponseResult::Repair(value) => write_json(value),
    }
}

fn write_json(value: &impl Serialize) -> Result<(), CliError> {
    let body = serde_json::to_vec(value).map_err(CliError::Encode)?;
    let mut output = io::stdout().lock();
    output.write_all(&body).map_err(CliError::WriteOutput)?;
    writeln!(output).map_err(CliError::WriteOutput)
}

fn error_code(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::MalformedRequest => "malformed_request",
        ErrorCode::UnsupportedVersion => "unsupported_version",
        ErrorCode::InvalidDocument => "invalid_document",
        ErrorCode::NotFound => "not_found",
        ErrorCode::Conflict => "conflict",
        ErrorCode::RevisionConflict => "revision_conflict",
        ErrorCode::IdempotencyConflict => "idempotency_conflict",
        ErrorCode::StaleAttempt => "stale_attempt",
        ErrorCode::ReviewStateConflict => "review_state_conflict",
        ErrorCode::Busy => "busy",
        ErrorCode::Internal => "internal",
    }
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;
    use gnosis_core::protocol::MAX_REPAIR_BATCH_SIZE;

    use super::{AdminCommand, BackupCommand, Cli, Command, Request, command_request};

    #[test]
    fn parses_device_and_maintenance_commands() {
        let create = Cli::try_parse_from([
            "gnosisd-admin",
            "--socket",
            "/tmp/gnosis.sock",
            "device-create",
            "emacs",
            "--scope",
            "read",
            "--scope",
            "review",
        ])
        .unwrap();
        assert!(matches!(
            command_request(create.command).unwrap(),
            Request::DeviceCreate { name, scopes }
                if name == "emacs" && scopes == ["read", "review"]
        ));

        let repair = Cli::try_parse_from([
            "gnosisd-admin",
            "--socket",
            "/tmp/gnosis.sock",
            "admin",
            "repair",
            "--batch-size",
            &MAX_REPAIR_BATCH_SIZE.to_string(),
        ])
        .unwrap();
        assert!(matches!(
            command_request(repair.command).unwrap(),
            Request::Repair {
                batch_size: MAX_REPAIR_BATCH_SIZE
            }
        ));
    }

    #[test]
    fn backup_defaults_remain_bounded_and_explicit() {
        let prune = command_request(Command::Admin {
            command: AdminCommand::Backup {
                command: BackupCommand::Prune {
                    directory: "/backups".into(),
                    keep: 14,
                },
            },
        })
        .unwrap();
        assert_eq!(
            prune,
            Request::BackupPrune {
                directory: "/backups".into(),
                keep: 14,
            }
        );
    }
}
