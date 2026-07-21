use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Component, Path, PathBuf};
use std::process::ExitCode;

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use chrono::Utc;
use clap::{Parser, Subcommand};
use gnosis_core::{
    client_http::{DeliveryStatus, HttpClient, HttpClientError, SecretToken, validate_note_input},
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
            AssetHeadV1, DocumentImportRequestV1, NoteWriteRequestV1, OfflineAnswerV1,
            SetAcceptedSuccessRequestV1, SetSuspensionRequestV1,
        },
        request,
    },
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use thiserror::Error;
use uuid::Uuid;

const TOKEN_FILE_READ_LIMIT: u64 = 131;
const CLIENT_INPUT_LIMIT: u64 = 1024 * 1024;
const MANIFEST_ENTRY_LIMIT: usize = 1024;
const MANIFEST_SOURCE_LIMIT: usize = 64 * 1024 * 1024;
const MAX_IMPORT_JOURNAL_BYTES: u64 = 128 * 1024 * 1024;

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
    Asset {
        #[command(subcommand)]
        command: AssetCommand,
    },
    SystemInfo,
    Import {
        document_id: String,
        #[arg(long)]
        operation_id: String,
        #[arg(long)]
        content_type: String,
        #[arg(long, value_name = "N")]
        if_revision: Option<u64>,
    },
    ImportManifest {
        manifest: PathBuf,
        #[arg(long, value_name = "PATH")]
        journal: PathBuf,
        #[arg(long)]
        dry_run: bool,
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
enum AssetCommand {
    Put {
        digest: String,
        name: String,
        #[arg(long)]
        operation_id: String,
        #[arg(long, value_name = "N")]
        if_revision: Option<u64>,
        #[arg(long)]
        media_type: String,
    },
    Fetch {
        digest: String,
        name: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        size_bytes: u64,
        #[arg(long)]
        media_type: String,
    },
    Blob {
        digest: String,
    },
    Tombstone {
        digest: String,
        name: String,
        #[arg(long)]
        operation_id: String,
        #[arg(long)]
        if_revision: u64,
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
    #[error(
        "asset operation {operation_id} failed ({delivery} delivery); if retried, use the same operation ID: {source}"
    )]
    AssetOperation {
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
    #[error("bulk import manifest or journal is invalid")]
    BulkImport,
    #[error("could not read or persist bulk import state")]
    BulkIo(#[source] io::Error),
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
        }
        | CliError::AssetOperation {
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
        | CliError::InvalidClientInput
        | CliError::BulkImport => local_failure(ClientFailureCodeV1::Input),
        CliError::Client(_) | CliError::MissingSocket | CliError::SocketReservedForOperator => {
            local_failure(ClientFailureCodeV1::Protocol)
        }
        CliError::Encode(_) | CliError::WriteOutput(_) => {
            local_failure(ClientFailureCodeV1::Output)
        }
        CliError::Path | CliError::BulkIo(_) => local_failure(ClientFailureCodeV1::Path),
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
        CliError::ReviewOperation { source, .. } | CliError::AssetOperation { source, .. } => {
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
        Command::Asset { command } => run_asset(&cli.http_url, cli.token_file, command).await,
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
        Command::Import {
            document_id,
            operation_id,
            content_type,
            if_revision,
        } => {
            run_http_import(
                &cli.http_url,
                cli.token_file,
                document_id,
                operation_id,
                content_type,
                if_revision,
            )
            .await
        }
        Command::ImportManifest {
            manifest,
            journal,
            dry_run,
        } => {
            run_http_import_manifest(&cli.http_url, cli.token_file, &manifest, &journal, dry_run)
                .await
        }
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
            write_json(&client.thema(&thema_id).await.map_err(CliError::from)?).map_err(Into::into)
        }
        Command::DueThemata { day, limit } => {
            let client = http_client(&cli.http_url, cli.token_file)?;
            let page = client
                .due_page(day, limit, None)
                .await
                .map_err(CliError::from)?;
            write_json(&page.items).map_err(Into::into)
        }
        Command::DuePage { day, limit, after } => {
            let client = http_client(&cli.http_url, cli.token_file)?;
            let page = client
                .due_page(day, limit, after.as_deref())
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

async fn run_asset(
    base_url: &str,
    token_file: Option<PathBuf>,
    command: AssetCommand,
) -> Result<(), RunError> {
    match command {
        AssetCommand::Put {
            digest,
            name,
            operation_id,
            if_revision,
            media_type,
        } => {
            let client = http_client(base_url, token_file).map_err(|source| {
                asset_operation_error(operation_id.clone(), DeliveryStatus::NotSent, source)
            })?;
            let bytes = read_client_bytes().map_err(|source| {
                asset_operation_error(operation_id.clone(), DeliveryStatus::NotSent, source)
            })?;
            let result = client
                .put_asset(
                    &digest,
                    &name,
                    &operation_id,
                    if_revision,
                    &media_type,
                    &bytes,
                )
                .await
                .map_err(|source| asset_http_operation_error(operation_id.clone(), source))?;
            write_json(&result).map_err(|source| {
                asset_operation_error(operation_id, DeliveryStatus::Confirmed, source).into()
            })
        }
        AssetCommand::Fetch {
            digest,
            name,
            revision,
            size_bytes,
            media_type,
        } => {
            let client = http_client(base_url, token_file)?;
            let head = AssetHeadV1 {
                logical_path: format!("assets/{digest}/{name}"),
                revision,
                content_sha256: Some(digest),
                media_type: Some(media_type),
                active: true,
            };
            let bytes = client
                .asset(&head, size_bytes)
                .await
                .map_err(CliError::from)?;
            write_bytes(&bytes).map_err(Into::into)
        }
        AssetCommand::Blob { digest } => {
            let client = http_client(base_url, token_file)?;
            let bytes = client.blob(&digest).await.map_err(CliError::from)?;
            write_bytes(&bytes).map_err(Into::into)
        }
        AssetCommand::Tombstone {
            digest,
            name,
            operation_id,
            if_revision,
        } => {
            let client = http_client(base_url, token_file).map_err(|source| {
                asset_operation_error(operation_id.clone(), DeliveryStatus::NotSent, source)
            })?;
            let result = client
                .tombstone_asset(&digest, &name, &operation_id, if_revision)
                .await
                .map_err(|source| asset_http_operation_error(operation_id.clone(), source))?;
            write_json(&result).map_err(|source| {
                asset_operation_error(operation_id, DeliveryStatus::Confirmed, source).into()
            })
        }
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
    operation_id: String,
    content_type: String,
    expected_revision: Option<u64>,
) -> Result<(), RunError> {
    let client = http_client(base_url, token_file)?;
    let input = DocumentImportRequestV1 {
        content_type,
        source: read_source()?,
        expected_revision,
    };
    let result = client
        .import_document(&document_id, &operation_id, &input)
        .await
        .map_err(CliError::from)?;
    write_json(&result).map_err(Into::into)
}

async fn run_http_import_manifest(
    base_url: &str,
    token_file: Option<PathBuf>,
    manifest_path: &Path,
    journal_path: &Path,
    dry_run: bool,
) -> Result<(), RunError> {
    let manifest_bytes = read_bounded_file(manifest_path)?;
    let manifest: ImportManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|_| CliError::BulkImport)?;
    let entries = preflight_manifest(manifest_path, manifest)?;
    let manifest_sha256 = import_manifest_sha256(&manifest_bytes, &entries);
    if dry_run {
        return write_json(&ImportReport {
            version: 1,
            manifest_sha256,
            entry_count: entries.len(),
            completed_count: 0,
            path_hashes: entries.into_iter().map(|entry| entry.path_hash).collect(),
        })
        .map_err(Into::into);
    }
    let mut journal = load_journal(journal_path, &manifest_sha256)?;
    let operation_ids: BTreeSet<&str> = entries
        .iter()
        .map(|entry| entry.operation_id.as_str())
        .collect();
    if journal
        .completed
        .keys()
        .any(|operation_id| !operation_ids.contains(operation_id.as_str()))
    {
        return Err(CliError::BulkImport.into());
    }
    persist_journal(journal_path, &journal)?;
    let client = http_client(base_url, token_file)?;

    for entry in &entries {
        if journal.completed.contains_key(&entry.operation_id) {
            continue;
        }
        let input = NoteWriteRequestV1 {
            logical_path: entry.logical_path.clone(),
            content_type: entry.content_type.clone(),
            source: entry.source.clone(),
        };
        let result = match (&entry.document_id, entry.expected_revision) {
            (None, None) => client.create_note(&entry.operation_id, &input).await,
            (Some(document_id), Some(revision)) => {
                client
                    .replace_note(document_id, &entry.operation_id, revision, &input)
                    .await
            }
            _ => return Err(CliError::BulkImport.into()),
        }
        .map_err(CliError::from)?;
        let value = serde_json::to_value(result).map_err(CliError::Encode)?;
        journal.completed.insert(entry.operation_id.clone(), value);
        persist_journal(journal_path, &journal)?;
    }

    write_json(&ImportReport {
        version: 1,
        manifest_sha256,
        entry_count: entries.len(),
        completed_count: journal.completed.len(),
        path_hashes: entries.into_iter().map(|entry| entry.path_hash).collect(),
    })
    .map_err(Into::into)
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

fn asset_http_operation_error(operation_id: String, source: HttpClientError) -> CliError {
    let delivery = source.delivery();
    asset_operation_error(operation_id, delivery, source.into())
}

fn asset_operation_error(
    operation_id: String,
    delivery: DeliveryStatus,
    source: CliError,
) -> CliError {
    CliError::AssetOperation {
        operation_id,
        delivery,
        source: Box::new(source),
    }
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
        Command::Asset { .. } => unreachable!("HTTP command reached socket"),
        Command::SystemInfo => Ok(Request::SystemInfo {}),
        Command::Import { .. }
        | Command::ImportManifest { .. }
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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportManifest {
    version: u32,
    root: PathBuf,
    entries: Vec<ImportManifestEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImportManifestEntry {
    operation_id: String,
    document_id: Option<String>,
    expected_revision: Option<u64>,
    logical_path: String,
    content_type: String,
    source_path: PathBuf,
}

#[derive(Debug)]
struct PreparedImportEntry {
    operation_id: String,
    document_id: Option<String>,
    expected_revision: Option<u64>,
    logical_path: String,
    content_type: String,
    source: String,
    path_hash: String,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImportJournal {
    version: u32,
    manifest_sha256: String,
    completed: BTreeMap<String, serde_json::Value>,
}

#[derive(Serialize)]
struct ImportReport {
    version: u32,
    manifest_sha256: String,
    entry_count: usize,
    completed_count: usize,
    path_hashes: Vec<String>,
}

fn preflight_manifest(
    manifest_path: &Path,
    manifest: ImportManifest,
) -> Result<Vec<PreparedImportEntry>, CliError> {
    if manifest.version != 1
        || manifest.entries.is_empty()
        || manifest.entries.len() > MANIFEST_ENTRY_LIMIT
    {
        return Err(CliError::BulkImport);
    }
    let parent = manifest_path.parent().ok_or(CliError::BulkImport)?;
    let root_input = if manifest.root.is_absolute() {
        manifest.root
    } else {
        parent.join(manifest.root)
    };
    let root = root_input.canonicalize().map_err(CliError::BulkIo)?;
    let mut operation_ids = BTreeSet::new();
    let mut document_ids = BTreeSet::new();
    let mut logical_paths = BTreeSet::new();
    let mut source_paths = BTreeSet::new();
    let mut prepared = Vec::with_capacity(manifest.entries.len());
    let mut total_source_bytes = 0_usize;

    for entry in manifest.entries {
        if !valid_client_operation_id(&entry.operation_id)
            || !operation_ids.insert(entry.operation_id.clone())
            || !logical_paths.insert(entry.logical_path.to_lowercase())
            || !relative_normal_path(&entry.source_path)
            || !source_paths.insert(entry.source_path.to_string_lossy().to_lowercase())
            || entry.document_id.is_some() != entry.expected_revision.is_some()
            || entry.document_id.as_deref().is_some_and(|document_id| {
                document_id.trim().is_empty()
                    || document_id.len() > 128
                    || !document_ids.insert(document_id.to_owned())
            })
            || entry
                .expected_revision
                .is_some_and(|revision| i64::try_from(revision).is_err())
        {
            return Err(CliError::BulkImport);
        }
        let source_file = open_confined_file(&root, &entry.source_path)?;
        let mut source_bytes = Vec::new();
        source_file
            .take(CLIENT_INPUT_LIMIT + 1)
            .read_to_end(&mut source_bytes)
            .map_err(CliError::BulkIo)?;
        if source_bytes.len() > CLIENT_INPUT_LIMIT as usize {
            return Err(CliError::ClientInputTooLarge);
        }
        total_source_bytes = total_source_bytes
            .checked_add(source_bytes.len())
            .filter(|total| *total <= MANIFEST_SOURCE_LIMIT)
            .ok_or(CliError::ClientInputTooLarge)?;
        let source = String::from_utf8(source_bytes).map_err(|_| CliError::BulkImport)?;
        validate_note_input(&NoteWriteRequestV1 {
            logical_path: entry.logical_path.clone(),
            content_type: entry.content_type.clone(),
            source: source.clone(),
        })
        .map_err(|_| CliError::BulkImport)?;
        prepared.push(PreparedImportEntry {
            operation_id: entry.operation_id,
            document_id: entry.document_id,
            expected_revision: entry.expected_revision,
            path_hash: format!("{:x}", Sha256::digest(entry.logical_path.as_bytes())),
            logical_path: entry.logical_path,
            content_type: entry.content_type,
            source,
        });
    }
    Ok(prepared)
}

fn import_manifest_sha256(manifest_bytes: &[u8], entries: &[PreparedImportEntry]) -> String {
    let mut digest = Sha256::new();
    digest.update(manifest_bytes);
    for entry in entries {
        digest.update((entry.source.len() as u64).to_be_bytes());
        digest.update(entry.source.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn open_confined_file(root: &Path, relative: &Path) -> Result<File, CliError> {
    use rustix::fs::{Mode, OFlags, open, openat};

    let components = relative.components().collect::<Vec<_>>();
    if components.is_empty() {
        return Err(CliError::BulkImport);
    }
    let mut current = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|error| CliError::BulkIo(error.into()))?;
    for (index, component) in components.into_iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(CliError::BulkImport);
        };
        let last = index + 1 == relative.components().count();
        let flags = if last {
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW
        } else {
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW
        };
        current = openat(&current, name, flags, Mode::empty()).map_err(|_| CliError::BulkImport)?;
    }
    let file = File::from(current);
    if !file.metadata().map_err(CliError::BulkIo)?.is_file() {
        return Err(CliError::BulkImport);
    }
    Ok(file)
}

fn relative_normal_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn read_bounded_file(path: &Path) -> Result<Vec<u8>, CliError> {
    read_file_with_limit(path, CLIENT_INPUT_LIMIT)
}

fn read_file_with_limit(path: &Path, limit: u64) -> Result<Vec<u8>, CliError> {
    let mut bytes = Vec::new();
    File::open(path)
        .map_err(CliError::BulkIo)?
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(CliError::BulkIo)?;
    if bytes.len() > limit as usize {
        return Err(CliError::ClientInputTooLarge);
    }
    Ok(bytes)
}

fn load_journal(path: &Path, manifest_sha256: &str) -> Result<ImportJournal, CliError> {
    if !path.exists() {
        return Ok(ImportJournal {
            version: 1,
            manifest_sha256: manifest_sha256.to_owned(),
            completed: BTreeMap::new(),
        });
    }
    let metadata = path.symlink_metadata().map_err(CliError::BulkIo)?;
    if !metadata.file_type().is_file() || metadata.permissions().mode() & 0o777 != 0o600 {
        return Err(CliError::BulkImport);
    }
    let bytes = read_file_with_limit(path, MAX_IMPORT_JOURNAL_BYTES)?;
    let journal: ImportJournal =
        serde_json::from_slice(&bytes).map_err(|_| CliError::BulkImport)?;
    if journal.version != 1 || journal.manifest_sha256 != manifest_sha256 {
        return Err(CliError::BulkImport);
    }
    Ok(journal)
}

fn persist_journal(path: &Path, journal: &ImportJournal) -> Result<(), CliError> {
    let body = serde_json::to_vec(journal).map_err(CliError::Encode)?;
    if body.len() > MAX_IMPORT_JOURNAL_BYTES as usize {
        return Err(CliError::ClientInputTooLarge);
    }
    let temporary = path.with_extension(format!("tmp-{}", Uuid::now_v7()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&temporary)
            .map_err(CliError::BulkIo)?;
        file.write_all(&body).map_err(CliError::BulkIo)?;
        file.sync_all().map_err(CliError::BulkIo)?;
        fs::rename(&temporary, path).map_err(CliError::BulkIo)?;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(CliError::BulkIo)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn read_source() -> Result<String, CliError> {
    read_source_from(io::stdin())
}

fn read_source_from(reader: impl Read) -> Result<String, CliError> {
    let mut input = Vec::new();
    reader
        .take(CLIENT_INPUT_LIMIT + 1)
        .read_to_end(&mut input)
        .map_err(CliError::ReadInput)?;
    if input.len() > CLIENT_INPUT_LIMIT as usize {
        return Err(CliError::ClientInputTooLarge);
    }
    String::from_utf8(input)
        .map_err(|error| CliError::ReadInput(io::Error::new(io::ErrorKind::InvalidData, error)))
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

fn read_client_bytes() -> Result<Vec<u8>, CliError> {
    let mut input = Vec::new();
    io::stdin()
        .take(CLIENT_INPUT_LIMIT + 1)
        .read_to_end(&mut input)
        .map_err(CliError::ReadClientInput)?;
    if input.len() > CLIENT_INPUT_LIMIT as usize {
        return Err(CliError::ClientInputTooLarge);
    }
    Ok(input)
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

fn write_bytes(value: &[u8]) -> Result<(), CliError> {
    io::stdout()
        .lock()
        .write_all(value)
        .map_err(CliError::WriteOutput)
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
    use std::{
        fs,
        io::Cursor,
        os::unix::fs::{PermissionsExt as _, symlink},
    };

    use clap::Parser as _;
    use gnosis_core::protocol::client_v1::{ClientDeliveryV1, ClientFailureCodeV1};

    use super::{
        AssetCommand, CLIENT_INPUT_LIMIT, Cli, CliError, Command, ImportManifest,
        ImportManifestEntry, RunError, client_failure, correlation_id, preflight_manifest,
        read_device_token, read_source_from, run_asset, run_http_import_manifest,
    };

    #[test]
    fn source_input_is_bounded() {
        let input = vec![b'x'; CLIENT_INPUT_LIMIT as usize + 1];
        assert!(matches!(
            read_source_from(Cursor::new(input)),
            Err(CliError::ClientInputTooLarge)
        ));
    }

    #[test]
    fn single_import_requires_an_explicit_content_type() {
        let cli = Cli::try_parse_from([
            "gnosis",
            "import",
            "document",
            "--operation-id",
            "import-document",
            "--content-type",
            "text/org; charset=utf-8",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Command::Import {
                document_id,
                operation_id,
                content_type,
                if_revision: None,
            } if document_id == "document"
                && operation_id == "import-document"
                && content_type == "text/org; charset=utf-8"
        ));
    }

    #[test]
    fn manifest_preflight_reads_confined_sources_before_dispatch() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("note.org"), "#+title: Note\n").unwrap();
        let entries = preflight_manifest(
            &directory.path().join("manifest.json"),
            ImportManifest {
                version: 1,
                root: directory.path().to_owned(),
                entries: vec![ImportManifestEntry {
                    operation_id: "bulk-note".into(),
                    document_id: None,
                    expected_revision: None,
                    logical_path: "notes/note.org".into(),
                    content_type: "text/org; charset=utf-8".into(),
                    source_path: "note.org".into(),
                }],
            },
        )
        .unwrap();

        assert_eq!(entries[0].source, "#+title: Note\n");
        assert_eq!(entries[0].path_hash.len(), 64);
    }

    #[test]
    fn manifest_preflight_rejects_casefold_collisions() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("first.org"), "#+title: First\n").unwrap();
        fs::write(directory.path().join("second.org"), "#+title: Second\n").unwrap();
        let entry =
            |operation_id: &str, logical_path: &str, source_path: &str| ImportManifestEntry {
                operation_id: operation_id.into(),
                document_id: None,
                expected_revision: None,
                logical_path: logical_path.into(),
                content_type: "text/org; charset=utf-8".into(),
                source_path: source_path.into(),
            };
        let manifest = ImportManifest {
            version: 1,
            root: directory.path().to_owned(),
            entries: vec![
                entry("bulk-first", "Notes/Note.org", "first.org"),
                entry("bulk-second", "notes/note.org", "second.org"),
            ],
        };

        assert!(matches!(
            preflight_manifest(&directory.path().join("manifest.json"), manifest),
            Err(CliError::BulkImport)
        ));
    }

    #[test]
    fn manifest_preflight_rejects_symlink_and_oversized_sources() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("target.org"), "#+title: Target\n").unwrap();
        symlink(
            directory.path().join("target.org"),
            directory.path().join("link.org"),
        )
        .unwrap();
        let entry = |source_path: &str| ImportManifest {
            version: 1,
            root: directory.path().to_owned(),
            entries: vec![ImportManifestEntry {
                operation_id: "bulk-note".into(),
                document_id: None,
                expected_revision: None,
                logical_path: "notes/note.org".into(),
                content_type: "text/org; charset=utf-8".into(),
                source_path: source_path.into(),
            }],
        };

        assert!(matches!(
            preflight_manifest(&directory.path().join("manifest.json"), entry("link.org")),
            Err(CliError::BulkImport)
        ));

        fs::write(
            directory.path().join("large.org"),
            vec![b'x'; 1024 * 1024 + 1],
        )
        .unwrap();
        assert!(matches!(
            preflight_manifest(&directory.path().join("manifest.json"), entry("large.org")),
            Err(CliError::ClientInputTooLarge)
        ));

        fs::write(directory.path().join("invalid.org"), "* Missing title\n").unwrap();
        assert!(matches!(
            preflight_manifest(
                &directory.path().join("manifest.json"),
                entry("invalid.org")
            ),
            Err(CliError::BulkImport)
        ));
    }

    #[tokio::test]
    async fn manifest_dry_run_needs_no_token_or_network_and_writes_no_journal() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("note.org"), "#+title: Note\n").unwrap();
        let manifest_path = directory.path().join("manifest.json");
        fs::write(
            &manifest_path,
            r#"{"version":1,"root":".","entries":[{"operation_id":"op","document_id":null,"expected_revision":null,"logical_path":"note.org","content_type":"text/org; charset=utf-8","source_path":"note.org"}]}"#,
        )
        .unwrap();
        let journal_path = directory.path().join("journal.json");

        run_http_import_manifest("not a URL", None, &manifest_path, &journal_path, true)
            .await
            .unwrap();

        assert!(!journal_path.exists());
    }

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
    fn managed_asset_cli_requires_durable_mutation_identity() {
        let cli = Cli::try_parse_from([
            "gnosis",
            "asset",
            "put",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "image.png",
            "--operation-id",
            "asset-put",
            "--if-revision",
            "7",
            "--media-type",
            "image/png",
        ])
        .unwrap();

        assert!(matches!(
            cli.command,
            Command::Asset {
                command: AssetCommand::Put {
                    operation_id,
                    if_revision: Some(7),
                    ..
                }
            } if operation_id == "asset-put"
        ));
    }

    #[tokio::test]
    async fn managed_asset_pre_dispatch_failures_retain_operation_identity() {
        let digest = "a".repeat(64);
        for (operation_id, command) in [
            (
                "asset-put",
                AssetCommand::Put {
                    digest: digest.clone(),
                    name: "image.png".into(),
                    operation_id: "asset-put".into(),
                    if_revision: None,
                    media_type: "image/png".into(),
                },
            ),
            (
                "asset-delete",
                AssetCommand::Tombstone {
                    digest: digest.clone(),
                    name: "image.png".into(),
                    operation_id: "asset-delete".into(),
                    if_revision: 1,
                },
            ),
        ] {
            let error = run_asset("http://127.0.0.1:7842", None, command)
                .await
                .unwrap_err();
            let RunError::Client(error) = error else {
                panic!("asset command returned a protocol error");
            };
            let failure = client_failure(&error);
            assert_eq!(failure.code, ClientFailureCodeV1::Credentials);
            assert_eq!(failure.delivery, ClientDeliveryV1::NotSent);
            assert_eq!(failure.operation_id.as_deref(), Some(operation_id));
        }
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
