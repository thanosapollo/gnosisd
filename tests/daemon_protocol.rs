use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    sync::Arc,
    time::Duration,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use gnosis_core::migration::{migrate_legacy_database, migrate_legacy_server};
use gnosis_core::protocol::{
    DaemonConfig, DaemonServer, ErrorCode, MAX_ASSET_CHUNK_BYTES, PROTOCOL_VERSION, Request,
    RequestEnvelope, ResponseEnvelope, ResponseOutcome, ResponseResult, ReviewAnswerInput,
    ReviewSourceInput, ReviewSourceKindInput, ServerError, request,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use tempfile::TempDir;
use tokio::sync::{Barrier, oneshot};

struct RunningDaemon {
    socket: PathBuf,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), gnosis_core::protocol::ServerError>>,
    _directory: TempDir,
}

struct DaemonProcess {
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), gnosis_core::protocol::ServerError>>,
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn id(&self) -> u32 {
        self.child.as_ref().expect("child already reaped").id()
    }

    async fn wait_for_exit(mut self) -> ExitStatus {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match self.try_reap() {
                    Ok(Some(status)) => return status,
                    Ok(None) => tokio::time::sleep(Duration::from_millis(10)).await,
                    Err(error) => panic!("failed to reap daemon: {error}"),
                }
            }
        })
        .await
        .expect("daemon did not exit after SIGTERM")
    }

    fn try_reap(&mut self) -> std::io::Result<Option<ExitStatus>> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = child.try_wait()?;
        if status.is_some() {
            self.child = None;
        }
        Ok(status)
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        let _ = child.kill();
        for _ in 0..100 {
            match self.try_reap() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
}

impl DaemonProcess {
    async fn start(socket: &Path, database: &Path) -> Self {
        let server = DaemonServer::bind(DaemonConfig::new(socket, database))
            .await
            .unwrap();
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = receiver.await;
        }));
        Self { shutdown, task }
    }

    async fn stop(self) {
        self.shutdown.send(()).unwrap();
        self.task.await.unwrap().unwrap();
    }
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

    async fn start_review_fixture() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("legacy.db");
        let socket = directory.path().join("gnosis.sock");
        let database = directory.path().join("gnosis.sqlite");
        create_review_fixture(&source);
        migrate_legacy_database(&source, &database).unwrap();
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

    async fn start_asset_fixture() -> (Self, String, String, Vec<u8>) {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("legacy.db");
        let notes = directory.path().join("Notes");
        let socket = directory.path().join("gnosis.sock");
        let database = directory.path().join("gnosis.sqlite");
        let blobs = database.with_extension("blobs");
        let bytes = (0..1024 * 1024)
            .map(|index| u8::try_from(index % 251).unwrap())
            .collect::<Vec<_>>();
        let tombstoned = b"tombstoned asset";
        let asset_path = canonical_test_asset_path(&bytes, "large.png");
        let tombstoned_path = canonical_test_asset_path(tombstoned, "old.png");
        fs::create_dir_all(notes.join("Pictures")).unwrap();
        fs::write(notes.join("Pictures/large.png"), &bytes).unwrap();
        fs::write(notes.join("Pictures/old.png"), tombstoned).unwrap();
        fs::write(
            notes.join("note.org"),
            "#+TITLE: Assets\n[[file:Pictures/large.png]]\n[[file:Pictures/old.png]]\n",
        )
        .unwrap();
        create_review_fixture(&source);
        migrate_legacy_server(&source, &database, &notes, &blobs).unwrap();
        Connection::open(&database)
            .unwrap()
            .execute(
                "UPDATE managed_asset SET active = 0 WHERE asset_path = ?1",
                [&tombstoned_path],
            )
            .unwrap();
        let server = DaemonServer::bind(DaemonConfig::new(&socket, database))
            .await
            .unwrap();
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = receiver.await;
        }));
        (
            Self {
                socket,
                shutdown,
                task,
                _directory: directory,
            },
            asset_path,
            tombstoned_path,
            bytes,
        )
    }

    async fn stop(self) {
        let socket = self.socket.clone();
        self.shutdown.send(()).unwrap();
        self.task.await.unwrap().unwrap();
        assert!(!socket.exists());
    }
}

#[tokio::test]
async fn managed_assets_are_served_as_bounded_verified_chunks() {
    let (daemon, logical_path, tombstoned_path, bytes) = RunningDaemon::start_asset_fixture().await;
    let first = asset_chunk(&daemon.socket, "asset-first", &logical_path, 0, 17).await;
    let retry = asset_chunk(&daemon.socket, "asset-retry", &logical_path, 0, 17).await;
    assert_eq!(first, retry);
    assert_eq!(decode_chunk(&first), bytes[..17]);
    assert!(!first.eof);

    let middle = asset_chunk(&daemon.socket, "asset-middle", &logical_path, 401, 29).await;
    assert_eq!(decode_chunk(&middle), bytes[401..430]);
    assert_eq!(middle.offset, 401);
    assert!(!middle.eof);

    let maximum = asset_chunk(
        &daemon.socket,
        "asset-maximum",
        &logical_path,
        0,
        MAX_ASSET_CHUNK_BYTES,
    )
    .await;
    assert_eq!(
        decode_chunk(&maximum),
        bytes[..MAX_ASSET_CHUNK_BYTES as usize]
    );
    assert!(!maximum.eof);
    let final_chunk = asset_chunk(
        &daemon.socket,
        "asset-final",
        &logical_path,
        u64::from(MAX_ASSET_CHUNK_BYTES),
        MAX_ASSET_CHUNK_BYTES,
    )
    .await;
    assert_eq!(
        decode_chunk(&final_chunk),
        bytes[MAX_ASSET_CHUNK_BYTES as usize..]
    );
    assert!(final_chunk.eof);

    let terminal = asset_chunk(
        &daemon.socket,
        "asset-terminal",
        &logical_path,
        bytes.len() as u64,
        1,
    )
    .await;
    assert!(decode_chunk(&terminal).is_empty());
    assert!(terminal.eof);
    assert_eq!(terminal.total_size, bytes.len() as u64);
    assert_eq!(
        terminal.content_sha256,
        format!("{:x}", Sha256::digest(&bytes))
    );
    assert_eq!(terminal.logical_path, logical_path);

    assert_asset_error(
        &daemon.socket,
        "asset-past-eof",
        &logical_path,
        bytes.len() as u64 + 1,
        1,
        ErrorCode::MalformedRequest,
    )
    .await;
    assert_asset_error(
        &daemon.socket,
        "asset-wrong-digest",
        &format!("assets/{}/large.png", "0".repeat(64)),
        0,
        1,
        ErrorCode::NotFound,
    )
    .await;
    assert_asset_error(
        &daemon.socket,
        "asset-tombstoned",
        &tombstoned_path,
        0,
        1,
        ErrorCode::NotFound,
    )
    .await;
    assert_asset_error(
        &daemon.socket,
        "asset-invalid-path",
        "../../large.png",
        0,
        1,
        ErrorCode::MalformedRequest,
    )
    .await;
    daemon.stop().await;
}

#[tokio::test]
async fn online_review_commit_is_idempotent_after_lost_response() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let database = directory.path().join("gnosis.sqlite");
    let socket = directory.path().join("gnosis.sock");
    create_review_fixture(&source);
    migrate_legacy_database(&source, &database).unwrap();
    let daemon = DaemonProcess::start(&socket, &database).await;
    let session = successful_review_result(
        &socket,
        "session",
        Request::ReviewSessionCreate {
            source: manual_review_source(),
            target_count: None,
        },
    )
    .await;
    let ResponseResult::ReviewSession(session) = session else {
        panic!("unexpected session response");
    };
    let attempt = successful_review_result(
        &socket,
        "begin",
        Request::ReviewAttemptBegin {
            session_id: session.session_id,
        },
    )
    .await;
    let ResponseResult::ReviewAttempt(attempt) = attempt else {
        panic!("unexpected attempt response");
    };
    assert_eq!(attempt.presentation.answer, ["Answer"]);
    let attempt_id = attempt.attempt_id;
    let graded = successful_review_result(
        &socket,
        "answer",
        Request::ReviewAttemptAnswer {
            attempt_id: attempt_id.clone(),
            response: ReviewAnswerInput::Text("Answer".into()),
        },
    )
    .await;
    let ResponseResult::ReviewAttempt(graded) = graded else {
        panic!("unexpected grade response");
    };
    assert_eq!(graded.detected_success, Some(true));
    for id in ["override-1", "override-2"] {
        successful_review_result(
            &socket,
            id,
            Request::ReviewAttemptOverride {
                attempt_id: attempt_id.clone(),
            },
        )
        .await;
    }
    for id in ["suspend-1", "suspend-2"] {
        successful_review_result(
            &socket,
            id,
            Request::ReviewAttemptSuspensionToggle {
                attempt_id: attempt_id.clone(),
            },
        )
        .await;
    }
    let first = successful_review_result(
        &socket,
        "commit-1",
        Request::ReviewAttemptCommit {
            attempt_id: attempt_id.clone(),
        },
    )
    .await;
    let retry = successful_review_result(
        &socket,
        "commit-2",
        Request::ReviewAttemptCommit {
            attempt_id: attempt_id.clone(),
        },
    )
    .await;
    assert_eq!(first, retry);
    daemon.stop().await;
    let connection = Connection::open(&database).unwrap();
    let event_count: u64 = connection
        .query_row(
            "SELECT count(*) FROM review_event WHERE attempt_id = ?1",
            [&attempt_id],
            |row| row.get(0),
        )
        .unwrap();
    let transition_count: u64 = connection
        .query_row(
            "SELECT count(*) FROM review_state_revision
             WHERE thema_id = '7' AND state_revision = 2",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_count, 1);
    assert_eq!(transition_count, 1);
}

#[tokio::test]
async fn online_review_session_recovers_and_has_explicit_lifecycle() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let database = directory.path().join("gnosis.sqlite");
    let socket = directory.path().join("gnosis.sock");
    create_review_fixture(&source);
    migrate_legacy_database(&source, &database).unwrap();
    let daemon = DaemonProcess::start(&socket, &database).await;
    let session = successful_review_result(
        &socket,
        "create",
        Request::ReviewSessionCreate {
            source: manual_review_source(),
            target_count: None,
        },
    )
    .await;
    let ResponseResult::ReviewSession(session) = session else {
        panic!("unexpected session response");
    };
    let session_id = session.session_id;
    successful_review_result(
        &socket,
        "begin",
        Request::ReviewAttemptBegin {
            session_id: session_id.clone(),
        },
    )
    .await;
    let second = call(
        &socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "second".into(),
            request: Request::ReviewSessionCreate {
                source: manual_review_source(),
                target_count: None,
            },
        },
    )
    .await;
    let ResponseOutcome::Failure { error } = second.outcome else {
        panic!("second active session unexpectedly succeeded");
    };
    assert_eq!(error.code, ErrorCode::Conflict);
    daemon.stop().await;

    let daemon = DaemonProcess::start(&socket, &database).await;
    let active = successful_review_result(&socket, "active", Request::ReviewSessionActive {}).await;
    let ResponseResult::ActiveReviewSession(active) = active else {
        panic!("unexpected active-session response");
    };
    assert_eq!(active.session.as_ref().unwrap().session_id, session_id);
    successful_review_result(
        &socket,
        "resume-active",
        Request::ReviewSessionResume {
            session_id: session_id.clone(),
        },
    )
    .await;
    let quit = successful_review_result(
        &socket,
        "quit",
        Request::ReviewSessionQuit {
            session_id: session_id.clone(),
        },
    )
    .await;
    let ResponseResult::ReviewSession(quit) = quit else {
        panic!("unexpected quit response");
    };
    assert_eq!(quit.status, "quit");
    successful_review_result(
        &socket,
        "resume-quit",
        Request::ReviewSessionResume {
            session_id: session_id.clone(),
        },
    )
    .await;
    let aborted =
        successful_review_result(&socket, "abort", Request::ReviewSessionAbort { session_id })
            .await;
    let ResponseResult::ReviewSession(aborted) = aborted else {
        panic!("unexpected abort response");
    };
    assert_eq!(aborted.status, "aborted");
    let none =
        successful_review_result(&socket, "active-none", Request::ReviewSessionActive {}).await;
    let ResponseResult::ActiveReviewSession(none) = none else {
        panic!("unexpected empty active-session response");
    };
    assert_eq!(none.session, None);
    daemon.stop().await;
}

#[tokio::test]
async fn unix_review_begin_preserves_raw_compatibility_presentation() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let database = directory.path().join("gnosis.sqlite");
    let socket = directory.path().join("gnosis.sock");
    create_review_fixture(&source);
    Connection::open(&source)
        .unwrap()
        .execute(
            "UPDATE themata SET keimenon = ?1 WHERE id = 7",
            [r#""Question Answer""#],
        )
        .unwrap();
    migrate_legacy_database(&source, &database).unwrap();
    let daemon = DaemonProcess::start(&socket, &database).await;

    let session = successful_review_result(
        &socket,
        "unsafe-local-session",
        Request::ReviewSessionCreate {
            source: manual_review_source(),
            target_count: None,
        },
    )
    .await;
    let ResponseResult::ReviewSession(session) = session else {
        panic!("unexpected session response");
    };
    let attempt = successful_review_result(
        &socket,
        "unsafe-local-attempt",
        Request::ReviewAttemptBegin {
            session_id: session.session_id,
        },
    )
    .await;
    let attempt = review_attempt_result(attempt);

    assert_eq!(attempt.presentation.keimenon, "Question Answer");
    assert_eq!(attempt.presentation.answer, vec!["Answer"]);
    daemon.stop().await;
}

#[tokio::test]
async fn online_review_toggle_retries_are_persistently_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let database = directory.path().join("gnosis.sqlite");
    let socket = directory.path().join("gnosis.sock");
    create_review_fixture(&source);
    migrate_legacy_database(&source, &database).unwrap();
    let daemon = DaemonProcess::start(&socket, &database).await;
    let attempt_id = begin_and_answer_review(&socket).await;
    let first = review_attempt_result(
        successful_review_result(
            &socket,
            "override-retry",
            Request::ReviewAttemptOverride {
                attempt_id: attempt_id.clone(),
            },
        )
        .await,
    );
    daemon.stop().await;

    let daemon = DaemonProcess::start(&socket, &database).await;
    let retry = review_attempt_result(
        successful_review_result(
            &socket,
            "override-retry",
            Request::ReviewAttemptOverride {
                attempt_id: attempt_id.clone(),
            },
        )
        .await,
    );
    assert_eq!(retry, first);
    let intentional_second = review_attempt_result(
        successful_review_result(
            &socket,
            "override-new",
            Request::ReviewAttemptOverride {
                attempt_id: attempt_id.clone(),
            },
        )
        .await,
    );
    assert_eq!(intentional_second.override_count, first.override_count + 1);
    assert_ne!(intentional_second.accepted_success, first.accepted_success);

    let suspended = review_attempt_result(
        successful_review_result(
            &socket,
            "suspension-retry",
            Request::ReviewAttemptSuspensionToggle {
                attempt_id: attempt_id.clone(),
            },
        )
        .await,
    );
    let suspension_retry = review_attempt_result(
        successful_review_result(
            &socket,
            "suspension-retry",
            Request::ReviewAttemptSuspensionToggle {
                attempt_id: attempt_id.clone(),
            },
        )
        .await,
    );
    assert_eq!(suspension_retry, suspended);
    let suspension_second = review_attempt_result(
        successful_review_result(
            &socket,
            "suspension-new",
            Request::ReviewAttemptSuspensionToggle {
                attempt_id: attempt_id.clone(),
            },
        )
        .await,
    );
    assert_ne!(
        suspension_second.desired_suspended,
        suspended.desired_suspended
    );

    let reused = call(
        &socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "override-retry".into(),
            request: Request::ReviewAttemptSuspensionToggle { attempt_id },
        },
    )
    .await;
    let ResponseOutcome::Failure { error } = reused.outcome else {
        panic!("request ID reuse with another payload unexpectedly succeeded");
    };
    assert_eq!(error.code, ErrorCode::Conflict);
    daemon.stop().await;
}

#[tokio::test]
async fn desired_review_operation_survives_restart_with_new_correlation_id() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let database = directory.path().join("gnosis.sqlite");
    let socket = directory.path().join("gnosis.sock");
    create_review_fixture(&source);
    migrate_legacy_database(&source, &database).unwrap();
    let daemon = DaemonProcess::start(&socket, &database).await;
    let attempt_id = begin_and_answer_review(&socket).await;
    let operation = Request::ReviewAttemptAcceptanceSetV2 {
        operation_version: 1,
        operation_id: "durable-operation".into(),
        attempt_id: attempt_id.clone(),
        expected_attempt_revision: 1,
        accepted_success: false,
    };
    let first = review_attempt_mutation_v2(
        successful_review_result(&socket, "correlation-before", operation.clone()).await,
    );
    assert_eq!(first.attempt_revision, 2);
    assert_eq!(first.attempt.accepted_success, Some(false));
    daemon.stop().await;

    let daemon = DaemonProcess::start(&socket, &database).await;
    let replay = review_attempt_mutation_v2(
        successful_review_result(&socket, "correlation-after", operation).await,
    );
    assert_eq!(replay, first);
    let state = successful_review_result(
        &socket,
        "state",
        Request::ReviewAttemptStateV2 {
            attempt_id: attempt_id.clone(),
        },
    )
    .await;
    let ResponseResult::ReviewAttemptStateV2(state) = state else {
        panic!("unexpected review state response");
    };
    assert_eq!(state.attempt_revision, 2);

    let reused = call(
        &socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "correlation-reused".into(),
            request: Request::ReviewAttemptAcceptanceSetV2 {
                operation_version: 1,
                operation_id: "durable-operation".into(),
                attempt_id: attempt_id.clone(),
                expected_attempt_revision: 2,
                accepted_success: true,
            },
        },
    )
    .await;
    let ResponseOutcome::Failure { error } = reused.outcome else {
        panic!("changed operation payload unexpectedly succeeded");
    };
    assert_eq!(error.code, ErrorCode::IdempotencyConflict);

    successful_review_result(
        &socket,
        "legacy-suspension",
        Request::ReviewAttemptSuspensionToggle {
            attempt_id: attempt_id.clone(),
        },
    )
    .await;
    let stale = call(
        &socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "stale-correlation".into(),
            request: Request::ReviewAttemptSuspensionSetV2 {
                operation_version: 1,
                operation_id: "stale-operation".into(),
                attempt_id,
                expected_attempt_revision: 2,
                desired_suspended: false,
            },
        },
    )
    .await;
    let ResponseOutcome::Failure { error } = stale.outcome else {
        panic!("stale operation unexpectedly succeeded");
    };
    assert_eq!(error.code, ErrorCode::RevisionConflict);
    daemon.stop().await;
}

#[tokio::test]
async fn online_review_rejects_a_different_second_answer() {
    let daemon = RunningDaemon::start_review_fixture().await;
    let attempt_id = begin_and_answer_review(&daemon.socket).await;
    let response = call(
        &daemon.socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "answer-different".into(),
            request: Request::ReviewAttemptAnswer {
                attempt_id,
                response: ReviewAnswerInput::Text("Different".into()),
            },
        },
    )
    .await;
    let ResponseOutcome::Failure { error } = response.outcome else {
        panic!("different second answer unexpectedly succeeded");
    };
    assert_eq!(error.code, ErrorCode::Conflict);
    daemon.stop().await;
}

#[tokio::test]
async fn online_review_maps_invalid_answer_shape_to_a_client_error() {
    let daemon = RunningDaemon::start_review_fixture().await;
    let attempt_id = begin_review(&daemon.socket).await;
    let response = call(
        &daemon.socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "invalid-answer".into(),
            request: Request::ReviewAttemptAnswer {
                attempt_id,
                response: ReviewAnswerInput::Choice("Answer".into()),
            },
        },
    )
    .await;
    let ResponseOutcome::Failure { error } = response.outcome else {
        panic!("invalid answer shape unexpectedly succeeded");
    };
    assert_eq!(error.code, ErrorCode::MalformedRequest);
    daemon.stop().await;
}

#[tokio::test]
async fn online_review_keeps_an_invalid_stored_matcher_internal() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let database = directory.path().join("gnosis.sqlite");
    let socket = directory.path().join("gnosis.sock");
    create_review_fixture(&source);
    migrate_legacy_database(&source, &database).unwrap();
    let daemon = DaemonProcess::start(&socket, &database).await;
    let attempt_id = begin_review(&socket).await;
    daemon.stop().await;
    Connection::open(&database)
        .unwrap()
        .execute(
            "UPDATE review_attempt SET matcher_version = 99 WHERE attempt_id = ?1",
            [&attempt_id],
        )
        .unwrap();

    let daemon = DaemonProcess::start(&socket, &database).await;
    let response = call(
        &socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "invalid-matcher".into(),
            request: Request::ReviewAttemptAnswer {
                attempt_id,
                response: ReviewAnswerInput::Text("Answer".into()),
            },
        },
    )
    .await;
    let ResponseOutcome::Failure { error } = response.outcome else {
        panic!("invalid stored matcher unexpectedly succeeded");
    };
    assert_eq!(error.code, ErrorCode::Internal);
    daemon.stop().await;
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
            ResponseOutcome::Success { result } => {
                let ResponseResult::DocumentImported(result) = *result else {
                    panic!("unexpected success response");
                };
                assert_eq!(result.revision, 2);
                imported += 1;
            }
            ResponseOutcome::Failure { error } => {
                assert_eq!(error.code, ErrorCode::Conflict);
                assert!(!error.message.contains("database is locked"));
                conflicts += 1;
            }
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
    let child = Command::new(env!("CARGO_BIN_EXE_gnosisd"))
        .args([
            "--socket",
            socket.to_str().unwrap(),
            "--database",
            database.to_str().unwrap(),
        ])
        .spawn()
        .unwrap();
    let child = ChildGuard::new(child);
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
    let status = child.wait_for_exit().await;
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

async fn successful_review_result(socket: &Path, id: &str, request: Request) -> ResponseResult {
    let response = call(
        socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: id.into(),
            request,
        },
    )
    .await;
    let ResponseOutcome::Success { result } = response.outcome else {
        panic!("review request failed: {response:?}");
    };
    *result
}

async fn asset_chunk(
    socket: &Path,
    id: &str,
    logical_path: &str,
    offset: u64,
    limit: u32,
) -> gnosis_core::protocol::AssetChunkView {
    let response = call(
        socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: id.into(),
            request: Request::AssetChunk {
                logical_path: logical_path.into(),
                offset,
                limit,
            },
        },
    )
    .await;
    let ResponseOutcome::Success { result } = response.outcome else {
        panic!("asset request failed: {response:?}");
    };
    let ResponseResult::AssetChunk(chunk) = *result else {
        panic!("unexpected asset response");
    };
    chunk
}

async fn assert_asset_error(
    socket: &Path,
    id: &str,
    logical_path: &str,
    offset: u64,
    limit: u32,
    expected: ErrorCode,
) {
    let response = call(
        socket,
        RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: id.into(),
            request: Request::AssetChunk {
                logical_path: logical_path.into(),
                offset,
                limit,
            },
        },
    )
    .await;
    let ResponseOutcome::Failure { error } = response.outcome else {
        panic!("asset request unexpectedly succeeded: {response:?}");
    };
    assert_eq!(error.code, expected);
}

fn decode_chunk(chunk: &gnosis_core::protocol::AssetChunkView) -> Vec<u8> {
    STANDARD.decode(&chunk.chunk_base64).unwrap()
}

fn canonical_test_asset_path(bytes: &[u8], name: &str) -> String {
    format!("assets/{:x}/{name}", Sha256::digest(bytes))
}

async fn begin_review(socket: &Path) -> String {
    let session = successful_review_result(
        socket,
        "begin-session",
        Request::ReviewSessionCreate {
            source: manual_review_source(),
            target_count: None,
        },
    )
    .await;
    let ResponseResult::ReviewSession(session) = session else {
        panic!("unexpected session response");
    };
    let attempt = successful_review_result(
        socket,
        "begin-attempt",
        Request::ReviewAttemptBegin {
            session_id: session.session_id,
        },
    )
    .await;
    review_attempt_result(attempt).attempt_id
}

async fn begin_and_answer_review(socket: &Path) -> String {
    let attempt_id = begin_review(socket).await;
    successful_review_result(
        socket,
        "answer-attempt",
        Request::ReviewAttemptAnswer {
            attempt_id: attempt_id.clone(),
            response: ReviewAnswerInput::Text("Answer".into()),
        },
    )
    .await;
    attempt_id
}

fn review_attempt_result(result: ResponseResult) -> gnosis_core::protocol::LocalReviewAttemptView {
    let ResponseResult::ReviewAttempt(attempt) = result else {
        panic!("unexpected review attempt response");
    };
    *attempt
}

fn review_attempt_mutation_v2(
    result: ResponseResult,
) -> gnosis_core::protocol::LocalReviewAttemptMutationV2 {
    let ResponseResult::ReviewAttemptMutationV2(attempt) = result else {
        panic!("unexpected review attempt mutation response");
    };
    *attempt
}

fn manual_review_source() -> ReviewSourceInput {
    ReviewSourceInput {
        kind: ReviewSourceKindInput::Manual,
        tags: Vec::new(),
        exclude_tags: Vec::new(),
        topic_node_id: None,
        topic_forward_depth: None,
        topic_backward_depth: None,
        manual_ids: vec!["7".into()],
    }
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

fn create_review_fixture(path: &Path) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE themata (
                id INTEGER PRIMARY KEY,
                type TEXT NOT NULL,
                keimenon TEXT NOT NULL,
                hypothesis TEXT NOT NULL,
                answer TEXT NOT NULL,
                source_guid TEXT,
                archived_at_us INTEGER
            );
            CREATE TABLE nodes (
                id TEXT PRIMARY KEY,
                file TEXT NOT NULL,
                title TEXT NOT NULL,
                level TEXT NOT NULL
            );
            CREATE TABLE node_tag (
                node_id TEXT NOT NULL,
                tag TEXT NOT NULL,
                FOREIGN KEY (node_id) REFERENCES nodes(id),
                UNIQUE (node_id, tag)
            );
            CREATE TABLE review_state (
                thema_id INTEGER PRIMARY KEY,
                state_revision INTEGER NOT NULL,
                review_count INTEGER NOT NULL,
                legacy_review_count INTEGER NOT NULL,
                legacy_unclassified_count INTEGER NOT NULL,
                last_reviewed_at_us INTEGER,
                last_review_day INTEGER,
                next_review_day INTEGER NOT NULL,
                interval_days INTEGER,
                consecutive_successes INTEGER NOT NULL,
                total_successes INTEGER NOT NULL,
                consecutive_failures INTEGER NOT NULL,
                total_failures INTEGER NOT NULL,
                suspended INTEGER NOT NULL,
                gnosis_plus REAL NOT NULL,
                gnosis_minus REAL NOT NULL,
                gnosis_synolon REAL NOT NULL,
                FOREIGN KEY (thema_id) REFERENCES themata(id)
            );
            CREATE TABLE extras (
                id INTEGER PRIMARY KEY,
                parathema TEXT,
                review_image TEXT,
                FOREIGN KEY (id) REFERENCES themata(id)
            );
            CREATE TABLE thema_tag (
                thema_id INTEGER NOT NULL,
                tag TEXT NOT NULL,
                UNIQUE (thema_id, tag),
                FOREIGN KEY (thema_id) REFERENCES themata(id)
            );
            CREATE TABLE thema_links (
                source TEXT,
                dest TEXT,
                UNIQUE (source, dest),
                FOREIGN KEY (source) REFERENCES themata(id)
            );
            INSERT INTO themata VALUES (
                7, '"basic"', '"Question"', 'nil', '("Answer")', NULL, NULL
            );
            INSERT INTO review_state VALUES (
                7, 1, 0, 0, 0, NULL, NULL, 20260715, NULL,
                0, 0, 0, 0, 0, 1.0, 1.0, 1.3
            );
            PRAGMA user_version = 9;
            "#,
        )
        .unwrap();
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
    let ResponseOutcome::Success { result } = response.outcome else {
        panic!("import failed: {:?}", response.outcome);
    };
    let ResponseResult::DocumentImported(result) = *result else {
        panic!("unexpected import response");
    };
    assert_eq!(result.revision, revision);
    assert_eq!(result.node_count, 1);
    assert_eq!(result.occurrence_count, 1);
    assert_eq!(result.semantic_edge_count, 1);
}

fn assert_document(response: ResponseEnvelope, index: usize, revision: u64) {
    let ResponseOutcome::Success { result } = response.outcome else {
        panic!("query failed: {:?}", response.outcome);
    };
    let ResponseResult::Document(document) = *result else {
        panic!("unexpected document response");
    };
    assert_eq!(document.document_id, format!("doc-{index}"));
    assert_eq!(document.revision, revision);
    assert_eq!(document.title, format!("Document {index}"));
}

fn assert_document_revision(response: ResponseEnvelope, revision: u64) {
    let ResponseOutcome::Success { result } = response.outcome else {
        panic!("query failed: {:?}", response.outcome);
    };
    let ResponseResult::Document(document) = *result else {
        panic!("unexpected document response");
    };
    assert_eq!(document.revision, revision);
    assert_eq!(document.nodes.len(), 1);
    assert_eq!(document.link_occurrences.len(), 1);
    assert_eq!(document.semantic_edges.len(), 1);
}

fn socket_mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o777
}
