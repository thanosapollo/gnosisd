use std::{
    fs,
    future::Future,
    net::SocketAddr,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::{
    net::{TcpListener, UnixListener, UnixStream},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::timeout,
};

use crate::{
    application::{
        ApplicationError, DueCursor, ImportDocument, ImportSummary, LegacyQueryError,
        PreparedImport, QueryDocument, QueryDueThemata, QueryDueThemataPage, QueryLegacyThema,
        ReviewAttemptMutation, ReviewAttemptOperation, ReviewAttemptOperationResult,
        ReviewAttemptOperationStatus, ReviewOperationAuthority, generate_device,
    },
    protocol::{
        ActiveReviewSessionView, AssetChunkView, BackupView, BackupsPrunedView, DeviceCreatedView,
        DeviceView, DocumentView, DuePageView, DueThemaView, ErrorCode, ImportResult,
        LegacyInfoView, LocalReviewAttemptMutationV2, LocalReviewAttemptStateV2,
        LocalReviewAttemptView, LocalReviewSessionView, MessageValidationError, PROTOCOL_VERSION,
        ProtocolError, Request, RequestEnvelope, ResponseEnvelope, ResponseResult,
        ReviewAnswerInput, ReviewEventView, ReviewSourceInput, SystemInfo, ThemaView, frame,
    },
    service::{ServiceError, ServiceHandle, ServiceRuntime, ServiceStartError},
    store::StoreError,
};

const DEFAULT_MAX_CONNECTIONS: usize = 32;
const DEFAULT_STORE_QUEUE_CAPACITY: usize = 8;
const MAX_CONNECTIONS: usize = 1024;
const MAX_STORE_QUEUE_CAPACITY: usize = 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub database_path: PathBuf,
    pub blob_path: PathBuf,
    pub http_addr: Option<SocketAddr>,
    pub max_connections: usize,
    pub store_queue_capacity: usize,
}

impl DaemonConfig {
    pub fn new(socket_path: impl Into<PathBuf>, database_path: impl Into<PathBuf>) -> Self {
        let database_path = database_path.into();
        let blob_path = database_path.with_extension("blobs");
        Self {
            socket_path: socket_path.into(),
            database_path,
            blob_path,
            http_addr: None,
            max_connections: DEFAULT_MAX_CONNECTIONS,
            store_queue_capacity: DEFAULT_STORE_QUEUE_CAPACITY,
        }
    }
}

pub struct DaemonServer {
    listener: UnixListener,
    http_listener: Option<TcpListener>,
    socket: SocketGuard,
    service: ServiceRuntime,
    max_connections: usize,
}

impl DaemonServer {
    pub async fn bind(config: DaemonConfig) -> Result<Self, ServerError> {
        validate_config(&config)?;
        let _startup_lock = StartupLock::acquire(&config.socket_path)?;
        prepare_socket(&config.socket_path).await?;
        let listener = UnixListener::bind(&config.socket_path).map_err(ServerError::Socket)?;
        let http_listener = match config.http_addr {
            Some(address) => Some(
                TcpListener::bind(address)
                    .await
                    .map_err(ServerError::Http)?,
            ),
            None => None,
        };
        let socket = SocketGuard::new(config.socket_path.clone())?;
        fs::set_permissions(&config.socket_path, fs::Permissions::from_mode(0o600))
            .map_err(ServerError::Socket)?;
        let service = ServiceRuntime::start(
            config.database_path,
            config.blob_path,
            config.store_queue_capacity,
        )
        .await
        .map_err(ServerError::store)?;
        Ok(Self {
            listener,
            http_listener,
            socket,
            service,
            max_connections: config.max_connections,
        })
    }

    pub fn http_addr(&self) -> Option<SocketAddr> {
        self.http_listener
            .as_ref()
            .and_then(|listener| listener.local_addr().ok())
    }

    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<(), ServerError> {
        let Self {
            listener,
            http_listener,
            socket,
            service,
            max_connections,
        } = self;
        let state = ServerState {
            store: service.handle(),
        };
        let semaphore = std::sync::Arc::new(Semaphore::new(max_connections));
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let http_task = http_listener.map(|listener| {
            tokio::spawn(serve_http(
                listener,
                state.store.clone(),
                shutdown_receiver.clone(),
            ))
        });
        let mut handlers = JoinSet::new();
        let accept_error = accept_until_shutdown(
            &listener,
            state.clone(),
            semaphore,
            shutdown_receiver,
            &mut handlers,
            shutdown,
        )
        .await;
        let _ = shutdown_sender.send(true);
        while handlers.join_next().await.is_some() {}
        let http_result = match http_task {
            Some(task) => task.await.map_err(|_| ServerError::HttpTask)?,
            None => Ok(()),
        };
        drop(state);
        service.finish().await.map_err(ServerError::service)?;
        drop(socket);
        accept_error.and(http_result.map_err(ServerError::Http))
    }
}

async fn serve_http(
    listener: TcpListener,
    service: ServiceHandle,
    mut shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    axum::serve(listener, crate::http::router(service))
        .with_graceful_shutdown(async move {
            while !*shutdown.borrow_and_update() && shutdown.changed().await.is_ok() {}
        })
        .await
}

async fn accept_until_shutdown(
    listener: &UnixListener,
    state: ServerState,
    semaphore: std::sync::Arc<Semaphore>,
    shutdown_receiver: watch::Receiver<bool>,
    handlers: &mut JoinSet<()>,
    shutdown: impl Future<Output = ()>,
) -> Result<(), ServerError> {
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            () = &mut shutdown => return Ok(()),
            Some(_) = handlers.join_next(), if !handlers.is_empty() => {}
            accepted = listener.accept() => {
                let (stream, _) = accepted.map_err(ServerError::Socket)?;
                let Ok(permit) = semaphore.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let state = state.clone();
                let receiver = shutdown_receiver.clone();
                handlers.spawn(async move {
                    let _permit = permit;
                    handle_connection(stream, state, receiver).await;
                });
            }
        }
    }
}

async fn handle_connection(
    mut stream: UnixStream,
    state: ServerState,
    mut shutdown: watch::Receiver<bool>,
) {
    let request = tokio::select! {
        _ = shutdown.changed() => return,
        read = timeout(IO_TIMEOUT, frame::read_json_frame(&mut stream)) => read,
    };
    let response = match request {
        Ok(Ok(Some(request))) => dispatch(request, state).await,
        Ok(Ok(None)) => return,
        Ok(Err(_)) => malformed_response(String::new()),
        Err(_) => return,
    };
    let _ = timeout(IO_TIMEOUT, frame::write_json_frame(&mut stream, &response)).await;
}

async fn dispatch(request: RequestEnvelope, state: ServerState) -> ResponseEnvelope {
    let id = request.id.clone();
    if let Err(error) = request.validate() {
        return validation_response(id, error);
    }
    let request_sha256 = match request_sha256(&request.request) {
        Ok(value) => value,
        Err(error) => return ResponseEnvelope::error(id, error),
    };
    let result = match request.request {
        Request::SystemInfo {} => Ok(ResponseResult::SystemInfo(SystemInfo {
            protocol_version: PROTOCOL_VERSION,
        })),
        Request::DocumentImport {
            document_id,
            org,
            expected_revision,
        } => import_document(state, document_id, org, expected_revision).await,
        Request::DocumentGet { document_id } => get_document(state, document_id).await,
        Request::LegacyInfo {} => legacy_info(state).await,
        Request::ThemaGet { thema_id } => get_thema(state, thema_id).await,
        Request::ReviewDue { day, limit } => due_themata(state, day, limit).await,
        Request::ReviewDuePage { day, limit, after } => {
            due_themata_page(state, day, limit, after).await
        }
        Request::ReviewSessionCreate {
            source,
            target_count,
        } => create_review_session(state, source, target_count).await,
        Request::ReviewSessionActive {} => active_review_session(state).await,
        Request::ReviewSessionResume { session_id } => {
            resume_review_session(state, session_id).await
        }
        Request::ReviewSessionQuit { session_id } => {
            stop_review_session(state, session_id, false).await
        }
        Request::ReviewSessionAbort { session_id } => {
            stop_review_session(state, session_id, true).await
        }
        Request::ReviewAttemptBegin { session_id } => begin_review_attempt(state, session_id).await,
        Request::ReviewAttemptAnswer {
            attempt_id,
            response,
        } => answer_review_attempt(state, attempt_id, response).await,
        Request::ReviewAttemptOverride { attempt_id } => {
            override_review_attempt(state, attempt_id, id.clone(), request_sha256.clone()).await
        }
        Request::ReviewAttemptSuspensionToggle { attempt_id } => {
            toggle_review_suspension(state, attempt_id, id.clone(), request_sha256.clone()).await
        }
        Request::ReviewAttemptStateV2 { attempt_id } => {
            review_attempt_state_v2(state, attempt_id).await
        }
        Request::ReviewAttemptAcceptanceSetV2 {
            operation_version,
            operation_id,
            attempt_id,
            expected_attempt_revision,
            accepted_success,
        } => {
            apply_review_attempt_operation_v2(
                state,
                operation_version,
                operation_id,
                attempt_id,
                expected_attempt_revision,
                ReviewAttemptMutation::AcceptedSuccess(accepted_success),
            )
            .await
        }
        Request::ReviewAttemptSuspensionSetV2 {
            operation_version,
            operation_id,
            attempt_id,
            expected_attempt_revision,
            desired_suspended,
        } => {
            apply_review_attempt_operation_v2(
                state,
                operation_version,
                operation_id,
                attempt_id,
                expected_attempt_revision,
                ReviewAttemptMutation::Suspension(desired_suspended),
            )
            .await
        }
        Request::ReviewAttemptCommit { attempt_id } => {
            commit_review_attempt(state, attempt_id).await
        }
        Request::AssetChunk {
            logical_path,
            offset,
            limit,
        } => asset_chunk(state, logical_path, offset, limit).await,
        Request::DeviceCreate { name, scopes } => create_device(state, name, scopes).await,
        Request::DeviceList {} => list_devices(state).await,
        Request::DeviceRevoke { device_id } => revoke_device(state, device_id).await,
        Request::BackupCreate { destination } => create_backup(state, destination).await,
        Request::BackupVerify { path } => verify_backup(state, path).await,
        Request::BackupPrune { directory, keep } => prune_backups(state, directory, keep).await,
        Request::BackupRestoreValidate {
            path,
            temporary_directory,
        } => validate_restore(state, path, temporary_directory).await,
    };
    match result {
        Ok(result) => bounded_success(id, result),
        Err(error) => ResponseEnvelope::error(id, error),
    }
}

fn bounded_success(id: String, result: ResponseResult) -> ResponseEnvelope {
    let response = ResponseEnvelope::success(id.clone(), result);
    match serde_json::to_vec(&response) {
        Ok(bytes) if bytes.len() <= crate::protocol::MAX_FRAME_SIZE => response,
        _ => ResponseEnvelope::error(
            id,
            ProtocolError::new(ErrorCode::Internal, "response exceeds protocol limit"),
        ),
    }
}

async fn asset_chunk(
    state: ServerState,
    logical_path: String,
    offset: u64,
    limit: u32,
) -> Result<ResponseResult, ProtocolError> {
    let (head, bytes) = state
        .store
        .asset(logical_path.clone())
        .await
        .map_err(actor_error)?
        .ok_or_else(asset_not_found)?;
    let total_size = u64::try_from(bytes.len()).map_err(|_| internal_error())?;
    let start = usize::try_from(offset).map_err(|_| invalid_asset_offset())?;
    if start > bytes.len() {
        return Err(invalid_asset_offset());
    }
    let limit = usize::try_from(limit).map_err(|_| invalid_asset_offset())?;
    let end = start.saturating_add(limit).min(bytes.len());
    let content_sha256 = head.blob_sha256.ok_or_else(internal_error)?;
    Ok(ResponseResult::AssetChunk(AssetChunkView {
        logical_path,
        content_sha256,
        total_size,
        offset,
        chunk_base64: STANDARD.encode(&bytes[start..end]),
        eof: end == bytes.len(),
    }))
}

async fn legacy_info(state: ServerState) -> Result<ResponseResult, ProtocolError> {
    let info = state.store.legacy_info().await.map_err(actor_error)?;
    Ok(ResponseResult::LegacyInfo(LegacyInfoView::from(info)))
}

async fn get_thema(state: ServerState, thema_id: String) -> Result<ResponseResult, ProtocolError> {
    let query = QueryLegacyThema::new(thema_id).map_err(invalid_legacy_query)?;
    let thema = state
        .store
        .get_legacy_thema(query.thema_id().clone())
        .await
        .map_err(actor_error)?
        .ok_or_else(thema_not_found)?;
    Ok(ResponseResult::Thema(ThemaView::from(thema)))
}

async fn due_themata(
    state: ServerState,
    day: u32,
    limit: u16,
) -> Result<ResponseResult, ProtocolError> {
    let query = QueryDueThemata::new(day, limit).map_err(invalid_legacy_query)?;
    let themata = state.store.due_themata(query).await.map_err(actor_error)?;
    Ok(ResponseResult::DueThemata(
        themata.into_iter().map(DueThemaView::from).collect(),
    ))
}

async fn due_themata_page(
    state: ServerState,
    day: u32,
    limit: u16,
    after: Option<String>,
) -> Result<ResponseResult, ProtocolError> {
    let query = QueryDueThemataPage::new(day, limit, after).map_err(invalid_legacy_query)?;
    let mut themata = state
        .store
        .due_themata_page(query)
        .await
        .map_err(actor_error)?;
    let next = if themata.len() > usize::from(limit) {
        themata.truncate(usize::from(limit));
        Some(
            DueCursor::after(themata.last().ok_or_else(internal_error)?)
                .map_err(|_| internal_error())?
                .encode(),
        )
    } else {
        None
    };
    Ok(ResponseResult::DuePage(DuePageView::new(themata, next)))
}

async fn create_review_session(
    state: ServerState,
    source: ReviewSourceInput,
    target_count: Option<u32>,
) -> Result<ResponseResult, ProtocolError> {
    let source = source.try_into().map_err(|_| invalid_review())?;
    let session = state
        .store
        .create_review_session(source, target_count, None, review_now_us()?)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::ReviewSession(LocalReviewSessionView::from(
        session,
    )))
}

async fn active_review_session(state: ServerState) -> Result<ResponseResult, ProtocolError> {
    let session = state
        .store
        .active_review_session(None)
        .await
        .map_err(actor_error)?
        .map(LocalReviewSessionView::from);
    Ok(ResponseResult::ActiveReviewSession(
        ActiveReviewSessionView { session },
    ))
}

async fn resume_review_session(
    state: ServerState,
    session_id: String,
) -> Result<ResponseResult, ProtocolError> {
    let session = state
        .store
        .resume_review_session(session_id, None)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::ReviewSession(LocalReviewSessionView::from(
        session,
    )))
}

async fn stop_review_session(
    state: ServerState,
    session_id: String,
    abort: bool,
) -> Result<ResponseResult, ProtocolError> {
    let session = state
        .store
        .stop_review_session(session_id, None, abort, review_now_us()?)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::ReviewSession(LocalReviewSessionView::from(
        session,
    )))
}

async fn begin_review_attempt(
    state: ServerState,
    session_id: String,
) -> Result<ResponseResult, ProtocolError> {
    let attempt = state
        .store
        .begin_review_attempt(session_id, None, review_now_us()?)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::ReviewAttempt(Box::new(
        LocalReviewAttemptView::from(attempt),
    )))
}

async fn answer_review_attempt(
    state: ServerState,
    attempt_id: String,
    response: ReviewAnswerInput,
) -> Result<ResponseResult, ProtocolError> {
    let attempt = state
        .store
        .grade_review_attempt(attempt_id, None, response.into(), review_now_us()?)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::ReviewAttempt(Box::new(
        LocalReviewAttemptView::from(attempt),
    )))
}

async fn override_review_attempt(
    state: ServerState,
    attempt_id: String,
    request_id: String,
    request_sha256: String,
) -> Result<ResponseResult, ProtocolError> {
    let attempt = state
        .store
        .override_local_review_attempt(attempt_id, request_id, request_sha256, review_now_us()?)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::ReviewAttempt(Box::new(
        LocalReviewAttemptView::from(attempt),
    )))
}

async fn toggle_review_suspension(
    state: ServerState,
    attempt_id: String,
    request_id: String,
    request_sha256: String,
) -> Result<ResponseResult, ProtocolError> {
    let attempt = state
        .store
        .toggle_local_attempt_suspension(attempt_id, request_id, request_sha256, review_now_us()?)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::ReviewAttempt(Box::new(
        LocalReviewAttemptView::from(attempt),
    )))
}

async fn review_attempt_state_v2(
    state: ServerState,
    attempt_id: String,
) -> Result<ResponseResult, ProtocolError> {
    let attempt = state
        .store
        .review_attempt_state(attempt_id, None)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::ReviewAttemptStateV2(Box::new(
        LocalReviewAttemptStateV2::from(attempt),
    )))
}

async fn apply_review_attempt_operation_v2(
    state: ServerState,
    operation_version: u32,
    operation_id: String,
    attempt_id: String,
    expected_attempt_revision: u64,
    mutation: ReviewAttemptMutation,
) -> Result<ResponseResult, ProtocolError> {
    let operation = ReviewAttemptOperation::new(
        operation_version,
        operation_id,
        attempt_id,
        expected_attempt_revision,
        mutation,
    )
    .map_err(|_| invalid_review())?;
    let result = state
        .store
        .apply_review_attempt_operation(
            operation,
            ReviewOperationAuthority::Local,
            review_now_us()?,
        )
        .await
        .map_err(attempt_operation_error)?;
    desired_review_operation_result(result)
}

fn desired_review_operation_result(
    result: ReviewAttemptOperationResult,
) -> Result<ResponseResult, ProtocolError> {
    match result.status {
        ReviewAttemptOperationStatus::Accepted => Ok(ResponseResult::ReviewAttemptMutationV2(
            Box::new(LocalReviewAttemptMutationV2::from(result)),
        )),
        ReviewAttemptOperationStatus::RevisionConflict => Err(ProtocolError::new(
            ErrorCode::RevisionConflict,
            "review attempt revision changed",
        )),
        ReviewAttemptOperationStatus::StaleAttempt => Err(ProtocolError::new(
            ErrorCode::StaleAttempt,
            "thema content or review state changed",
        )),
        ReviewAttemptOperationStatus::StateConflict => Err(ProtocolError::new(
            ErrorCode::ReviewStateConflict,
            "review attempt is not mutable",
        )),
    }
}

fn attempt_operation_error(error: ServiceError) -> ProtocolError {
    match error {
        ServiceError::Store(StoreError::IdempotencyConflict) => ProtocolError::new(
            ErrorCode::IdempotencyConflict,
            "operation identity was reused with different content",
        ),
        other => actor_error(other),
    }
}

async fn commit_review_attempt(
    state: ServerState,
    attempt_id: String,
) -> Result<ResponseResult, ProtocolError> {
    let event_id = state
        .store
        .commit_review_attempt(attempt_id, None, review_now_us()?)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::ReviewEvent(ReviewEventView { event_id }))
}

async fn import_document(
    state: ServerState,
    document_id: String,
    org: String,
    expected_revision: Option<u64>,
) -> Result<ResponseResult, ProtocolError> {
    let command =
        ImportDocument::new(document_id, org, expected_revision).map_err(|_| invalid_document())?;
    let prepared = tokio::task::spawn_blocking(move || command.prepare_server())
        .await
        .map_err(|_| internal_error())?
        .map_err(|_| invalid_document())?;
    let prepared = tokio::task::spawn_blocking(move || {
        validate_response_size(&prepared.prepared)?;
        Ok::<_, ProtocolError>(prepared)
    })
    .await
    .map_err(|_| internal_error())??;
    let summary = state.store.replace(prepared).await.map_err(actor_error)?;
    Ok(ResponseResult::DocumentImported(import_result(summary)))
}

fn validate_response_size(prepared: &PreparedImport) -> Result<(), ProtocolError> {
    let view = DocumentView::from_projection(&prepared.document_id, u64::MAX, &prepared.projection);
    let response = ResponseEnvelope::success(
        "x".repeat(crate::protocol::MAX_REQUEST_ID_LEN),
        ResponseResult::Document(view),
    );
    let size = serde_json::to_vec(&response)
        .map_err(|_| internal_error())?
        .len();
    if size > crate::protocol::MAX_FRAME_SIZE {
        Err(ProtocolError::new(
            ErrorCode::InvalidDocument,
            "document projection exceeds protocol limit",
        ))
    } else {
        Ok(())
    }
}

fn import_result(summary: ImportSummary) -> ImportResult {
    ImportResult {
        document_id: summary.document_id.to_string(),
        revision: summary.revision,
        content_sha256: summary.content_sha256,
        node_count: summary.node_count,
        occurrence_count: summary.occurrence_count,
        semantic_edge_count: summary.semantic_edge_count,
    }
}

async fn get_document(
    state: ServerState,
    document_id: String,
) -> Result<ResponseResult, ProtocolError> {
    let query = QueryDocument::new(document_id).map_err(|_| invalid_document())?;
    let stored = state
        .store
        .get(query.document_id().clone())
        .await
        .map_err(actor_error)?;
    let stored = stored.ok_or_else(not_found)?;
    Ok(ResponseResult::Document(DocumentView::from_projection(
        stored.document_id(),
        stored.revision(),
        stored.projection(),
    )))
}

async fn create_device(
    state: ServerState,
    name: String,
    scopes: Vec<String>,
) -> Result<ResponseResult, ProtocolError> {
    let device = generate_device(name, scopes).map_err(|_| malformed_admin())?;
    let token = device.token.clone();
    let record = state
        .store
        .create_device(device, now_us()?)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::DeviceCreated(DeviceCreatedView {
        device: DeviceView::from(record),
        token,
    }))
}

async fn list_devices(state: ServerState) -> Result<ResponseResult, ProtocolError> {
    let devices = state.store.devices().await.map_err(actor_error)?;
    Ok(ResponseResult::Devices(
        devices.into_iter().map(DeviceView::from).collect(),
    ))
}

async fn revoke_device(
    state: ServerState,
    device_id: String,
) -> Result<ResponseResult, ProtocolError> {
    if device_id.is_empty() {
        return Err(malformed_admin());
    }
    let device = state
        .store
        .revoke_device(device_id, now_us()?)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::Device(DeviceView::from(device)))
}

async fn create_backup(
    state: ServerState,
    destination: String,
) -> Result<ResponseResult, ProtocolError> {
    require_path(&destination)?;
    let created_at_us = u64::try_from(now_us()?).map_err(|_| internal_error())?;
    let summary = state
        .store
        .create_backup(destination.into(), created_at_us)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::Backup(backup_view(summary)))
}

async fn verify_backup(state: ServerState, path: String) -> Result<ResponseResult, ProtocolError> {
    require_path(&path)?;
    let summary = state
        .store
        .verify_backup(path.into())
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::Backup(backup_view(summary)))
}

async fn prune_backups(
    state: ServerState,
    directory: String,
    keep: u16,
) -> Result<ResponseResult, ProtocolError> {
    require_path(&directory)?;
    if keep == 0 {
        return Err(ProtocolError::new(
            ErrorCode::MalformedRequest,
            "backup retention must be positive",
        ));
    }
    let removed = state
        .store
        .prune_backups(directory.into(), keep)
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::BackupsPruned(BackupsPrunedView {
        removed: removed
            .into_iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect(),
    }))
}

async fn validate_restore(
    state: ServerState,
    path: String,
    temporary_directory: String,
) -> Result<ResponseResult, ProtocolError> {
    require_path(&path)?;
    require_path(&temporary_directory)?;
    let summary = state
        .store
        .validate_restore(path.into(), temporary_directory.into())
        .await
        .map_err(actor_error)?;
    Ok(ResponseResult::Backup(backup_view(summary)))
}

fn backup_view(summary: crate::service::BackupSummary) -> BackupView {
    BackupView {
        path: summary.path.to_string_lossy().into_owned(),
        created_at_us: summary.created_at_us,
        database_sha256: summary.database_sha256,
        blob_count: summary.blob_count,
    }
}

fn require_path(path: &str) -> Result<(), ProtocolError> {
    if path.is_empty() || path.contains('\0') {
        Err(ProtocolError::new(
            ErrorCode::MalformedRequest,
            "invalid backup path",
        ))
    } else {
        Ok(())
    }
}

fn now_us() -> Result<i64, ProtocolError> {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| internal_error())?
        .as_micros();
    i64::try_from(micros).map_err(|_| internal_error())
}

fn review_now_us() -> Result<u64, ProtocolError> {
    u64::try_from(now_us()?).map_err(|_| internal_error())
}

fn request_sha256(request: &Request) -> Result<String, ProtocolError> {
    let bytes = serde_json::to_vec(request).map_err(|_| internal_error())?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn malformed_admin() -> ProtocolError {
    ProtocolError::new(ErrorCode::MalformedRequest, "invalid device operation")
}

fn validation_response(id: String, error: MessageValidationError) -> ResponseEnvelope {
    let error = match error {
        MessageValidationError::UnsupportedVersion(_) => ProtocolError::new(
            ErrorCode::UnsupportedVersion,
            "unsupported protocol version",
        ),
        MessageValidationError::InvalidRequestId => {
            ProtocolError::new(ErrorCode::MalformedRequest, "invalid request ID")
        }
        MessageValidationError::InvalidReviewRequest => {
            ProtocolError::new(ErrorCode::MalformedRequest, "invalid review request")
        }
        MessageValidationError::InvalidAssetChunk => {
            ProtocolError::new(ErrorCode::MalformedRequest, "invalid asset chunk request")
        }
    };
    ResponseEnvelope::error(id, error)
}

fn malformed_response(id: String) -> ResponseEnvelope {
    ResponseEnvelope::error(
        id,
        ProtocolError::new(ErrorCode::MalformedRequest, "malformed request"),
    )
}

fn invalid_document() -> ProtocolError {
    ProtocolError::new(ErrorCode::InvalidDocument, "invalid document")
}

fn not_found() -> ProtocolError {
    ProtocolError::new(ErrorCode::NotFound, "document not found")
}

fn thema_not_found() -> ProtocolError {
    ProtocolError::new(ErrorCode::NotFound, "thema not found")
}

fn asset_not_found() -> ProtocolError {
    ProtocolError::new(ErrorCode::NotFound, "managed asset not found")
}

fn invalid_asset_offset() -> ProtocolError {
    ProtocolError::new(ErrorCode::MalformedRequest, "invalid asset offset")
}

fn invalid_legacy_query(_error: LegacyQueryError) -> ProtocolError {
    ProtocolError::new(ErrorCode::MalformedRequest, "invalid legacy query")
}

fn invalid_review() -> ProtocolError {
    ProtocolError::new(ErrorCode::MalformedRequest, "invalid review request")
}

fn internal_error() -> ProtocolError {
    ProtocolError::new(ErrorCode::Internal, "internal daemon failure")
}

fn actor_error(error: ServiceError) -> ProtocolError {
    match error {
        ServiceError::Busy => ProtocolError::new(ErrorCode::Busy, "daemon is busy"),
        ServiceError::InvalidReviewReplay => ProtocolError::new(
            ErrorCode::MalformedRequest,
            "invalid review replay identity",
        ),
        ServiceError::Application(error) => actor_application_error(error),
        ServiceError::Store(error) => actor_store_error(error),
        ServiceError::Stopped
        | ServiceError::Corrupt
        | ServiceError::AssetDigest
        | ServiceError::InvalidBlobMetadata
        | ServiceError::Blob(_)
        | ServiceError::Review(_)
        | ServiceError::Settings(_)
        | ServiceError::Backup(_) => internal_error(),
    }
}

fn actor_application_error(error: ApplicationError) -> ProtocolError {
    match error {
        ApplicationError::Conflict { .. } => {
            ProtocolError::new(ErrorCode::Conflict, "document revision conflict")
        }
        ApplicationError::InvalidContent(_) => {
            ProtocolError::new(ErrorCode::MalformedRequest, "invalid document content")
        }
        ApplicationError::InvalidDocumentId(_)
        | ApplicationError::InvalidOrg(_)
        | ApplicationError::Storage => internal_error(),
    }
}

fn actor_store_error(error: StoreError) -> ProtocolError {
    match error {
        StoreError::NotFound => ProtocolError::new(ErrorCode::NotFound, "resource not found"),
        StoreError::InvalidReviewResponse => {
            ProtocolError::new(ErrorCode::MalformedRequest, "invalid review response")
        }
        StoreError::ActiveReviewSession
        | StoreError::StaleAttempt
        | StoreError::InvalidReviewState
        | StoreError::ReviewOwnership
        | StoreError::IdempotencyConflict => {
            ProtocolError::new(ErrorCode::Conflict, "review state conflict")
        }
        StoreError::Conflict { .. }
        | StoreError::DeviceSequence { .. }
        | StoreError::ReplicaCursor
        | StoreError::ReplicaServer
        | StoreError::BlobRequired { .. }
        | StoreError::BlobMetadata
        | StoreError::PathChange
        | StoreError::InvalidProjection { .. }
        | StoreError::CorruptData { .. }
        | StoreError::UnsupportedSchemaVersion { .. }
        | StoreError::Storage(_) => internal_error(),
    }
}

#[derive(Clone)]
struct ServerState {
    store: ServiceHandle,
}

async fn prepare_socket(path: &Path) -> Result<(), ServerError> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ServerError::Socket(error)),
    };
    if !metadata.file_type().is_socket() {
        return Err(ServerError::SocketPathOccupied);
    }
    match UnixStream::connect(path).await {
        Ok(_) => Err(ServerError::AlreadyRunning),
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => {
            remove_observed_socket(path, &metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(ServerError::Socket(error)),
    }
}

fn remove_observed_socket(path: &Path, observed: &fs::Metadata) -> Result<(), ServerError> {
    let current = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(ServerError::Socket(error)),
    };
    if !current.file_type().is_socket()
        || current.dev() != observed.dev()
        || current.ino() != observed.ino()
    {
        return Err(ServerError::SocketChanged);
    }
    fs::remove_file(path).map_err(ServerError::Socket)
}

struct SocketGuard {
    path: PathBuf,
    device: u64,
    inode: u64,
}

#[derive(Debug)]
struct StartupLock {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl StartupLock {
    fn acquire(socket_path: &Path) -> Result<Self, ServerError> {
        let path = startup_lock_path(socket_path);
        fs::create_dir(&path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                ServerError::StartupLocked
            } else {
                ServerError::Socket(error)
            }
        })?;
        if let Err(error) = fs::set_permissions(&path, fs::Permissions::from_mode(0o700)) {
            let _ = fs::remove_dir(&path);
            return Err(ServerError::Socket(error));
        }
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) => {
                let _ = fs::remove_dir(&path);
                return Err(ServerError::Socket(error));
            }
        };
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for StartupLock {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_dir()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = fs::remove_dir(&self.path);
        }
    }
}

fn startup_lock_path(socket_path: &Path) -> PathBuf {
    let mut value = socket_path.as_os_str().to_owned();
    value.push(".startup-lock");
    PathBuf::from(value)
}

impl SocketGuard {
    fn new(path: PathBuf) -> Result<Self, ServerError> {
        let metadata = fs::symlink_metadata(&path).map_err(ServerError::Socket)?;
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.device
            && metadata.ino() == self.inode
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn validate_config(config: &DaemonConfig) -> Result<(), ServerError> {
    if !(3..=MAX_CONNECTIONS).contains(&config.max_connections)
        || !(1..=MAX_STORE_QUEUE_CAPACITY).contains(&config.store_queue_capacity)
        || config
            .http_addr
            .is_some_and(|address| !address.ip().is_loopback())
    {
        Err(ServerError::InvalidConfig)
    } else {
        Ok(())
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("daemon connection or queue bound is outside the supported range")]
    InvalidConfig,
    #[error("socket path is occupied by a non-socket file")]
    SocketPathOccupied,
    #[error("another daemon is already listening on the socket")]
    AlreadyRunning,
    #[error("socket path changed while stale state was being checked")]
    SocketChanged,
    #[error("another daemon startup owns the socket lock")]
    StartupLocked,
    #[error("socket operation failed")]
    Socket(#[source] std::io::Error),
    #[error("HTTP listener failed")]
    Http(#[source] std::io::Error),
    #[error("HTTP server task stopped unexpectedly")]
    HttpTask,
    #[error("database initialization failed")]
    Store(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("blob storage initialization failed")]
    Blob(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("could not start database owner thread")]
    ActorStart(#[source] std::io::Error),
    #[error("database owner thread stopped unexpectedly")]
    ActorStopped,
}

impl ServerError {
    fn store(error: ServiceStartError) -> Self {
        match error {
            ServiceStartError::Store(error) => Self::Store(Box::new(error)),
            ServiceStartError::Blob(error) => Self::Blob(Box::new(error)),
            ServiceStartError::Thread(error) => Self::ActorStart(error),
            ServiceStartError::Stopped => Self::ActorStopped,
        }
    }

    fn service(_error: ServiceError) -> Self {
        Self::ActorStopped
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::net::UnixListener as StdUnixListener};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        DaemonConfig, ServerError, StartupLock, bounded_success, remove_observed_socket,
        validate_config,
    };
    use crate::{
        application::ApplicationError,
        protocol::{DueThemaView, ErrorCode, ResponseOutcome, ResponseResult},
        service::ServiceError,
        store::StoreError,
    };

    #[test]
    fn busy_actor_error_becomes_stable_protocol_error() {
        let error = super::actor_error(ServiceError::Busy);
        assert_eq!(error.code, crate::protocol::ErrorCode::Busy);
        assert_eq!(error.message, "daemon is busy");
    }

    #[test]
    fn actor_errors_separate_client_faults_from_corruption() {
        assert_actor_error(
            ServiceError::Store(StoreError::InvalidReviewResponse),
            ErrorCode::MalformedRequest,
            "invalid review response",
        );
        assert_actor_error(
            ServiceError::Application(ApplicationError::Conflict {
                expected: 1,
                actual: Some(2),
            }),
            ErrorCode::Conflict,
            "document revision conflict",
        );
        assert_actor_error(
            ServiceError::Store(StoreError::CorruptData {
                field: "private field",
            }),
            ErrorCode::Internal,
            "internal daemon failure",
        );
    }

    fn assert_actor_error(error: ServiceError, code: ErrorCode, message: &'static str) {
        let error = super::actor_error(error);
        assert_eq!(error.code, code);
        assert_eq!(error.message, message);
    }

    #[test]
    fn oversized_success_becomes_stable_protocol_error() {
        let response = bounded_success(
            "large-response".into(),
            ResponseResult::DueThemata(vec![DueThemaView {
                id: "thema".into(),
                kind: "basic".into(),
                keimenon: "x".repeat(crate::protocol::MAX_FRAME_SIZE),
                hypothesis: Vec::new(),
                answer: Vec::new(),
                next_review_day: 20260714,
                review_count: 0,
            }]),
        );
        let ResponseOutcome::Failure { error } = response.outcome else {
            panic!("oversized response was accepted");
        };
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(error.message, "response exceeds protocol limit");
    }

    #[test]
    fn stale_socket_cleanup_preserves_replacement_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("socket");
        drop(StdUnixListener::bind(&path).unwrap());
        let observed = fs::symlink_metadata(&path).unwrap();
        fs::remove_file(&path).unwrap();
        fs::write(&path, "replacement").unwrap();
        let error = remove_observed_socket(&path, &observed).unwrap_err();
        assert!(matches!(error, ServerError::SocketChanged));
        assert_eq!(fs::read_to_string(path).unwrap(), "replacement");
    }

    #[test]
    fn startup_lock_serializes_stale_cleanup_and_bind() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("socket");
        let first = StartupLock::acquire(&socket).unwrap();
        let second = StartupLock::acquire(&socket).unwrap_err();
        assert!(matches!(second, ServerError::StartupLocked));
        drop(first);
        assert!(StartupLock::acquire(&socket).is_ok());
    }

    #[test]
    fn http_listener_must_remain_on_loopback() {
        let mut config = DaemonConfig::new("socket", "database");
        config.http_addr = Some("127.0.0.1:7842".parse().unwrap());
        assert!(validate_config(&config).is_ok());
        config.http_addr = Some("0.0.0.0:7842".parse().unwrap());
        assert!(matches!(
            validate_config(&config),
            Err(ServerError::InvalidConfig)
        ));
    }

    #[tokio::test]
    async fn daemon_serves_http_health_on_its_loopback_listener() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = DaemonConfig::new(
            directory.path().join("gnosis.sock"),
            directory.path().join("gnosis.sqlite"),
        );
        config.http_addr = Some("127.0.0.1:0".parse().unwrap());
        let server = super::DaemonServer::bind(config).await.unwrap();
        let address = server.http_addr().unwrap();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server.run(async move {
            let _ = stopped.await;
        }));
        let mut stream = tokio::net::TcpStream::connect(address).await.unwrap();
        stream
            .write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).await.unwrap();
        assert!(response.starts_with("HTTP/1.1 200 OK"));
        assert!(response.ends_with("{\"status\":\"ok\"}"));
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }
}
