use std::{
    fs,
    future::Future,
    net::SocketAddr,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use thiserror::Error;
use tokio::{
    net::{TcpListener, UnixListener, UnixStream},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::timeout,
};

use crate::{
    application::{ApplicationError, generate_device},
    protocol::{
        BackupView, BackupsPrunedView, DeviceCreatedView, DeviceView, ErrorCode,
        MessageValidationError, PROTOCOL_VERSION, ProtocolError, RepairView, Request,
        RequestEnvelope, ResponseEnvelope, ResponseResult, SystemInfo, frame,
    },
    service::{ServiceError, ServiceHandle, ServiceRuntime, ServiceStartError},
    store::StoreError,
};

const DEFAULT_MAX_CONNECTIONS: usize = 32;
const DEFAULT_STORE_QUEUE_CAPACITY: usize = 8;
const MAX_CONNECTIONS: usize = 1024;
const MAX_STORE_QUEUE_CAPACITY: usize = 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(not(test))]
const HTTP_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const HTTP_DRAIN_TIMEOUT: Duration = Duration::from_millis(100);

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

    pub async fn restore_backup(
        config: DaemonConfig,
        backup: impl AsRef<Path>,
    ) -> Result<PathBuf, ServerError> {
        validate_config(&config)?;
        let startup_lock = StartupLock::acquire(&config.socket_path)?;
        prepare_socket(&config.socket_path).await?;
        let backup = backup.as_ref().to_owned();
        tokio::task::spawn_blocking(move || {
            let _startup_lock = startup_lock;
            crate::service::cold_restore(&backup, &config.database_path, &config.blob_path)
        })
        .await
        .map_err(|_| ServerError::RestoreTask)?
        .map_err(|error| ServerError::Restore(Box::new(error)))
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
    shutdown: watch::Receiver<bool>,
) -> std::io::Result<()> {
    let app = crate::http::router_with_shutdown(service, shutdown.clone());
    let drain_shutdown = shutdown.clone();
    let server = std::future::IntoFuture::into_future(
        axum::serve(listener, app).with_graceful_shutdown(shutdown_requested(shutdown)),
    );
    tokio::pin!(server);
    tokio::select! {
        result = &mut server => result,
        () = shutdown_requested(drain_shutdown) => {
            match timeout(HTTP_DRAIN_TIMEOUT, &mut server).await {
                Ok(result) => result,
                Err(_) => Ok(()),
            }
        }
    }
}

async fn shutdown_requested(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow_and_update() && shutdown.changed().await.is_ok() {}
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
    let result = match request.request {
        Request::SystemInfo {} => Ok(ResponseResult::SystemInfo(SystemInfo {
            protocol_version: PROTOCOL_VERSION,
        })),

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
        Request::Repair { batch_size } => repair(state, batch_size).await,
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

async fn repair(state: ServerState, batch_size: u16) -> Result<ResponseResult, ProtocolError> {
    let summary = state.store.repair(batch_size).await.map_err(actor_error)?;
    Ok(ResponseResult::Repair(RepairView {
        document_revisions_repaired: summary.document_revisions_repaired,
        blobs_verified: summary.blobs_verified,
        max_batch_observed: summary.max_batch_observed,
    }))
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
        MessageValidationError::InvalidRepairBatchSize => {
            ProtocolError::new(ErrorCode::MalformedRequest, "invalid repair batch size")
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

fn internal_error() -> ProtocolError {
    ProtocolError::new(ErrorCode::Internal, "internal daemon failure")
}

fn actor_error(error: ServiceError) -> ProtocolError {
    match error {
        ServiceError::Busy => ProtocolError::new(ErrorCode::Busy, "daemon is busy"),

        ServiceError::Application(error) => actor_application_error(error),
        ServiceError::Store(error) => actor_store_error(error),
        ServiceError::Stopped
        | ServiceError::Corrupt
        | ServiceError::AssetDigest
        | ServiceError::InvalidBlobMetadata
        | ServiceError::InvalidRepairBatchSize
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
        | StoreError::ExpectedAbsent { .. }
        | StoreError::DeviceSequence { .. }
        | StoreError::ReplicaCursor
        | StoreError::ReplicaServer
        | StoreError::InvalidCursor
        | StoreError::BlobRequired { .. }
        | StoreError::BlobMetadata
        | StoreError::PathChange
        | StoreError::PathConflict
        | StoreError::InvalidProjection { .. }
        | StoreError::CorruptData { .. }
        | StoreError::UnsupportedSchemaVersion { .. }
        | StoreError::UnsupportedApplicationId { .. }
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
    #[error("cold restore failed")]
    Restore(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("cold restore task failed")]
    RestoreTask,
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
    use std::{fs, os::unix::net::UnixListener as StdUnixListener, time::Duration};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use super::{
        DaemonConfig, ServerError, StartupLock, bounded_success, remove_observed_socket,
        validate_config,
    };
    use crate::{
        application::{ApplicationError, generate_device},
        protocol::{DeviceView, ErrorCode, ResponseOutcome, ResponseResult},
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
            ResponseResult::Devices(vec![DeviceView {
                device_id: "device".into(),
                name: "x".repeat(crate::protocol::MAX_FRAME_SIZE),
                scopes: Vec::new(),
                created_at_us: 0,
                last_used_at_us: None,
                revoked_at_us: None,
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

    #[tokio::test]
    async fn cold_restore_requires_exclusive_stopped_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("socket");
        let config = DaemonConfig::new(&socket, directory.path().join("gnosis.sqlite"));

        let startup_lock = StartupLock::acquire(&socket).unwrap();
        let error = super::DaemonServer::restore_backup(
            config.clone(),
            directory.path().join("missing-backup"),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, ServerError::StartupLocked));
        drop(startup_lock);

        let server = super::DaemonServer::bind(config.clone()).await.unwrap();
        let error =
            super::DaemonServer::restore_backup(config, directory.path().join("missing-backup"))
                .await
                .unwrap_err();
        assert!(matches!(error, ServerError::AlreadyRunning));
        drop(server);
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
        assert!(response.ends_with("{\"status\":\"ready\"}"));
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn http_listener_has_no_uptime_deadline() {
        let directory = tempfile::tempdir().unwrap();
        let mut config = DaemonConfig::new(
            directory.path().join("gnosis.sock"),
            directory.path().join("gnosis.sqlite"),
        );
        config.http_addr = Some("127.0.0.1:0".parse().unwrap());
        let server = super::DaemonServer::bind(config).await.unwrap();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server.run(async move {
            let _ = stopped.await;
        }));
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert!(!task.is_finished());
        shutdown.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn shutdown_closes_sse_and_stalled_request_after_an_accepted_write() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let mut config = DaemonConfig::new(directory.path().join("gnosis.sock"), &database);
        config.http_addr = Some("127.0.0.1:0".parse().unwrap());
        let server = super::DaemonServer::bind(config).await.unwrap();
        let service = server.service.handle();
        let device =
            generate_device("writer".into(), vec!["read".into(), "content:write".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let address = server.http_addr().unwrap();
        let (shutdown, stopped) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(server.run(async move {
            let _ = stopped.await;
        }));

        let mut events = tokio::net::TcpStream::connect(address).await.unwrap();
        events
            .write_all(
                format!(
                    "GET /v1/events HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut event_headers = Vec::new();
        tokio::time::timeout(Duration::from_secs(1), async {
            while !event_headers.ends_with(b"\r\n\r\n") {
                event_headers.push(events.read_u8().await.unwrap());
            }
        })
        .await
        .expect("SSE response headers");
        assert!(event_headers.starts_with(b"HTTP/1.1 200 OK"));

        let body = serde_json::to_vec(&serde_json::json!({
            "logical_path": "shutdown.org",
            "org": "#+title: Shutdown\n",
        }))
        .unwrap();
        let mut mutation = tokio::net::TcpStream::connect(address).await.unwrap();
        mutation
            .write_all(
                format!(
                    "POST /v1/document-mutations HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nIdempotency-Key: shutdown-write\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        mutation.write_all(&body).await.unwrap();
        let mut mutation_response = String::new();
        mutation
            .read_to_string(&mut mutation_response)
            .await
            .unwrap();
        assert!(mutation_response.starts_with("HTTP/1.1 200 OK"));

        let mut stalled = tokio::net::TcpStream::connect(address).await.unwrap();
        stalled
            .write_all(
                b"POST /v1/document-mutations HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\n",
            )
            .await
            .unwrap();
        drop(service);
        shutdown.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(1), task)
            .await
            .expect("stalled request retained daemon beyond the HTTP drain deadline")
            .unwrap()
            .unwrap();
        drop(stalled);
        drop(events);

        let connection = rusqlite::Connection::open(database).unwrap();
        let revisions: u64 = connection
            .query_row("SELECT count(*) FROM document_revision", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(revisions, 1);
    }
}
