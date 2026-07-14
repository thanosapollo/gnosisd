use std::{
    fs,
    future::Future,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::mpsc::{self, SyncSender, TrySendError},
    thread,
    time::Duration,
};

use thiserror::Error;
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{Semaphore, oneshot, watch},
    task::JoinSet,
    time::timeout,
};

use crate::{
    application::{
        ApplicationError, ImportDocument, ImportSummary, LegacyQueryError, PreparedImport,
        QueryDocument, QueryDueThemata, QueryLegacyThema, persist_import,
    },
    domain::{DueThema, LegacyDatabaseInfo, LegacyThema, ThemaId},
    protocol::{
        DocumentView, DueThemaView, ErrorCode, ImportResult, LegacyInfoView,
        MessageValidationError, PROTOCOL_VERSION, ProtocolError, Request, RequestEnvelope,
        ResponseEnvelope, ResponseResult, SystemInfo, ThemaView, frame,
    },
    store::{SqliteStore, StoreError, StoredDocument},
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
    pub max_connections: usize,
    pub store_queue_capacity: usize,
}

impl DaemonConfig {
    pub fn new(socket_path: impl Into<PathBuf>, database_path: impl Into<PathBuf>) -> Self {
        Self {
            socket_path: socket_path.into(),
            database_path: database_path.into(),
            max_connections: DEFAULT_MAX_CONNECTIONS,
            store_queue_capacity: DEFAULT_STORE_QUEUE_CAPACITY,
        }
    }
}

pub struct DaemonServer {
    listener: UnixListener,
    socket: SocketGuard,
    actor: StoreActor,
    max_connections: usize,
}

impl DaemonServer {
    pub async fn bind(config: DaemonConfig) -> Result<Self, ServerError> {
        validate_config(&config)?;
        let _startup_lock = StartupLock::acquire(&config.socket_path)?;
        prepare_socket(&config.socket_path).await?;
        let listener = UnixListener::bind(&config.socket_path).map_err(ServerError::Socket)?;
        let socket = SocketGuard::new(config.socket_path.clone())?;
        fs::set_permissions(&config.socket_path, fs::Permissions::from_mode(0o600))
            .map_err(ServerError::Socket)?;
        let actor = StoreActor::start(config.database_path, config.store_queue_capacity)
            .await
            .map_err(ServerError::store)?;
        Ok(Self {
            listener,
            socket,
            actor,
            max_connections: config.max_connections,
        })
    }

    pub async fn run(self, shutdown: impl Future<Output = ()>) -> Result<(), ServerError> {
        let Self {
            listener,
            socket,
            actor,
            max_connections,
        } = self;
        let state = ServerState {
            store: actor.handle(),
        };
        let semaphore = std::sync::Arc::new(Semaphore::new(max_connections));
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
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
        drop(state);
        actor.finish().await?;
        drop(socket);
        accept_error
    }
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
        Request::DocumentImport {
            document_id,
            org,
            expected_revision,
        } => import_document(state, document_id, org, expected_revision).await,
        Request::DocumentGet { document_id } => get_document(state, document_id).await,
        Request::LegacyInfo {} => legacy_info(state).await,
        Request::ThemaGet { thema_id } => get_thema(state, thema_id).await,
        Request::ReviewDue { day, limit } => due_themata(state, day, limit).await,
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

async fn legacy_info(state: ServerState) -> Result<ResponseResult, ProtocolError> {
    let info = state.store.legacy_info().await.map_err(actor_error)?;
    Ok(ResponseResult::LegacyInfo(LegacyInfoView::from(info)))
}

async fn get_thema(state: ServerState, thema_id: String) -> Result<ResponseResult, ProtocolError> {
    let query = QueryLegacyThema::new(thema_id).map_err(invalid_legacy_query)?;
    let thema = state
        .store
        .get_thema(query.thema_id().clone())
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

async fn import_document(
    state: ServerState,
    document_id: String,
    org: String,
    expected_revision: Option<u64>,
) -> Result<ResponseResult, ProtocolError> {
    let command =
        ImportDocument::new(document_id, org, expected_revision).map_err(|_| invalid_document())?;
    let prepared = tokio::task::spawn_blocking(move || command.prepare())
        .await
        .map_err(|_| internal_error())?
        .map_err(|_| invalid_document())?;
    let prepared = tokio::task::spawn_blocking(move || {
        validate_response_size(&prepared)?;
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

fn validation_response(id: String, error: MessageValidationError) -> ResponseEnvelope {
    let error = match error {
        MessageValidationError::UnsupportedVersion(_) => ProtocolError::new(
            ErrorCode::UnsupportedVersion,
            "unsupported protocol version",
        ),
        MessageValidationError::InvalidRequestId => {
            ProtocolError::new(ErrorCode::MalformedRequest, "invalid request ID")
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

fn invalid_legacy_query(_error: LegacyQueryError) -> ProtocolError {
    ProtocolError::new(ErrorCode::MalformedRequest, "invalid legacy query")
}

fn internal_error() -> ProtocolError {
    ProtocolError::new(ErrorCode::Internal, "internal daemon failure")
}

fn actor_error(error: ActorError) -> ProtocolError {
    match error {
        ActorError::Busy => ProtocolError::new(ErrorCode::Busy, "daemon is busy"),
        ActorError::Application(ApplicationError::Conflict { .. }) => {
            ProtocolError::new(ErrorCode::Conflict, "document revision conflict")
        }
        ActorError::Stopped | ActorError::Application(_) | ActorError::Store(_) => internal_error(),
    }
}

#[derive(Clone)]
struct ServerState {
    store: StoreHandle,
}

enum StoreCommand {
    Replace {
        prepared: Box<PreparedImport>,
        response: oneshot::Sender<Result<ImportSummary, ApplicationError>>,
    },
    Get {
        document_id: crate::domain::DocumentId,
        response: oneshot::Sender<Result<Option<StoredDocument>, StoreError>>,
    },
    LegacyInfo {
        response: oneshot::Sender<Result<Option<LegacyDatabaseInfo>, StoreError>>,
    },
    GetThema {
        thema_id: ThemaId,
        response: oneshot::Sender<Result<Option<LegacyThema>, StoreError>>,
    },
    DueThemata {
        query: QueryDueThemata,
        response: oneshot::Sender<Result<Vec<DueThema>, StoreError>>,
    },
}

#[derive(Clone)]
struct StoreHandle {
    sender: SyncSender<StoreCommand>,
}

impl StoreHandle {
    async fn replace(&self, prepared: PreparedImport) -> Result<ImportSummary, ActorError> {
        let (sender, receiver) = oneshot::channel();
        enqueue(
            &self.sender,
            StoreCommand::Replace {
                prepared: Box::new(prepared),
                response: sender,
            },
        )?;
        receiver
            .await
            .map_err(|_| ActorError::Stopped)?
            .map_err(ActorError::Application)
    }

    async fn get(
        &self,
        document_id: crate::domain::DocumentId,
    ) -> Result<Option<StoredDocument>, ActorError> {
        let (sender, receiver) = oneshot::channel();
        enqueue(
            &self.sender,
            StoreCommand::Get {
                document_id,
                response: sender,
            },
        )?;
        receiver
            .await
            .map_err(|_| ActorError::Stopped)?
            .map_err(ActorError::Store)
    }

    async fn legacy_info(&self) -> Result<Option<LegacyDatabaseInfo>, ActorError> {
        let (sender, receiver) = oneshot::channel();
        enqueue(&self.sender, StoreCommand::LegacyInfo { response: sender })?;
        receiver
            .await
            .map_err(|_| ActorError::Stopped)?
            .map_err(ActorError::Store)
    }

    async fn get_thema(&self, thema_id: ThemaId) -> Result<Option<LegacyThema>, ActorError> {
        let (sender, receiver) = oneshot::channel();
        enqueue(
            &self.sender,
            StoreCommand::GetThema {
                thema_id,
                response: sender,
            },
        )?;
        receiver
            .await
            .map_err(|_| ActorError::Stopped)?
            .map_err(ActorError::Store)
    }

    async fn due_themata(&self, query: QueryDueThemata) -> Result<Vec<DueThema>, ActorError> {
        let (sender, receiver) = oneshot::channel();
        enqueue(
            &self.sender,
            StoreCommand::DueThemata {
                query,
                response: sender,
            },
        )?;
        receiver
            .await
            .map_err(|_| ActorError::Stopped)?
            .map_err(ActorError::Store)
    }
}

struct StoreActor {
    handle: StoreHandle,
    join: thread::JoinHandle<()>,
}

impl StoreActor {
    async fn start(path: PathBuf, capacity: usize) -> Result<Self, ActorStartError> {
        tokio::task::spawn_blocking(move || Self::start_blocking(path, capacity))
            .await
            .map_err(|_| ActorStartError::Stopped)?
    }

    fn start_blocking(path: PathBuf, capacity: usize) -> Result<Self, ActorStartError> {
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let join = thread::Builder::new()
            .name("gnosis-sqlite".into())
            .spawn(move || store_actor(path, receiver, ready_sender))
            .map_err(ActorStartError::Thread)?;
        ready_receiver
            .recv()
            .map_err(|_| ActorStartError::Stopped)?
            .map_err(ActorStartError::Store)?;
        Ok(Self {
            handle: StoreHandle { sender },
            join,
        })
    }

    fn handle(&self) -> StoreHandle {
        self.handle.clone()
    }

    async fn finish(self) -> Result<(), ServerError> {
        drop(self.handle);
        tokio::task::spawn_blocking(move || self.join.join())
            .await
            .map_err(|_| ServerError::ActorStopped)?
            .map_err(|_| ServerError::ActorStopped)
    }
}

fn store_actor(
    path: PathBuf,
    receiver: mpsc::Receiver<StoreCommand>,
    ready: SyncSender<Result<(), StoreError>>,
) {
    let mut store = match SqliteStore::open(path) {
        Ok(store) => store,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    if ready.send(Ok(())).is_err() {
        return;
    }
    for command in receiver {
        run_store_command(&mut store, command);
    }
}

fn run_store_command(store: &mut SqliteStore, command: StoreCommand) {
    match command {
        StoreCommand::Replace { prepared, response } => {
            let result = persist_import(store, &prepared);
            let _ = response.send(result);
        }
        StoreCommand::Get {
            document_id,
            response,
        } => {
            let _ = response.send(store.get_document(&document_id));
        }
        StoreCommand::LegacyInfo { response } => {
            let _ = response.send(store.legacy_database_info());
        }
        StoreCommand::GetThema { thema_id, response } => {
            let _ = response.send(store.get_legacy_thema(&thema_id));
        }
        StoreCommand::DueThemata { query, response } => {
            let _ = response.send(store.due_legacy_themata(query.day(), query.limit()));
        }
    }
}

fn enqueue(sender: &SyncSender<StoreCommand>, command: StoreCommand) -> Result<(), ActorError> {
    match sender.try_send(command) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(ActorError::Busy),
        Err(TrySendError::Disconnected(_)) => Err(ActorError::Stopped),
    }
}

#[derive(Debug, Error)]
enum ActorError {
    #[error("store queue is full")]
    Busy,
    #[error("store actor stopped")]
    Stopped,
    #[error("store operation failed")]
    Store(#[source] StoreError),
    #[error("application operation failed")]
    Application(#[source] ApplicationError),
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
    #[error("database initialization failed")]
    Store(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("could not start database owner thread")]
    ActorStart(#[source] std::io::Error),
    #[error("database owner thread stopped unexpectedly")]
    ActorStopped,
}

impl ServerError {
    fn store(error: ActorStartError) -> Self {
        match error {
            ActorStartError::Store(error) => Self::Store(Box::new(error)),
            ActorStartError::Thread(error) => Self::ActorStart(error),
            ActorStartError::Stopped => Self::ActorStopped,
        }
    }
}

#[derive(Debug)]
enum ActorStartError {
    Store(StoreError),
    Thread(std::io::Error),
    Stopped,
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::net::UnixListener as StdUnixListener, sync::mpsc};

    use tokio::sync::oneshot;

    use super::{
        ActorError, ServerError, ServerState, StartupLock, StoreCommand, StoreHandle,
        bounded_success, dispatch, enqueue, remove_observed_socket,
    };
    use crate::{
        application::ImportDocument,
        domain::DocumentId,
        protocol::{
            DueThemaView, ErrorCode, Request, RequestEnvelope, ResponseOutcome, ResponseResult,
        },
    };

    #[test]
    fn full_store_queue_reports_busy() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        let command = get_command("first");
        enqueue(&sender, command).unwrap();
        let error = enqueue(&sender, get_command("second")).unwrap_err();
        assert!(matches!(error, ActorError::Busy));
    }

    #[test]
    fn busy_actor_error_becomes_stable_protocol_error() {
        let error = super::actor_error(ActorError::Busy);
        assert_eq!(error.code, crate::protocol::ErrorCode::Busy);
        assert_eq!(error.message, "daemon is busy");
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

    #[tokio::test]
    async fn full_store_queue_returns_busy_to_application_request() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        enqueue(&sender, get_command("queued")).unwrap();
        let state = ServerState {
            store: StoreHandle { sender },
        };
        let response = dispatch(
            RequestEnvelope {
                version: 1,
                id: "busy-request".into(),
                request: Request::DocumentGet {
                    document_id: "document".into(),
                },
            },
            state,
        )
        .await;
        let ResponseOutcome::Failure { error } = response.outcome else {
            panic!("full queue unexpectedly accepted request");
        };
        assert_eq!(error.code, ErrorCode::Busy);
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
    async fn accepted_write_finishes_after_response_is_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("drain.sqlite");
        let actor = super::StoreActor::start(path.clone(), 1).await.unwrap();
        let handle = actor.handle();
        let prepared = ImportDocument::new("document", "#+TITLE: Drained\n".into(), None)
            .unwrap()
            .prepare()
            .unwrap();
        let (response, receiver) = oneshot::channel();
        enqueue(
            &handle.sender,
            StoreCommand::Replace {
                prepared: Box::new(prepared),
                response,
            },
        )
        .unwrap();
        drop(receiver);
        drop(handle);
        actor.finish().await.unwrap();

        let store = crate::store::SqliteStore::open(path).unwrap();
        let id = DocumentId::new("document").unwrap();
        assert!(store.get_document(&id).unwrap().is_some());
    }

    fn get_command(id: &str) -> StoreCommand {
        let (response, _) = oneshot::channel();
        StoreCommand::Get {
            document_id: DocumentId::new(id).unwrap(),
            response,
        }
    }
}
