use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
    sync::Arc,
    time::Duration,
};

use gnosis_core::protocol::{
    DaemonConfig, DaemonServer, ErrorCode, PROTOCOL_VERSION, Request, RequestEnvelope,
    ResponseEnvelope, ResponseOutcome, ResponseResult, ServerError, request,
};
use tempfile::TempDir;
use tokio::sync::{Barrier, oneshot};

struct RunningDaemon {
    socket: PathBuf,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), gnosis_core::protocol::ServerError>>,
    _directory: TempDir,
}

impl RunningDaemon {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("gnosis.sock");
        let database = directory.path().join("gnosis.sqlite");
        let server = DaemonServer::bind(DaemonConfig::new(&socket, database))
            .await
            .unwrap();
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = receiver.await;
        }));
        Self {
            socket,
            shutdown,
            task,
            _directory: directory,
        }
    }

    async fn stop(self) {
        let socket = self.socket.clone();
        self.shutdown.send(()).unwrap();
        self.task.await.unwrap().unwrap();
        assert!(!socket.exists());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_simultaneous_clients_import_and_query_documents() {
    let daemon = RunningDaemon::start().await;
    assert_eq!(socket_mode(&daemon.socket), 0o600);
    let barrier = Arc::new(Barrier::new(4));
    let mut clients = Vec::new();
    for index in 0..3 {
        let socket = daemon.socket.clone();
        let barrier = barrier.clone();
        clients.push(tokio::spawn(async move {
            barrier.wait().await;
            let response = import(&socket, index, None).await;
            assert_imported(response, 1);
        }));
    }
    barrier.wait().await;
    for client in clients {
        client.await.unwrap();
    }
    for index in 0..3 {
        assert_document(get(&daemon.socket, index).await, index, 1);
    }
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_stale_writes_are_serialized_and_atomic() {
    let daemon = RunningDaemon::start().await;
    assert_imported(import_as(&daemon.socket, "shared", 0, None).await, 1);
    let barrier = Arc::new(Barrier::new(4));
    let mut clients = Vec::new();
    for index in 10..13 {
        let socket = daemon.socket.clone();
        let barrier = barrier.clone();
        clients.push(tokio::spawn(async move {
            barrier.wait().await;
            import_as(&socket, "shared", index, Some(1)).await
        }));
    }
    barrier.wait().await;
    let mut imported = 0;
    let mut conflicts = 0;
    for client in clients {
        match client.await.unwrap().outcome {
            ResponseOutcome::Success {
                result: ResponseResult::DocumentImported(result),
            } => {
                assert_eq!(result.revision, 2);
                imported += 1;
            }
            ResponseOutcome::Failure { error } => {
                assert_eq!(error.code, ErrorCode::Conflict);
                assert!(!error.message.contains("database is locked"));
                conflicts += 1;
            }
            other => panic!("unexpected response: {other:?}"),
        }
    }
    assert_eq!((imported, conflicts), (1, 2));
    let response = get_as(&daemon.socket, "shared", "get-shared").await;
    assert_document_revision(response, 2);
    daemon.stop().await;
}

#[tokio::test]
async fn unsupported_protocol_version_is_rejected_without_internal_details() {
    let daemon = RunningDaemon::start().await;
    let response = call(
        &daemon.socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION + 1,
            id: "wrong-version".into(),
            request: Request::SystemInfo {},
        },
    )
    .await;
    let ResponseOutcome::Failure { error } = response.outcome else {
        panic!("unsupported version unexpectedly succeeded");
    };
    assert_eq!(error.code, ErrorCode::UnsupportedVersion);
    assert!(!error.message.contains("sqlite"));
    assert!(!error.message.contains('/'));
    daemon.stop().await;
}

#[tokio::test]
async fn socket_setup_preserves_files_and_replaces_only_stale_sockets() {
    let directory = tempfile::tempdir().unwrap();
    let occupied = directory.path().join("occupied");
    fs::write(&occupied, "keep").unwrap();
    let result = DaemonServer::bind(DaemonConfig::new(
        &occupied,
        directory.path().join("occupied.sqlite"),
    ))
    .await;
    let Err(error) = result else {
        panic!("daemon replaced a non-socket file");
    };
    assert!(matches!(error, ServerError::SocketPathOccupied));
    assert_eq!(fs::read_to_string(&occupied).unwrap(), "keep");

    let stale = directory.path().join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).unwrap());
    wait_for_stale_socket(&stale).await;
    let server = DaemonServer::bind(DaemonConfig::new(
        &stale,
        directory.path().join("stale.sqlite"),
    ))
    .await
    .unwrap();
    let (shutdown, receiver) = oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = receiver.await;
    }));
    shutdown.send(()).unwrap();
    task.await.unwrap().unwrap();
    assert!(!stale.exists());
}

#[tokio::test]
async fn second_daemon_cannot_remove_a_live_socket() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("gnosis.sock");
    let first = DaemonServer::bind(DaemonConfig::new(
        &socket,
        directory.path().join("first.sqlite"),
    ))
    .await
    .unwrap();
    let result = DaemonServer::bind(DaemonConfig::new(
        &socket,
        directory.path().join("second.sqlite"),
    ))
    .await;
    let Err(error) = result else {
        panic!("second daemon unexpectedly bound live socket");
    };
    assert!(matches!(error, ServerError::AlreadyRunning));
    assert!(socket.exists());
    first.run(async {}).await.unwrap();
    assert!(!socket.exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn diagnostic_client_exits_nonzero_with_concise_protocol_error() {
    let daemon = RunningDaemon::start().await;
    let executable = env!("CARGO_BIN_EXE_gnosis");
    let socket = daemon.socket.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(executable)
            .args([
                "--socket",
                socket.to_str().unwrap(),
                "get-document",
                "missing",
            ])
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "not_found: document not found\n"
    );
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn projection_too_large_for_protocol_is_rejected_before_commit() {
    let daemon = RunningDaemon::start().await;
    let response = call(
        &daemon.socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "large-import".into(),
            request: Request::DocumentImport {
                document_id: "large".into(),
                org: deeply_nested_org(),
                expected_revision: None,
            },
        },
    )
    .await;
    let ResponseOutcome::Failure { error } = response.outcome else {
        panic!("oversized projection unexpectedly imported");
    };
    assert_eq!(error.code, ErrorCode::InvalidDocument);
    let response = get_as(&daemon.socket, "large", "get-large").await;
    let ResponseOutcome::Failure { error } = response.outcome else {
        panic!("rejected projection was stored");
    };
    assert_eq!(error.code, ErrorCode::NotFound);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sigterm_drains_daemon_and_removes_socket() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("process.sock");
    let database = directory.path().join("process.sqlite");
    let mut child = Command::new(env!("CARGO_BIN_EXE_gnosisd"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--database",
            database.to_str().unwrap(),
        ])
        .spawn()
        .unwrap();
    wait_for_daemon(&socket).await;
    assert_eq!(
        fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    let status = tokio::task::spawn_blocking(move || child.wait().unwrap())
        .await
        .unwrap();
    assert!(status.success());
    assert!(!socket.exists());
}

async fn import(socket: &Path, index: usize, revision: Option<u64>) -> ResponseEnvelope {
    import_as(socket, &format!("doc-{index}"), index, revision).await
}

async fn import_as(
    socket: &Path,
    document_id: &str,
    index: usize,
    revision: Option<u64>,
) -> ResponseEnvelope {
    call(
        socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: format!("import-{index}"),
            request: Request::DocumentImport {
                document_id: document_id.into(),
                org: org_document(index),
                expected_revision: revision,
            },
        },
    )
    .await
}

async fn get(socket: &Path, index: usize) -> ResponseEnvelope {
    get_as(socket, &format!("doc-{index}"), &format!("get-{index}")).await
}

async fn get_as(socket: &Path, document_id: &str, request_id: &str) -> ResponseEnvelope {
    call(
        socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: request_id.into(),
            request: Request::DocumentGet {
                document_id: document_id.into(),
            },
        },
    )
    .await
}

async fn call(socket: &Path, envelope: RequestEnvelope) -> ResponseEnvelope {
    request(socket, &envelope).await.unwrap()
}

async fn wait_for_daemon(socket: &Path) {
    for attempt in 0..200 {
        let envelope = RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: format!("ready-{attempt}"),
            request: Request::SystemInfo {},
        };
        if request(socket, &envelope).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon did not become ready");
}

async fn wait_for_stale_socket(socket: &Path) {
    for _ in 0..100 {
        match std::os::unix::net::UnixStream::connect(socket) {
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => return,
            Err(error) => panic!("unexpected stale-socket probe error: {error}"),
            Ok(stream) => drop(stream),
        }
        tokio::task::yield_now().await;
    }
    panic!("socket listener did not close");
}

fn org_document(index: usize) -> String {
    format!(
        ":PROPERTIES:\n:ID: root-{index}\n:END:\n#+TITLE: Document {index}\n\nLink [[id:target-{index}]].\n"
    )
}

fn deeply_nested_org() -> String {
    let mut org = String::from(":PROPERTIES:\n:ID: large-root\n:END:\n#+TITLE: Large\n");
    for level in 1..=300 {
        let title = format!("{level}-{}", "x".repeat(200));
        org.push_str(&format!(
            "{} {title}\n:PROPERTIES:\n:ID: large-{level}\n:END:\n",
            "*".repeat(level)
        ));
    }
    org
}

fn assert_imported(response: ResponseEnvelope, revision: u64) {
    let ResponseOutcome::Success {
        result: ResponseResult::DocumentImported(result),
    } = response.outcome
    else {
        panic!("import failed: {:?}", response.outcome);
    };
    assert_eq!(result.revision, revision);
    assert_eq!(result.node_count, 1);
    assert_eq!(result.occurrence_count, 1);
    assert_eq!(result.semantic_edge_count, 1);
}

fn assert_document(response: ResponseEnvelope, index: usize, revision: u64) {
    let ResponseOutcome::Success {
        result: ResponseResult::Document(document),
    } = response.outcome
    else {
        panic!("query failed: {:?}", response.outcome);
    };
    assert_eq!(document.document_id, format!("doc-{index}"));
    assert_eq!(document.revision, revision);
    assert_eq!(document.title, format!("Document {index}"));
}

fn assert_document_revision(response: ResponseEnvelope, revision: u64) {
    let ResponseOutcome::Success {
        result: ResponseResult::Document(document),
    } = response.outcome
    else {
        panic!("query failed: {:?}", response.outcome);
    };
    assert_eq!(document.revision, revision);
    assert_eq!(document.nodes.len(), 1);
    assert_eq!(document.link_occurrences.len(), 1);
    assert_eq!(document.semantic_edges.len(), 1);
}

fn socket_mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}
