use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::process::ExitCode;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::Utc;
use clap::{Parser, Subcommand};
use gnosis_core::{
    client_http::{DeliveryStatus, HttpClient, HttpClientError, SecretToken},
    client_replica::{
        Error as LocalReplicaError, LocalReplica, OfflineAnswer, OfflineResponse,
        OfflineReviewOutcome, ReconciledOfflineReview, ReviewConflictReason,
    },
    client_review::{
        ReviewPackAcquisition, ReviewPackAcquisitionError, active_question, offline_asset,
    },
    client_sync::{ReplicaSync, ReplicaSyncError},
    protocol::{
        ClientError, ErrorCode, PROTOCOL_VERSION, ProtocolError, Request, RequestEnvelope,
        ResponseOutcome, ResponseResult,
        client_v1::{
            ClientDeliveryV1, ClientFailureCodeV1, ClientFailureV1, OfflineReviewAnswerReceiptV1,
            OfflineReviewAnswerRequestV1, OfflineReviewAssetRequestV1, OfflineReviewAssetV1,
            OfflineReviewConflictReasonV1, OfflineReviewResultPageRequestV1,
            OfflineReviewResultPageV1, OfflineReviewResultV1, ReviewPackAcquireRequestV1,
            ReviewPackCurrentV1, ReviewPackReceiptV1, valid_client_operation_id,
        },
        http_v1::{
            DocumentImportRequestV1, OfflineAnswerV1, SetAcceptedSuccessRequestV1,
            SetSuspensionRequestV1,
        },
        request,
    },
};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use uuid::Uuid;

const TOKEN_FILE_READ_LIMIT: u64 = 131;
const CLIENT_INPUT_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Parser)]
#[command(name = "gnosis")]
struct Cli {
    #[arg(long, value_name = "PATH")]
    socket: Option<PathBuf>,
    #[arg(long, default_value = "http://127.0.0.1:7842", value_name = "URL")]
    http_url: String,
    #[arg(long, value_name = "PATH")]
    token_file: Option<PathBuf>,
    #[arg(long)]
    json_errors: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Sync {
        #[arg(long, value_name = "PATH")]
        replica: PathBuf,
    },
    ReviewPack {
        #[command(subcommand)]
        command: ReviewPackCommand,
    },
    SystemInfo,
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
enum ReviewPackCommand {
    Acquire {
        #[arg(long, value_name = "PATH")]
        replica: PathBuf,
    },
    Resume {
        #[arg(long, value_name = "PATH")]
        replica: PathBuf,
    },
    Current {
        #[arg(long, value_name = "PATH")]
        replica: PathBuf,
    },
    Answer {
        #[arg(long, value_name = "PATH")]
        replica: PathBuf,
    },
    Abandon {
        #[arg(long, value_name = "PATH")]
        replica: PathBuf,
    },
    Asset {
        #[arg(long, value_name = "PATH")]
        replica: PathBuf,
    },
    Results {
        #[arg(long, value_name = "PATH")]
        replica: PathBuf,
    },
}

impl ReviewPackCommand {
    #[cfg(test)]
    const fn name(&self) -> &'static str {
        match self {
            Self::Acquire { .. } => "acquire",
            Self::Resume { .. } => "resume",
            Self::Current { .. } => "current",
            Self::Answer { .. } => "answer",
            Self::Abandon { .. } => "abandon",
            Self::Asset { .. } => "asset",
            Self::Results { .. } => "results",
        }
    }
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
    #[error("could not read client request from standard input")]
    ReadClientInput(#[source] io::Error),
    #[error("client request exceeds the input limit")]
    ClientInputTooLarge,
    #[error("client request is not valid JSON")]
    DecodeClientInput(#[source] serde_json::Error),
    #[error("client request values are invalid")]
    InvalidClientInput,
    #[error(transparent)]
    Client(#[from] ClientError),
    #[error(transparent)]
    Http(#[from] HttpClientError),
    #[error(transparent)]
    Replica(#[from] LocalReplicaError),
    #[error(transparent)]
    Sync(Box<ReplicaSyncError>),
    #[error(transparent)]
    ReviewPack(Box<ReviewPackAcquisitionError>),
    #[error("--socket is required for this local operator command")]
    MissingSocket,
    #[error("--socket is only valid for local operator commands")]
    SocketReservedForOperator,
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
        delivery: DeliveryStatus,
        #[source]
        source: Box<CliError>,
    },
    #[error("client result failed ({delivery} delivery): {source}")]
    ClientResult {
        delivery: DeliveryStatus,
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

impl From<ReviewPackAcquisitionError> for CliError {
    fn from(error: ReviewPackAcquisitionError) -> Self {
        Self::ReviewPack(Box::new(error))
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    let json_errors = cli.json_errors;
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(RunError::Protocol(error)) => {
            if json_errors {
                write_failure(&protocol_failure());
            } else {
                eprintln!("{}: {}", error_code(error.code), error.message);
            }
            ExitCode::FAILURE
        }
        Err(RunError::Client(error)) => {
            if json_errors {
                write_failure(&client_failure(&error));
            } else {
                print_client_error(&error);
            }
            ExitCode::FAILURE
        }
    }
}

fn write_failure(failure: &ClientFailureV1) {
    let mut output = io::stderr().lock();
    if serde_json::to_writer(&mut output, failure).is_ok() {
        let _ = writeln!(output);
    }
}

fn protocol_failure() -> ClientFailureV1 {
    failure(
        ClientFailureCodeV1::Protocol,
        ClientDeliveryV1::Confirmed,
        None,
        None,
    )
}

fn client_failure(error: &CliError) -> ClientFailureV1 {
    match error {
        CliError::Http(source) => http_failure(source, None),
        CliError::ReviewOperation {
            operation_id,
            delivery,
            source,
        } => {
            let mut failure = client_failure(source);
            failure.operation_id =
                valid_client_operation_id(operation_id).then(|| operation_id.clone());
            failure.delivery = client_delivery(*delivery);
            failure
        }
        CliError::ClientResult { delivery, source } => {
            let mut failure = client_failure(source);
            failure.delivery = client_delivery(*delivery);
            failure
        }
        CliError::Sync(source) => sync_failure(source),
        CliError::ReviewPack(source) => review_pack_failure(source),
        CliError::Replica(_) => local_failure(ClientFailureCodeV1::Replica),
        CliError::MissingTokenFile
        | CliError::ReadToken(_)
        | CliError::TokenPermissions
        | CliError::TokenFile => local_failure(ClientFailureCodeV1::Credentials),
        CliError::ReadInput(_)
        | CliError::ReadClientInput(_)
        | CliError::ClientInputTooLarge
        | CliError::DecodeClientInput(_)
        | CliError::InvalidClientInput => local_failure(ClientFailureCodeV1::Input),
        CliError::Client(_) | CliError::MissingSocket | CliError::SocketReservedForOperator => {
            local_failure(ClientFailureCodeV1::Protocol)
        }
        CliError::Encode(_) | CliError::WriteOutput(_) => {
            local_failure(ClientFailureCodeV1::Output)
        }
        CliError::Path => local_failure(ClientFailureCodeV1::Path),
    }
}

fn sync_failure(error: &ReplicaSyncError) -> ClientFailureV1 {
    match error {
        ReplicaSyncError::Http(source) => http_failure(source, None),
        ReplicaSyncError::StageAsset {
            operation, source, ..
        }
        | ReplicaSyncError::Upload {
            operation, source, ..
        } => http_failure(source, Some(operation.operation_id.clone())),
        ReplicaSyncError::RecordResult { operation, .. } => failure(
            ClientFailureCodeV1::Replica,
            ClientDeliveryV1::Confirmed,
            Some(operation.operation_id.clone()),
            None,
        ),
        ReplicaSyncError::Replica(_)
        | ReplicaSyncError::DeviceIdentity { .. }
        | ReplicaSyncError::MissingCursor => local_failure(ClientFailureCodeV1::Synchronization),
    }
}

fn review_pack_failure(error: &ReviewPackAcquisitionError) -> ClientFailureV1 {
    match error {
        ReviewPackAcquisitionError::Http(source) => http_failure(source, None),
        ReviewPackAcquisitionError::Replica(_) => local_failure(ClientFailureCodeV1::Replica),
        _ => local_failure(ClientFailureCodeV1::ReviewPack),
    }
}

fn http_failure(error: &HttpClientError, operation_id: Option<String>) -> ClientFailureV1 {
    let code = match error {
        HttpClientError::Server(_) => ClientFailureCodeV1::Server,
        HttpClientError::InvalidToken => ClientFailureCodeV1::Credentials,
        HttpClientError::InvalidBaseUrl
        | HttpClientError::InvalidCursor
        | HttpClientError::InvalidDigest
        | HttpClientError::InvalidUpload
        | HttpClientError::InvalidOperationId
        | HttpClientError::InvalidMutation
        | HttpClientError::InvalidReview => ClientFailureCodeV1::Input,
        _ => ClientFailureCodeV1::Http,
    };
    let api_error = match error {
        HttpClientError::Server(server) => Some(server.body().clone()),
        _ => None,
    };
    failure(
        code,
        client_delivery(error.delivery()),
        operation_id,
        api_error,
    )
}

fn local_failure(code: ClientFailureCodeV1) -> ClientFailureV1 {
    failure(code, ClientDeliveryV1::NotSent, None, None)
}

fn failure(
    code: ClientFailureCodeV1,
    delivery: ClientDeliveryV1,
    operation_id: Option<String>,
    api_error: Option<gnosis_core::protocol::http_v1::ApiErrorResponseV1>,
) -> ClientFailureV1 {
    ClientFailureV1 {
        version: 1,
        code,
        delivery,
        operation_id,
        api_error,
    }
}

const fn client_delivery(value: DeliveryStatus) -> ClientDeliveryV1 {
    match value {
        DeliveryStatus::NotSent => ClientDeliveryV1::NotSent,
        DeliveryStatus::Confirmed => ClientDeliveryV1::Confirmed,
        DeliveryStatus::Unknown => ClientDeliveryV1::Unknown,
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
    if cli.socket.is_some() && !cli.command.is_operator() {
        return Err(CliError::SocketReservedForOperator.into());
    }
    match cli.command {
        Command::Sync { replica } => run_http_sync(&cli.http_url, cli.token_file, replica).await,
        Command::ReviewPack { command } => {
            run_review_pack(&cli.http_url, cli.token_file, command).await
        }
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
        Command::ImportOrg {
            document_id,
            if_revision,
        } => run_http_import(&cli.http_url, cli.token_file, document_id, if_revision).await,
        Command::GetDocument { document_id } => {
            let client = http_client(&cli.http_url, cli.token_file)?;
            write_json(
                &client
                    .document(&document_id)
                    .await
                    .map_err(CliError::from)?,
            )
            .map_err(Into::into)
        }
        Command::GetThema { thema_id } => {
            let client = http_client(&cli.http_url, cli.token_file)?;
            write_json(
                &client
                    .legacy_thema(&thema_id)
                    .await
                    .map_err(CliError::from)?,
            )
            .map_err(Into::into)
        }
        Command::DueThemata { day, limit } => {
            let client = http_client(&cli.http_url, cli.token_file)?;
            let page = client
                .legacy_due_page(day, limit, None)
                .await
                .map_err(CliError::from)?;
            write_json(&page.items).map_err(Into::into)
        }
        Command::DuePage { day, limit, after } => {
            let client = http_client(&cli.http_url, cli.token_file)?;
            let page = client
                .legacy_due_page(day, limit, after.as_deref())
                .await
                .map_err(CliError::from)?;
            write_json(&page).map_err(Into::into)
        }
        command @ (Command::SystemInfo
        | Command::DeviceCreate { .. }
        | Command::DeviceList
        | Command::DeviceRevoke { .. }
        | Command::Admin { .. }) => run_socket(cli.socket, command).await,
    }
}

impl Command {
    const fn is_operator(&self) -> bool {
        matches!(
            self,
            Self::SystemInfo
                | Self::DeviceCreate { .. }
                | Self::DeviceList
                | Self::DeviceRevoke { .. }
                | Self::Admin { .. }
        )
    }
}

async fn run_http_import(
    base_url: &str,
    token_file: Option<PathBuf>,
    document_id: String,
    expected_revision: Option<u64>,
) -> Result<(), RunError> {
    let client = http_client(base_url, token_file)?;
    let input = DocumentImportRequestV1 {
        org: read_org()?,
        expected_revision,
    };
    let result = client
        .import_document(&document_id, &input)
        .await
        .map_err(CliError::from)?;
    write_json(&result).map_err(Into::into)
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
    write_json(&report)
        .map_err(|source| client_result_error(DeliveryStatus::Confirmed, source).into())
}

async fn run_review_pack(
    base_url: &str,
    token_file: Option<PathBuf>,
    command: ReviewPackCommand,
) -> Result<(), RunError> {
    match command {
        ReviewPackCommand::Acquire { replica } => {
            run_review_pack_acquire(base_url, token_file, replica).await
        }
        ReviewPackCommand::Resume { replica } => {
            run_review_pack_resume(base_url, token_file, replica).await
        }
        ReviewPackCommand::Current { replica } => run_review_pack_current(replica),
        ReviewPackCommand::Answer { replica } => run_review_pack_answer(replica),
        ReviewPackCommand::Abandon { replica } => {
            run_review_pack_abandon(base_url, token_file, replica).await
        }
        ReviewPackCommand::Asset { replica } => run_review_pack_asset(replica),
        ReviewPackCommand::Results { replica } => run_review_pack_results(replica),
    }
}

async fn run_review_pack_acquire(
    base_url: &str,
    token_file: Option<PathBuf>,
    replica_path: PathBuf,
) -> Result<(), RunError> {
    let input: ReviewPackAcquireRequestV1 = read_client_input()?;
    let (client, mut replica) = bound_replica(base_url, token_file, replica_path).await?;
    ReviewPackAcquisition::new(&client)
        .create(&mut replica, &input.session)
        .await
        .map_err(CliError::from)?;
    write_current(&replica)
        .map_err(|source| client_result_error(DeliveryStatus::Confirmed, source).into())
}

async fn run_review_pack_resume(
    base_url: &str,
    token_file: Option<PathBuf>,
    replica_path: PathBuf,
) -> Result<(), RunError> {
    let (client, mut replica) = bound_replica(base_url, token_file, replica_path).await?;
    ReviewPackAcquisition::new(&client)
        .resume_active(&mut replica)
        .await
        .map_err(CliError::from)?
        .ok_or(ReviewPackAcquisitionError::NoActivePack)
        .map_err(CliError::from)?;
    write_current(&replica)
        .map_err(|source| client_result_error(DeliveryStatus::Confirmed, source).into())
}

fn run_review_pack_current(replica_path: PathBuf) -> Result<(), RunError> {
    let replica = LocalReplica::open_existing(replica_path).map_err(CliError::from)?;
    write_current(&replica).map_err(Into::into)
}

fn run_review_pack_answer(replica_path: PathBuf) -> Result<(), RunError> {
    let input: OfflineReviewAnswerRequestV1 = read_client_input()?;
    let mut replica = LocalReplica::open_existing(replica_path).map_err(CliError::from)?;
    let (pack_id, answer) = offline_answer(input);
    let recorded = replica.answer(&pack_id, answer).map_err(CliError::from)?;
    let attempt_id = recorded.attempt_id.clone();
    let question = active_question(&replica)
        .map_err(CliError::from)
        .map_err(|source| {
            review_operation_error(attempt_id.clone(), DeliveryStatus::Confirmed, source)
        })?;
    let receipt = OfflineReviewAnswerReceiptV1 {
        version: 1,
        attempt_id: recorded.attempt_id,
        device_sequence: recorded.device_sequence,
        question,
    };
    write_json(&receipt).map_err(|source| {
        review_operation_error(attempt_id, DeliveryStatus::Confirmed, source).into()
    })
}

async fn run_review_pack_abandon(
    base_url: &str,
    token_file: Option<PathBuf>,
    replica_path: PathBuf,
) -> Result<(), RunError> {
    let (client, mut replica) = bound_replica(base_url, token_file, replica_path).await?;
    let acquisition = ReviewPackAcquisition::new(&client);
    let pack_id = acquisition
        .abandon_active(&mut replica)
        .await
        .map_err(CliError::from)?;
    write_json(&ReviewPackReceiptV1 {
        version: 1,
        pack_id,
    })
    .map_err(|source| client_result_error(DeliveryStatus::Confirmed, source).into())
}

fn run_review_pack_asset(replica_path: PathBuf) -> Result<(), RunError> {
    let input: OfflineReviewAssetRequestV1 = read_client_input()?;
    let replica = LocalReplica::open_existing(replica_path).map_err(CliError::from)?;
    let asset =
        offline_asset(&replica, &input.pack_id, &input.logical_path).map_err(CliError::from)?;
    write_json(&OfflineReviewAssetV1 {
        version: 1,
        pack_id: asset.pack_id,
        logical_path: asset.logical_path,
        content_sha256: asset.content_sha256,
        size_bytes: asset.size_bytes,
        media_type: asset.media_type,
        data_base64: BASE64.encode(asset.bytes),
    })
    .map_err(Into::into)
}

fn run_review_pack_results(replica_path: PathBuf) -> Result<(), RunError> {
    let input: OfflineReviewResultPageRequestV1 = read_client_input()?;
    if !(1..=100).contains(&input.limit) || input.after.is_some_and(|value| value > i64::MAX as u64)
    {
        return Err(CliError::InvalidClientInput.into());
    }
    let replica = LocalReplica::open_existing(replica_path).map_err(CliError::from)?;
    let page = if input.conflicts_only {
        replica.conflicts(input.after, input.limit)
    } else {
        replica.review_results(input.after, input.limit)
    }
    .map_err(CliError::from)?;
    let items = page
        .items
        .into_iter()
        .map(review_result)
        .collect::<Result<Vec<_>, _>>()?;
    write_json(&OfflineReviewResultPageV1 {
        version: 1,
        items,
        next_after: page.next_after,
    })
    .map_err(Into::into)
}

fn review_result(value: ReconciledOfflineReview) -> Result<OfflineReviewResultV1, CliError> {
    let common = (
        value.result_sequence,
        value.server_id,
        value.attempt_id,
        value.device_sequence,
    );
    match value.outcome {
        OfflineReviewOutcome::Accepted => accepted_review_result(common, value.event_id),
        OfflineReviewOutcome::Conflict => conflict_review_result(common, value.conflict_reason),
    }
}

fn accepted_review_result(
    common: (u64, String, String, u64),
    event_id: Option<String>,
) -> Result<OfflineReviewResultV1, CliError> {
    let (result_sequence, server_id, attempt_id, device_sequence) = common;
    Ok(OfflineReviewResultV1::Accepted {
        result_sequence,
        server_id,
        attempt_id,
        device_sequence,
        event_id: event_id.ok_or(LocalReplicaError::Response)?,
    })
}

fn conflict_review_result(
    common: (u64, String, String, u64),
    reason: Option<ReviewConflictReason>,
) -> Result<OfflineReviewResultV1, CliError> {
    let (result_sequence, server_id, attempt_id, device_sequence) = common;
    Ok(OfflineReviewResultV1::Conflict {
        result_sequence,
        server_id,
        attempt_id,
        device_sequence,
        conflict_reason: conflict_reason(reason.ok_or(LocalReplicaError::Response)?),
    })
}

const fn conflict_reason(reason: ReviewConflictReason) -> OfflineReviewConflictReasonV1 {
    match reason {
        ReviewConflictReason::LeaseEnded => OfflineReviewConflictReasonV1::LeaseEnded,
        ReviewConflictReason::SessionAdvanced => OfflineReviewConflictReasonV1::SessionAdvanced,
        ReviewConflictReason::ContentChanged => OfflineReviewConflictReasonV1::ContentChanged,
        ReviewConflictReason::SchedulingChanged => OfflineReviewConflictReasonV1::SchedulingChanged,
        ReviewConflictReason::CaptureMismatch => OfflineReviewConflictReasonV1::CaptureMismatch,
    }
}

async fn bound_replica(
    base_url: &str,
    token_file: Option<PathBuf>,
    replica_path: PathBuf,
) -> Result<(HttpClient, LocalReplica), CliError> {
    let client = http_client(base_url, token_file)?;
    let identity = client.device_identity().await?;
    let replica = LocalReplica::open(replica_path, &identity.device_id)?;
    ReviewPackAcquisition::new(&client)
        .verify_binding(&replica)
        .await?;
    Ok((client, replica))
}

fn write_current(replica: &LocalReplica) -> Result<(), CliError> {
    write_json(&ReviewPackCurrentV1 {
        version: 1,
        question: active_question(replica)?,
    })
}

fn offline_answer(input: OfflineReviewAnswerRequestV1) -> (String, OfflineAnswer) {
    (
        input.pack_id,
        OfflineAnswer {
            attempt_id: input.attempt_id,
            session_position: input.session_position,
            response: offline_response(input.response),
            accepted_success: input.accepted_success,
            desired_suspended: input.desired_suspended,
            answered_at_us: input.answered_at_us,
        },
    )
}

fn offline_response(response: OfflineAnswerV1) -> OfflineResponse {
    match response {
        OfflineAnswerV1::Text(value) => OfflineResponse::Text(value),
        OfflineAnswerV1::Texts(values) => OfflineResponse::Texts(values),
        OfflineAnswerV1::Choice(value) => OfflineResponse::Choice(value),
    }
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
    write_json(&state)
        .map_err(|source| client_result_error(DeliveryStatus::Confirmed, source).into())
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
        ResponseOutcome::Success { result } => write_result(&result)
            .map_err(|source| client_result_error(DeliveryStatus::Confirmed, source).into()),
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
        delivery,
        source: Box::new(source),
    }
}

fn client_result_error(delivery: DeliveryStatus, source: CliError) -> CliError {
    CliError::ClientResult {
        delivery,
        source: Box::new(source),
    }
}

fn command_request(command: Command) -> Result<Request, CliError> {
    match command {
        Command::Sync { .. } => unreachable!("HTTP command reached socket"),
        Command::ReviewPack { .. } => unreachable!("HTTP command reached socket"),
        Command::SystemInfo => Ok(Request::SystemInfo {}),
        Command::ImportOrg { .. }
        | Command::GetDocument { .. }
        | Command::GetThema { .. }
        | Command::DueThemata { .. }
        | Command::DuePage { .. }
        | Command::ReviewAttemptState { .. }
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

fn read_client_input<T>() -> Result<T, CliError>
where
    T: DeserializeOwned,
{
    let mut input = Vec::new();
    io::stdin()
        .take(CLIENT_INPUT_LIMIT + 1)
        .read_to_end(&mut input)
        .map_err(CliError::ReadClientInput)?;
    if input.len() > CLIENT_INPUT_LIMIT as usize {
        return Err(CliError::ClientInputTooLarge);
    }
    serde_json::from_slice(&input).map_err(CliError::DecodeClientInput)
}

fn write_result(result: &ResponseResult) -> Result<(), CliError> {
    match result {
        ResponseResult::SystemInfo(value) => write_json(value),
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

    #[test]
    fn review_pack_commands_have_explicit_online_and_offline_shapes() {
        for subcommand in [
            "acquire", "resume", "current", "answer", "abandon", "asset", "results",
        ] {
            let cli = Cli::try_parse_from([
                "gnosis",
                "review-pack",
                subcommand,
                "--replica",
                "/tmp/replica.sqlite",
            ])
            .unwrap();
            let Command::ReviewPack { command } = cli.command else {
                panic!("review-pack command was not parsed");
            };
            assert_eq!(command.name(), subcommand);
            assert!(cli.token_file.is_none());
        }
    }
}
