use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::ExitCode;

use chrono::Utc;
use clap::{Parser, Subcommand};
use gnosis_core::{
    client_http::{DeliveryStatus, HttpClient, HttpClientError, SecretToken},
    client_replica::{Error as LocalReplicaError, LocalReplica},
    client_sync::{ReplicaSync, ReplicaSyncError},
    protocol::{
        ClientError, ErrorCode, PROTOCOL_VERSION, ProtocolError, Request, RequestEnvelope,
        ResponseOutcome, ResponseResult,
        http_v1::{SetAcceptedSuccessRequestV1, SetSuspensionRequestV1},
        request,
    },
};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

const TOKEN_FILE_READ_LIMIT: u64 = 131;

#[derive(Debug, Parser)]
#[command(name = "gnosis")]
struct Cli {
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
    #[arg(long, default_value = "http://127.0.0.1:7842", value_name = "URL")]
    http_url: String,
    #[arg(long, value_name = "PATH")]
    token_file: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Sync {
        #[arg(long, value_name = "PATH")]
        replica: PathBuf,
    },
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
    #[error(transparent)]
    Http(#[from] HttpClientError),
    #[error(transparent)]
    Replica(#[from] LocalReplicaError),
    #[error(transparent)]
    Sync(Box<ReplicaSyncError>),
    #[error("--socket is required for this local operator or compatibility command")]
    MissingSocket,
    #[error("--token-file is required for this HTTP command")]
    MissingTokenFile,
    #[error("could not read Gnosis device token file")]
    ReadToken(#[source] io::Error),
    #[error("Gnosis device token file must not be accessible by group or others")]
    TokenPermissions,
    #[error("Gnosis device token file is invalid")]
    TokenFile,
    #[error(
        "review operation {operation_id} failed ({delivery} delivery); if retried, use the same operation ID: {source}"
    )]
    ReviewOperation {
        operation_id: String,
        delivery: &'static str,
        #[source]
        source: Box<CliError>,
    },
    #[error("could not encode daemon response")]
    Encode(#[source] serde_json::Error),
    #[error("could not write daemon response")]
    WriteOutput(#[source] io::Error),
    #[error("backup path is not valid UTF-8")]
    Path,
}

impl From<ReplicaSyncError> for CliError {
    fn from(error: ReplicaSyncError) -> Self {
        Self::Sync(Box::new(error))
    }
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
            print_client_error(&error);
            ExitCode::FAILURE
        }
    }
}

fn print_client_error(error: &CliError) {
    match error {
        CliError::Http(HttpClientError::Server(server)) => {
            let body = &server.body().error;
            eprintln!("{}: {}", body.code().as_str(), body.message());
        }
        CliError::ReviewOperation { source, .. } => {
            if let CliError::Http(HttpClientError::Server(server)) = source.as_ref() {
                let body = &server.body().error;
                eprintln!("{error}: {}: {}", body.code().as_str(), body.message());
            } else {
                eprintln!("{error}");
            }
        }
        _ => eprintln!("{error}"),
    }
}

async fn run(cli: Cli) -> Result<(), RunError> {
    match cli.command {
        Command::Sync { replica } => run_http_sync(&cli.http_url, cli.token_file, replica).await,
        Command::ReviewAttemptState { attempt_id } => {
            run_http_state(&cli.http_url, cli.token_file, attempt_id).await
        }
        Command::ReviewAttemptSetAccepted {
            attempt_id,
            operation_id,
            expected_revision,
            accepted_success,
        } => {
            run_http_accepted(
                &cli.http_url,
                cli.token_file,
                attempt_id,
                operation_id,
                expected_revision,
                accepted_success,
            )
            .await
        }
        Command::ReviewAttemptSetSuspended {
            attempt_id,
            operation_id,
            expected_revision,
            desired_suspended,
        } => {
            run_http_suspended(
                &cli.http_url,
                cli.token_file,
                attempt_id,
                operation_id,
                expected_revision,
                desired_suspended,
            )
            .await
        }
        command => run_socket(cli.socket, command).await,
    }
}

async fn run_http_sync(
    base_url: &str,
    token_file: Option<PathBuf>,
    replica_path: PathBuf,
) -> Result<(), RunError> {
    let client = http_client(base_url, token_file)?;
    let identity = client.device_identity().await.map_err(CliError::from)?;
    let mut replica =
        LocalReplica::open(replica_path, &identity.device_id).map_err(CliError::from)?;
    let report = ReplicaSync::new(&client)
        .synchronize(&mut replica)
        .await
        .map_err(CliError::from)?;
    write_json(&report).map_err(Into::into)
}

async fn run_http_state(
    base_url: &str,
    token_file: Option<PathBuf>,
    attempt_id: String,
) -> Result<(), RunError> {
    let client = http_client(base_url, token_file)?;
    let state = client
        .review_attempt_state(&attempt_id)
        .await
        .map_err(CliError::from)?;
    write_json(&state).map_err(Into::into)
}

async fn run_http_accepted(
    base_url: &str,
    token_file: Option<PathBuf>,
    attempt_id: String,
    operation_id: String,
    expected_revision: u64,
    accepted_success: bool,
) -> Result<(), RunError> {
    let client = http_client(base_url, token_file).map_err(|source| {
        review_operation_error(operation_id.clone(), DeliveryStatus::NotSent, source)
    })?;
    let input = SetAcceptedSuccessRequestV1 {
        version: 1,
        operation_id: operation_id.clone(),
        expected_attempt_revision: expected_revision,
        accepted_success,
    };
    let result = client
        .set_review_accepted_success(&attempt_id, &input)
        .await
        .map_err(|source| review_http_operation_error(operation_id.clone(), source))?;
    write_json(&result).map_err(|source| {
        review_operation_error(operation_id, DeliveryStatus::Confirmed, source)
    })?;
    Ok(())
}

async fn run_http_suspended(
    base_url: &str,
    token_file: Option<PathBuf>,
    attempt_id: String,
    operation_id: String,
    expected_revision: u64,
    desired_suspended: bool,
) -> Result<(), RunError> {
    let client = http_client(base_url, token_file).map_err(|source| {
        review_operation_error(operation_id.clone(), DeliveryStatus::NotSent, source)
    })?;
    let input = SetSuspensionRequestV1 {
        version: 1,
        operation_id: operation_id.clone(),
        expected_attempt_revision: expected_revision,
        desired_suspended,
    };
    let result = client
        .set_review_suspension(&attempt_id, &input)
        .await
        .map_err(|source| review_http_operation_error(operation_id.clone(), source))?;
    write_json(&result).map_err(|source| {
        review_operation_error(operation_id, DeliveryStatus::Confirmed, source)
    })?;
    Ok(())
}

async fn run_socket(socket: Option<PathBuf>, command: Command) -> Result<(), RunError> {
    let envelope = RequestEnvelope {
        version: PROTOCOL_VERSION,
        id: correlation_id(),
        request: command_request(command)?,
    };
    let response = request(socket.ok_or(CliError::MissingSocket)?, &envelope)
        .await
        .map_err(CliError::from)?;
    match response.outcome {
        ResponseOutcome::Success { result } => write_result(&result).map_err(Into::into),
        ResponseOutcome::Failure { error } => Err(RunError::Protocol(error)),
    }
}

fn http_client(base_url: &str, token_file: Option<PathBuf>) -> Result<HttpClient, CliError> {
    let path = token_file.ok_or(CliError::MissingTokenFile)?;
    HttpClient::new(base_url, read_device_token(path)?).map_err(Into::into)
}

fn read_device_token(path: PathBuf) -> Result<SecretToken, CliError> {
    let file = File::open(path).map_err(CliError::ReadToken)?;
    let mode = file
        .metadata()
        .map_err(CliError::ReadToken)?
        .permissions()
        .mode();
    if mode & 0o077 != 0 {
        return Err(CliError::TokenPermissions);
    }
    let mut bytes = Vec::new();
    file.take(TOKEN_FILE_READ_LIMIT)
        .read_to_end(&mut bytes)
        .map_err(CliError::ReadToken)?;
    if bytes.len() == TOKEN_FILE_READ_LIMIT as usize {
        return Err(CliError::TokenFile);
    }
    let value = String::from_utf8(bytes).map_err(|_| CliError::TokenFile)?;
    let value = value.strip_suffix('\n').unwrap_or(&value);
    let value = value.strip_suffix('\r').unwrap_or(value);
    SecretToken::new(value.to_owned()).map_err(Into::into)
}

fn review_http_operation_error(operation_id: String, source: HttpClientError) -> CliError {
    let delivery = source.delivery();
    review_operation_error(operation_id, delivery, source.into())
}

fn review_operation_error(
    operation_id: String,
    delivery: DeliveryStatus,
    source: CliError,
) -> CliError {
    CliError::ReviewOperation {
        operation_id,
        delivery: delivery_label(delivery),
        source: Box::new(source),
    }
}

const fn delivery_label(delivery: DeliveryStatus) -> &'static str {
    match delivery {
        DeliveryStatus::Confirmed => "confirmed",
        DeliveryStatus::NotSent => "not-sent",
        DeliveryStatus::Unknown => "unknown",
    }
}

fn command_request(command: Command) -> Result<Request, CliError> {
    match command {
        Command::Sync { .. } => unreachable!("HTTP command reached socket"),
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
        Command::ReviewAttemptState { .. }
        | Command::ReviewAttemptSetAccepted { .. }
        | Command::ReviewAttemptSetSuspended { .. } => unreachable!("HTTP command reached socket"),
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

#[derive(Debug, Error)]
enum RunError {
    #[error(transparent)]
    Client(#[from] CliError),
    #[error("protocol request failed")]
    Protocol(ProtocolError),
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt as _};

    use clap::Parser as _;

    use super::{Cli, CliError, Command, correlation_id, read_device_token};

    #[test]
    fn http_review_mutation_requires_durable_operation_identity() {
        let cli = Cli::try_parse_from([
            "gnosis",
            "--token-file",
            "/tmp/gnosis.token",
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

        assert!(matches!(
            cli.command,
            Command::ReviewAttemptSetAccepted {
                operation_id,
                attempt_id,
                expected_revision: 7,
                accepted_success: false,
            } if operation_id == "operation" && attempt_id == "attempt"
        ));
        assert!(cli.socket.is_none());
        assert_eq!(cli.http_url, "http://127.0.0.1:7842");
    }

    #[test]
    fn correlation_ids_are_fresh_and_not_operation_ids() {
        let first = correlation_id();
        let second = correlation_id();

        assert_ne!(first, second);
        assert!(first.starts_with("gnosis-cli-"));
        assert!(first.len() <= gnosis_core::protocol::MAX_REQUEST_ID_LEN);
    }

    #[test]
    fn token_file_rejects_group_or_other_access() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("device.token");
        fs::write(&path, format!("gnosis_{}\n", "a".repeat(43))).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();

        assert!(matches!(
            read_device_token(path),
            Err(CliError::TokenPermissions)
        ));
    }
}
