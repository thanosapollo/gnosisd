use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode;

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
#[command(name = "gnosis")]
struct Cli {
    #[arg(long, value_name = "PATH")]
    socket: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    SystemInfo,
    LegacyInfo,
    ImportOrg {
        document_id: String,
        #[arg(long, value_name = "N")]
        if_revision: Option<u64>,
    },
    GetDocument {
        document_id: String,
    },
    GetThema {
        thema_id: String,
    },
    DueThemata {
        day: u32,
        #[arg(long, default_value_t = 20)]
        limit: u16,
    },
    DuePage {
        day: u32,
        #[arg(long, default_value_t = 100)]
        limit: u16,
        #[arg(long)]
        after: Option<String>,
    },
    ReviewAttemptState {
        attempt_id: String,
    },
    ReviewAttemptSetAccepted {
        attempt_id: String,
        #[arg(long)]
        operation_id: String,
        #[arg(long)]
        expected_revision: u64,
        #[arg(long, action = clap::ArgAction::Set)]
        accepted_success: bool,
    },
    ReviewAttemptSetSuspended {
        attempt_id: String,
        #[arg(long)]
        operation_id: String,
        #[arg(long)]
        expected_revision: u64,
        #[arg(long, action = clap::ArgAction::Set)]
        desired_suspended: bool,
    },
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
    #[error("could not read Org document from standard input")]
    ReadInput(#[source] io::Error),
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error("could not encode daemon response")]
    Encode(#[source] serde_json::Error),
    #[error("could not write daemon response")]
    WriteOutput(#[source] io::Error),
    #[error("backup path is not valid UTF-8")]
    Path,
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
            eprintln!("{error}");
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
        Command::LegacyInfo => Ok(Request::LegacyInfo {}),
        Command::ImportOrg {
            document_id,
            if_revision,
        } => Ok(Request::DocumentImport {
            document_id,
            org: read_org()?,
            expected_revision: if_revision,
        }),
        Command::GetDocument { document_id } => Ok(Request::DocumentGet { document_id }),
        Command::GetThema { thema_id } => Ok(Request::ThemaGet { thema_id }),
        Command::DueThemata { day, limit } => Ok(Request::ReviewDue { day, limit }),
        Command::DuePage { day, limit, after } => Ok(Request::ReviewDuePage { day, limit, after }),
        Command::ReviewAttemptState { attempt_id } => {
            Ok(Request::ReviewAttemptStateV2 { attempt_id })
        }
        Command::ReviewAttemptSetAccepted {
            attempt_id,
            operation_id,
            expected_revision,
            accepted_success,
        } => Ok(Request::ReviewAttemptAcceptanceSetV2 {
            operation_version: 1,
            operation_id,
            attempt_id,
            expected_attempt_revision: expected_revision,
            accepted_success,
        }),
        Command::ReviewAttemptSetSuspended {
            attempt_id,
            operation_id,
            expected_revision,
            desired_suspended,
        } => Ok(Request::ReviewAttemptSuspensionSetV2 {
            operation_version: 1,
            operation_id,
            attempt_id,
            expected_attempt_revision: expected_revision,
            desired_suspended,
        }),
        Command::DeviceCreate { name, scopes } => Ok(Request::DeviceCreate { name, scopes }),
        Command::DeviceList => Ok(Request::DeviceList {}),
        Command::DeviceRevoke { device_id } => Ok(Request::DeviceRevoke { device_id }),
        Command::Admin { command } => admin_request(command),
    }
}

fn correlation_id() -> String {
    format!("gnosis-cli-{}", Uuid::now_v7())
}

fn admin_request(command: AdminCommand) -> Result<Request, CliError> {
    match command {
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

fn read_org() -> Result<String, CliError> {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .map_err(CliError::ReadInput)?;
    Ok(input)
}

fn write_result(result: &ResponseResult) -> Result<(), CliError> {
    match result {
        ResponseResult::SystemInfo(value) => write_json(value),
        ResponseResult::DocumentImported(value) => write_json(value),
        ResponseResult::Document(value) => write_json(value),
        ResponseResult::LegacyInfo(value) => write_json(value),
        ResponseResult::Thema(value) => write_json(value),
        ResponseResult::DueThemata(value) => write_json(value),
        ResponseResult::DuePage(value) => write_json(value),
        ResponseResult::ReviewSession(value) => write_json(value),
        ResponseResult::ActiveReviewSession(value) => write_json(value),
        ResponseResult::ReviewAttempt(value) => write_json(value),
        ResponseResult::ReviewAttemptStateV2(value) => write_json(value),
        ResponseResult::ReviewAttemptMutationV2(value) => write_json(value),
        ResponseResult::ReviewEvent(value) => write_json(value),
        ResponseResult::AssetChunk(value) => write_json(value),
        ResponseResult::DeviceCreated(value) => write_json(value),
        ResponseResult::Devices(value) => write_json(value),
        ResponseResult::Device(value) => write_json(value),
        ResponseResult::Backup(value) => write_json(value),
        ResponseResult::BackupsPruned(value) => write_json(value),
    }
}

fn write_json(value: &impl Serialize) -> Result<(), CliError> {
    let mut output = io::stdout().lock();
    serde_json::to_writer(&mut output, value).map_err(CliError::Encode)?;
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

#[derive(Debug, Error)]
enum RunError {
    #[error(transparent)]
    Client(#[from] CliError),
    #[error("protocol request failed")]
    Protocol(ProtocolError),
}

#[cfg(test)]
mod tests {
    use clap::Parser as _;

    use super::{Cli, command_request, correlation_id};
    use gnosis_core::protocol::Request;

    #[test]
    fn review_mutation_command_requires_durable_operation_identity() {
        let cli = Cli::try_parse_from([
            "gnosis",
            "--socket",
            "/tmp/gnosis.sock",
            "review-attempt-set-accepted",
            "attempt",
            "--operation-id",
            "operation",
            "--expected-revision",
            "7",
            "--accepted-success",
            "false",
        ])
        .unwrap();

        let request = command_request(cli.command).unwrap();
        assert!(matches!(
            request,
            Request::ReviewAttemptAcceptanceSetV2 {
                operation_version: 1,
                operation_id,
                attempt_id,
                expected_attempt_revision: 7,
                accepted_success: false,
            } if operation_id == "operation" && attempt_id == "attempt"
        ));
    }

    #[test]
    fn correlation_ids_are_fresh_and_not_operation_ids() {
        let first = correlation_id();
        let second = correlation_id();

        assert_ne!(first, second);
        assert!(first.starts_with("gnosis-cli-"));
        assert!(first.len() <= gnosis_core::protocol::MAX_REQUEST_ID_LEN);
    }
}
