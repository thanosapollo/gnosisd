use std::{
    fs,
    net::SocketAddr,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

use gnosis_core::{
    client_http::{DeliveryStatus, HttpClient, HttpClientError, SecretToken},
    client_replica::{
        DeviceUploadKind, LocalReplica, OfflineAnswer, OfflineAssetCreate, OfflineDocumentEdit,
        OfflineResponse, PendingDeviceUpload,
    },
    client_review::{ReviewPackAcquisition, ReviewPackAcquisitionError},
    client_sync::{ReplicaSync, ReplicaSyncError},
    protocol::{
        DaemonConfig, DaemonServer, PROTOCOL_VERSION, Request, RequestEnvelope, ResponseOutcome,
        ResponseResult, ServerError,
        http_v1::{
            ApiErrorCodeV1, DocumentMutationResultV1, NoteRenameRequestV1, NoteWriteRequestV1,
            ReviewAnswerV1, ReviewPackAssetV2, ReviewSessionRequestV1, ReviewSourceInputV1,
            ReviewSourceKindV1, SetAcceptedSuccessRequestV1, SetSuspensionRequestV1,
            ThemaWriteRequestV1,
        },
        request,
    },
};
use serde_json::json;
use sha2::{Digest as _, Sha256};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::{TcpListener, TcpStream},
    sync::oneshot,
    time::{Duration, timeout},
};

struct RunningDaemon {
    base_url: String,
    http_addr: SocketAddr,
    socket: PathBuf,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), ServerError>>,
    _directory: TempDir,
}

impl RunningDaemon {
    async fn start() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("gnosis.sock");
        let mut config = DaemonConfig::new(&socket, directory.path().join("gnosis.sqlite"));
        config.http_addr = Some("127.0.0.1:0".parse().unwrap());
        let server = DaemonServer::bind(config).await.unwrap();
        let address = server.http_addr().unwrap();
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = receiver.await;
        }));
        Self {
            base_url: format!("http://{address}"),
            http_addr: address,
            socket,
            shutdown,
            task,
            _directory: directory,
        }
    }

    async fn stop(self) {
        self.shutdown.send(()).unwrap();
        self.task.await.unwrap().unwrap();
    }
}

fn token() -> SecretToken {
    SecretToken::new(format!("gnosis_{}", "a".repeat(43))).unwrap()
}

#[test]
fn client_accepts_literal_loopback_http() {
    for url in [
        "http://localhost:7842",
        "http://127.0.0.1:7842",
        "http://[::1]:7842",
    ] {
        assert!(HttpClient::new(url, token()).is_ok(), "rejected {url}");
    }
    for url in [
        "http://gnosis.example",
        "http://localhost.example:7842",
        "ftp://127.0.0.1",
    ] {
        assert!(
            matches!(
                HttpClient::new(url, token()),
                Err(HttpClientError::InvalidBaseUrl)
            ),
            "accepted {url}"
        );
    }
}

#[test]
fn client_rejects_ambiguous_or_credentialed_base_urls() {
    for url in [
        "https://user@example.com",
        "https://:password@example.com",
        "https://example.com/prefix",
        "https://example.com?query=yes",
        "https://example.com/#fragment",
    ] {
        assert!(
            matches!(
                HttpClient::new(url, token()),
                Err(HttpClientError::InvalidBaseUrl)
            ),
            "accepted {url}"
        );
    }
}

#[test]
fn tokens_and_clients_have_redacted_debug_output() {
    let raw = format!("gnosis_{}", "b".repeat(43));
    let secret = SecretToken::new(raw.clone()).unwrap();
    assert!(!format!("{secret:?}").contains(&raw));

    let client = HttpClient::new("http://127.0.0.1:7842", secret).unwrap();
    assert!(!format!("{client:?}").contains(&raw));
}

#[test]
fn tokens_are_opaque_bounded_bearer_values() {
    for value in ["a".repeat(40), format!("other.{}=", "a".repeat(42))] {
        SecretToken::new(value).unwrap();
    }
    for value in [
        "a".repeat(39),
        "a".repeat(129),
        format!("{}!", "a".repeat(40)),
        format!("{} ", "a".repeat(40)),
    ] {
        assert!(matches!(
            SecretToken::new(value),
            Err(HttpClientError::InvalidToken)
        ));
    }
}

#[tokio::test]
async fn client_authenticates_and_decodes_replica_bootstrap() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(&daemon.base_url, SecretToken::new(raw_token).unwrap()).unwrap();

    let state = client.replica_state().await.unwrap();
    assert_eq!(state.version, 1);
    assert_eq!(state.next_device_sequence, 1);
    assert!(!state.server_id.is_empty());
    let content = client.content_capabilities().await.unwrap();
    let assets = client.asset_capabilities().await.unwrap();
    assert_eq!(content.server_id, state.server_id);
    assert_eq!(assets.server_id, state.server_id);
    assert_eq!(device_id.len(), 36);

    daemon.stop().await;
}

#[tokio::test]
async fn client_runs_the_typed_online_review_lifecycle() {
    let daemon = RunningDaemon::start().await;
    let (_, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let thema_id = create_review_thema(&daemon.base_url, &raw_token).await;
    let request = ReviewSessionRequestV1 {
        source: ReviewSourceInputV1 {
            kind: ReviewSourceKindV1::Manual,
            tags: Vec::new(),
            exclude_tags: Vec::new(),
            topic_node_id: None,
            topic_forward_depth: None,
            topic_backward_depth: None,
            manual_ids: vec![thema_id],
        },
        target_count: None,
    };

    let session = client.create_review_session(&request).await.unwrap();
    assert_eq!(
        client
            .active_review_session()
            .await
            .unwrap()
            .session
            .as_ref(),
        Some(&session)
    );
    assert_eq!(
        client
            .resume_review_session(&session.session_id)
            .await
            .unwrap(),
        session
    );
    let pending = client
        .begin_review_attempt(&session.session_id)
        .await
        .unwrap();
    let graded = client
        .answer_review_attempt(&pending.attempt_id, &ReviewAnswerV1::Text("Answer".into()))
        .await
        .unwrap();
    assert_eq!(graded.detected_success, Some(true));

    let state = client
        .review_attempt_state(&pending.attempt_id)
        .await
        .unwrap();
    let accepted = client
        .set_review_accepted_success(
            &pending.attempt_id,
            &SetAcceptedSuccessRequestV1 {
                version: 1,
                operation_id: "accept-review".into(),
                expected_attempt_revision: state.attempt_revision,
                accepted_success: false,
            },
        )
        .await
        .unwrap();
    let suspended = client
        .set_review_suspension(
            &pending.attempt_id,
            &SetSuspensionRequestV1 {
                version: 1,
                operation_id: "suspend-review".into(),
                expected_attempt_revision: accepted.attempt_revision,
                desired_suspended: true,
            },
        )
        .await
        .unwrap();
    assert!(suspended.attempt.desired_suspended);
    assert!(
        !client
            .commit_review_attempt(&pending.attempt_id)
            .await
            .unwrap()
            .event_id
            .is_empty()
    );
    daemon.stop().await;
}

#[tokio::test]
async fn client_acquires_a_complete_review_pack_and_resumes_after_restart() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let asset = b"offline review asset".to_vec();
    let asset_path = put_review_asset(&daemon.base_url, &raw_token, &asset).await;
    let thema_id = create_review_thema_with_asset(&daemon.base_url, &raw_token, &asset_path).await;
    let mut replica = bootstrap_replica(&daemon, &client, &device_id).await;
    let replica_path = daemon._directory.path().join("replica.sqlite");
    let request = ReviewSessionRequestV1 {
        source: manual_source(thema_id),
        target_count: None,
    };

    let proxy = spawn_drop_path_response_proxy(daemon.http_addr, "/assets/", 1).await;
    let interrupted_client = HttpClient::new(
        &proxy.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let error = ReviewPackAcquisition::new(&interrupted_client)
        .create(&mut replica, &request)
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        ReviewPackAcquisitionError::Http(HttpClientError::Transport {
            delivery: DeliveryStatus::Unknown
        })
    ));
    let pack = replica.active_review_pack().unwrap().unwrap();
    let pack_id = pack.pack_id.clone();
    assert_eq!(pack.assets.len(), 1);
    assert!(
        replica
            .blob(&pack.assets[0].content_sha256)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        replica.missing_blobs().unwrap(),
        vec![pack.assets[0].content_sha256.clone()]
    );
    assert_eq!(proxy.stop().await, 1);

    drop(replica);
    let mut resumed = LocalReplica::open(replica_path, &device_id).unwrap();
    let acquisition = ReviewPackAcquisition::new(&client);
    assert_eq!(
        acquisition.resume_active(&mut resumed).await.unwrap(),
        Some(pack_id.clone())
    );
    assert_eq!(
        resumed.blob(&pack.assets[0].content_sha256).unwrap(),
        Some(asset)
    );
    assert_eq!(
        resumed.active_review_pack().unwrap().unwrap().pack_id,
        pack_id
    );
    assert_eq!(
        client.review_pack(&pack_id).await.unwrap().pack.pack_id,
        pack_id
    );
    resumed
        .answer(
            &pack_id,
            OfflineAnswer {
                attempt_id: "offline-pack-attempt".into(),
                session_position: 0,
                response: OfflineResponse::Text("Answer".into()),
                accepted_success: None,
                desired_suspended: false,
                answered_at_us: pack.issued_at_us + 1,
                fuzz_sample: 0.0,
            },
        )
        .unwrap();
    acquisition.abandon(&mut resumed, &pack_id).await.unwrap();
    assert_eq!(
        resumed.review_pack(&pack_id).unwrap().unwrap().status,
        "abandoned"
    );
    assert_eq!(
        resumed
            .next_device_upload()
            .unwrap()
            .unwrap()
            .operation_id(),
        "offline-pack-attempt"
    );
    assert!(client.active_review_pack().await.unwrap().pack.is_none());
    daemon.stop().await;
}

#[tokio::test]
async fn invalid_pack_capture_rolls_back_the_pack_and_session() {
    let daemon = RunningDaemon::start().await;
    let (_, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let media_type = format!("a/{}", "b".repeat(254));
    assert_eq!(media_type.len(), 256);
    let path = put_review_asset_with_media(
        &daemon.base_url,
        &raw_token,
        b"invalid pack media",
        &media_type,
    )
    .await;
    let thema_id = create_review_thema_with_asset(&daemon.base_url, &raw_token, &path).await;
    let request = ReviewSessionRequestV1 {
        source: manual_source(thema_id),
        target_count: None,
    };

    assert!(matches!(
        client.create_review_pack(&request).await,
        Err(HttpClientError::Server(_))
    ));
    assert!(client.active_review_pack().await.unwrap().pack.is_none());
    assert_no_active_session(&daemon.socket).await;
    daemon.stop().await;
}

#[tokio::test]
async fn cli_routes_review_recovery_and_mutations_through_http() {
    let daemon = RunningDaemon::start().await;
    let (_, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let attempt_id = graded_attempt(&daemon, &client, &raw_token).await;
    let token_path = daemon._directory.path().join("client.token");
    fs::write(&token_path, format!("{raw_token}\n")).unwrap();
    fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();

    let state = run_cli(
        &daemon.base_url,
        &token_path,
        &["review-attempt-state", &attempt_id],
    )
    .await;
    assert!(
        state.status.success(),
        "{}",
        String::from_utf8_lossy(&state.stderr)
    );
    let state: serde_json::Value = serde_json::from_slice(&state.stdout).unwrap();
    let revision = state["attempt_revision"].as_u64().unwrap();

    let proxy = spawn_drop_response_proxy(daemon.http_addr).await;
    let uncertain = run_cli(
        &proxy.base_url,
        &token_path,
        &[
            "review-attempt-set-accepted",
            &attempt_id,
            "--operation-id",
            "cli-accept",
            "--expected-revision",
            &revision.to_string(),
            "--accepted-success",
            "false",
        ],
    )
    .await;
    assert!(!uncertain.status.success());
    let error = String::from_utf8(uncertain.stderr).unwrap();
    assert!(error.contains("review operation cli-accept failed (unknown delivery)"));
    assert!(error.contains("use the same operation ID"));
    assert!(proxy.task.await.unwrap().starts_with(b"HTTP/1.1 200"));

    let accepted = run_cli(
        &daemon.base_url,
        &token_path,
        &[
            "review-attempt-set-accepted",
            &attempt_id,
            "--operation-id",
            "cli-accept",
            "--expected-revision",
            &revision.to_string(),
            "--accepted-success",
            "false",
        ],
    )
    .await;
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    let accepted: serde_json::Value = serde_json::from_slice(&accepted.stdout).unwrap();
    assert_eq!(accepted["operation_id"], "cli-accept");
    assert_eq!(accepted["attempt"]["accepted_success"], false);
    let accepted_revision = accepted["attempt_revision"].as_u64().unwrap();

    let output_failure = run_cli_to_full(
        &daemon.base_url,
        &token_path,
        &[
            "review-attempt-set-suspended",
            &attempt_id,
            "--operation-id",
            "cli-suspend",
            "--expected-revision",
            &accepted_revision.to_string(),
            "--desired-suspended",
            "true",
        ],
    )
    .await;
    assert!(!output_failure.status.success());
    let error = String::from_utf8(output_failure.stderr).unwrap();
    assert!(error.contains("review operation cli-suspend failed (confirmed delivery)"));
    assert!(error.contains("could not write daemon response"), "{error}");

    let suspended = run_cli(
        &daemon.base_url,
        &token_path,
        &[
            "review-attempt-set-suspended",
            &attempt_id,
            "--operation-id",
            "cli-suspend",
            "--expected-revision",
            &accepted_revision.to_string(),
            "--desired-suspended",
            "true",
        ],
    )
    .await;
    assert!(
        suspended.status.success(),
        "{}",
        String::from_utf8_lossy(&suspended.stderr)
    );
    let suspended: serde_json::Value = serde_json::from_slice(&suspended.stdout).unwrap();
    assert_eq!(suspended["operation_id"], "cli-suspend");
    assert_eq!(suspended["attempt"]["desired_suspended"], true);

    let missing = run_cli(
        &daemon.base_url,
        &token_path,
        &["review-attempt-state", "missing-attempt"],
    )
    .await;
    assert!(!missing.status.success());
    assert!(
        String::from_utf8(missing.stderr)
            .unwrap()
            .starts_with("not_found: ")
    );

    let no_token = run_cli_without_token(
        &daemon.base_url,
        &[
            "review-attempt-set-accepted",
            &attempt_id,
            "--operation-id",
            "cli-no-token",
            "--expected-revision",
            "0",
            "--accepted-success",
            "true",
        ],
    )
    .await;
    assert!(!no_token.status.success());
    let error = String::from_utf8(no_token.stderr).unwrap();
    assert!(error.contains("review operation cli-no-token failed (not-sent delivery)"));
    assert!(error.contains("--token-file is required"));
    daemon.stop().await;
}

#[tokio::test]
async fn review_operations_reject_mismatched_success_identities() {
    for response in [
        review_operation_json(2, "operation", "attempt", 2),
        review_operation_json(1, "another-operation", "attempt", 2),
        review_operation_json(1, "operation", "another-attempt", 2),
        review_operation_json(1, "operation", "attempt", 3),
    ] {
        let raw = spawn_json_server(response).await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();
        let error = client
            .set_review_accepted_success(
                "attempt",
                &SetAcceptedSuccessRequestV1 {
                    version: 1,
                    operation_id: "operation".into(),
                    expected_attempt_revision: 1,
                    accepted_success: true,
                },
            )
            .await
            .unwrap_err();

        assert!(matches!(
            error,
            HttpClientError::InvalidResponse {
                delivery: DeliveryStatus::Unknown
            }
        ));
        raw.task.await.unwrap();
    }
}

#[tokio::test]
async fn review_pack_client_rejects_mismatched_and_corrupt_responses() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/protocol/offline-review-v1.json")).unwrap();
    let raw = spawn_json_server(fixture["pack"].clone()).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    assert!(matches!(
        client.review_pack("another-pack").await,
        Err(HttpClientError::InvalidResponse { .. })
    ));
    raw.task.await.unwrap();

    let mut inactive = fixture["pack"].clone();
    inactive["pack"]["status"] = json!("abandoned");
    let raw = spawn_json_response("201 Created", inactive).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    let request = ReviewSessionRequestV1 {
        source: manual_source("thema".into()),
        target_count: None,
    };
    assert!(matches!(
        client.create_review_pack(&request).await,
        Err(HttpClientError::InvalidResponse { .. })
    ));
    raw.task.await.unwrap();

    let mut completed = fixture["pack"]["pack"].clone();
    completed["status"] = json!("completed");
    completed["current_position"] = json!(1);
    let raw = spawn_json_server(json!({"version": 2, "pack": completed})).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    assert!(matches!(
        client.active_review_pack().await,
        Err(HttpClientError::InvalidResponse { .. })
    ));
    raw.task.await.unwrap();

    let expected = b"asset";
    let digest = format!("{:x}", Sha256::digest(expected));
    let raw = spawn_raw_server(
        b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: 5\r\nConnection: close\r\n\r\nother",
    )
    .await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    let asset = ReviewPackAssetV2 {
        logical_path: format!("assets/{digest}/image.png"),
        content_sha256: digest,
        size_bytes: 5,
        media_type: "image/png".into(),
    };
    assert!(matches!(
        client.review_pack_asset("pack", &asset).await,
        Err(HttpClientError::InvalidResponse { .. })
    ));
    raw.task.await.unwrap();
}

#[tokio::test]
async fn review_pack_client_requires_exact_success_statuses() {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/protocol/offline-review-v1.json")).unwrap();
    let request = ReviewSessionRequestV1 {
        source: manual_source("thema".into()),
        target_count: None,
    };

    let raw = spawn_json_server(fixture["pack"].clone()).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    assert_invalid_response(client.create_review_pack(&request).await);
    raw.task.await.unwrap();

    let active = json!({"version": 2, "pack": fixture["pack"]["pack"].clone()});
    let raw = spawn_json_response("201 Created", active).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    assert_invalid_response(client.active_review_pack().await);
    raw.task.await.unwrap();

    let raw = spawn_json_response("201 Created", fixture["pack"].clone()).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    assert_invalid_response(client.review_pack("pack").await);
    raw.task.await.unwrap();

    let mut abandoned = fixture["pack"].clone();
    abandoned["pack"]["status"] = json!("abandoned");
    let pack_id = abandoned["pack"]["pack_id"].as_str().unwrap().to_owned();
    let raw = spawn_json_response("201 Created", abandoned).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    assert_invalid_response(client.abandon_review_pack(&pack_id).await);
    raw.task.await.unwrap();

    let bytes = b"asset";
    let digest = format!("{:x}", Sha256::digest(bytes));
    let raw = spawn_raw_server(
        b"HTTP/1.1 206 Partial Content\r\nContent-Type: image/png\r\nContent-Length: 5\r\nConnection: close\r\n\r\nasset",
    )
    .await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    let asset = ReviewPackAssetV2 {
        logical_path: format!("assets/{digest}/image.png"),
        content_sha256: digest,
        size_bytes: 5,
        media_type: "image/png".into(),
    };
    assert_invalid_response(client.review_pack_asset("pack", &asset).await);
    raw.task.await.unwrap();
}

fn assert_invalid_response<T>(result: Result<T, HttpClientError>) {
    assert!(matches!(
        result,
        Err(HttpClientError::InvalidResponse { .. })
    ));
}

#[tokio::test]
async fn client_preserves_stable_server_errors() {
    let daemon = RunningDaemon::start().await;
    let client = HttpClient::new(&daemon.base_url, token()).unwrap();

    let error = client.replica_state().await.unwrap_err();
    let HttpClientError::Server(server) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert_eq!(server.status(), 401);
    assert_eq!(server.body().error.code(), ApiErrorCodeV1::Unauthorized);

    daemon.stop().await;
}

#[tokio::test]
async fn client_creates_and_replays_a_direct_document_mutation() {
    let daemon = RunningDaemon::start().await;
    let (_, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(&daemon.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let note = NoteWriteRequestV1 {
        logical_path: "note.org".into(),
        org: "#+title: Direct\n".into(),
    };

    let first = client.create_note("direct-create", &note).await.unwrap();
    let retry = client.create_note("direct-create", &note).await.unwrap();

    assert_eq!(retry, first);
    let DocumentMutationResultV1::Applied {
        authoritative,
        changed,
        ..
    } = first
    else {
        panic!("expected applied document mutation");
    };
    assert!(changed);
    assert_eq!(authoritative.revision, 1);

    let changed = NoteWriteRequestV1 {
        org: "#+title: Changed\n".into(),
        ..note
    };
    let error = client
        .create_note("direct-create", &changed)
        .await
        .unwrap_err();
    let HttpClientError::Server(error) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert_eq!(error.status(), 409);
    assert_eq!(
        error.body().error.code(),
        ApiErrorCodeV1::IdempotencyConflict
    );
    daemon.stop().await;
}

#[tokio::test]
async fn client_replaces_documents_against_an_explicit_base_revision() {
    let daemon = RunningDaemon::start().await;
    let (_, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(&daemon.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let original = NoteWriteRequestV1 {
        logical_path: "note.org".into(),
        org: "#+title: Original\n".into(),
    };
    let created = client
        .create_note("create-replace", &original)
        .await
        .unwrap();
    let DocumentMutationResultV1::Applied { authoritative, .. } = created else {
        panic!("expected applied document mutation");
    };
    let replacement = NoteWriteRequestV1 {
        org: "#+title: Replacement\n".into(),
        ..original
    };

    let replaced = client
        .replace_note(
            &authoritative.document_id,
            "replace-document",
            authoritative.revision,
            &replacement,
        )
        .await
        .unwrap();
    let retry = client
        .replace_note(
            &authoritative.document_id,
            "replace-document",
            authoritative.revision,
            &replacement,
        )
        .await
        .unwrap();

    assert_eq!(retry, replaced);
    let DocumentMutationResultV1::Applied { authoritative, .. } = replaced else {
        panic!("expected applied document mutation");
    };
    assert_eq!(authoritative.revision, 2);

    let stale = client
        .replace_note(
            &authoritative.document_id,
            "stale-document",
            1,
            &replacement,
        )
        .await
        .unwrap();
    let DocumentMutationResultV1::Rejected { rejection, .. } = stale else {
        panic!("expected retained rejection");
    };
    assert_eq!(
        rejection,
        gnosis_core::protocol::http_v1::DocumentMutationRejectionV1::RevisionConflict {
            expected_revision: Some(1),
            current_revision: Some(2),
        }
    );
    daemon.stop().await;
}

#[tokio::test]
async fn client_renames_and_tombstones_documents_with_durable_identities() {
    let daemon = RunningDaemon::start().await;
    let (_, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(&daemon.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let created = client
        .create_note("lifecycle-create", &note_request())
        .await
        .unwrap();
    let DocumentMutationResultV1::Applied { authoritative, .. } = created else {
        panic!("expected applied document mutation");
    };
    let rename = NoteRenameRequestV1 {
        logical_path: "renamed.org".into(),
    };

    let renamed = client
        .rename_note(
            &authoritative.document_id,
            "lifecycle-rename",
            authoritative.revision,
            &rename,
        )
        .await
        .unwrap();
    let rename_retry = client
        .rename_note(
            &authoritative.document_id,
            "lifecycle-rename",
            authoritative.revision,
            &rename,
        )
        .await
        .unwrap();
    assert_eq!(rename_retry, renamed);
    let DocumentMutationResultV1::Applied { authoritative, .. } = renamed else {
        panic!("expected applied rename");
    };
    assert_eq!(authoritative.logical_path, "renamed.org");

    let deleted = client
        .tombstone_note(
            &authoritative.document_id,
            "lifecycle-delete",
            authoritative.revision,
        )
        .await
        .unwrap();
    let delete_retry = client
        .tombstone_note(
            &authoritative.document_id,
            "lifecycle-delete",
            authoritative.revision,
        )
        .await
        .unwrap();
    assert_eq!(delete_retry, deleted);
    let DocumentMutationResultV1::Applied { authoritative, .. } = deleted else {
        panic!("expected applied tombstone");
    };
    assert!(!authoritative.active);
    daemon.stop().await;
}

#[tokio::test]
async fn lost_direct_document_response_replays_without_duplication() {
    let daemon = RunningDaemon::start().await;
    let (_, raw_token) = create_device(&daemon.socket).await;
    let direct = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let before = direct.replica_state().await.unwrap().cursor;
    let proxy = spawn_drop_response_proxy(daemon.http_addr).await;
    let uncertain = HttpClient::new(&proxy.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let note = NoteWriteRequestV1 {
        logical_path: "note.org".into(),
        org: "#+title: Once\n".into(),
    };

    assert!(matches!(
        uncertain.create_note("lost-direct", &note).await,
        Err(HttpClientError::Transport {
            delivery: DeliveryStatus::Unknown
        })
    ));
    assert!(proxy.task.await.unwrap().starts_with(b"HTTP/1.1 200"));
    let first_retry = direct.create_note("lost-direct", &note).await.unwrap();
    let second_retry = direct.create_note("lost-direct", &note).await.unwrap();

    assert_eq!(first_retry, second_retry);
    assert_eq!(direct.changes(&before).await.unwrap().items.len(), 1);
    daemon.stop().await;
}

#[tokio::test]
async fn direct_document_client_rejects_invalid_operation_ids_before_transport() {
    let client = HttpClient::new("http://127.0.0.1:9", token()).unwrap();
    let note = NoteWriteRequestV1 {
        logical_path: "note.org".into(),
        org: "#+title: Note\n".into(),
    };

    assert!(matches!(
        client.create_note("-invalid", &note).await,
        Err(HttpClientError::InvalidOperationId)
    ));
}

#[tokio::test]
async fn direct_document_client_rejects_another_operation_result() {
    let raw = spawn_json_server(json!({
        "status": "applied",
        "format_version": 1,
        "operation_id": "another-operation",
        "changed": true,
        "authoritative": {
            "document_id": "document",
            "logical_path": "note.org",
            "revision": 1,
            "content_sha256": null,
            "active": true
        },
        "cursor": "cursor"
    }))
    .await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    let note = NoteWriteRequestV1 {
        logical_path: "note.org".into(),
        org: "#+title: Note\n".into(),
    };

    assert!(matches!(
        client.create_note("expected-operation", &note).await,
        Err(HttpClientError::InvalidResponse {
            delivery: DeliveryStatus::Unknown
        })
    ));
    raw.task.await.unwrap();
}

#[tokio::test]
async fn direct_document_client_rejects_another_result_version() {
    let raw = spawn_json_server(document_mutation_json(2, "expected-operation", "document")).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();

    assert!(matches!(
        client
            .create_note("expected-operation", &note_request())
            .await,
        Err(HttpClientError::InvalidResponse {
            delivery: DeliveryStatus::Unknown
        })
    ));
    raw.task.await.unwrap();
}

#[tokio::test]
async fn direct_document_client_rejects_another_replacement_target() {
    let raw = spawn_json_server(document_mutation_json(
        1,
        "expected-operation",
        "another-document",
    ))
    .await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();

    assert!(matches!(
        client
            .replace_note(
                "expected-document",
                "expected-operation",
                1,
                &note_request(),
            )
            .await,
        Err(HttpClientError::InvalidResponse {
            delivery: DeliveryStatus::Unknown
        })
    ));
    raw.task.await.unwrap();
}

#[tokio::test]
async fn uncertain_server_failures_keep_unknown_mutation_delivery() {
    for (status, code) in [
        ("408 Request Timeout", "request_timeout"),
        ("500 Internal Server Error", "internal"),
    ] {
        let raw = spawn_json_response(
            status,
            json!({"error":{"code":code,"message":"request outcome is unknown","meta":null}}),
        )
        .await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();

        let error = client
            .create_note("expected-operation", &note_request())
            .await
            .unwrap_err();
        let HttpClientError::Server(error) = error else {
            panic!("expected typed server error");
        };
        assert_eq!(error.delivery(), DeliveryStatus::Unknown);
        raw.task.await.unwrap();
    }
}

#[tokio::test]
async fn client_pages_compose_with_the_durable_replica() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(&daemon.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let mut replica =
        LocalReplica::open(daemon._directory.path().join("replica.sqlite"), &device_id).unwrap();

    let state = client.replica_state().await.unwrap();
    replica.observe_state(state).unwrap();
    replica
        .install_content_capabilities(client.content_capabilities().await.unwrap())
        .unwrap();
    replica
        .install_asset_capabilities(client.asset_capabilities().await.unwrap())
        .unwrap();
    let snapshot = client.snapshot(None).await.unwrap();
    let cursor = snapshot.cursor.clone().unwrap();
    replica.install_snapshot_page(None, snapshot).unwrap();
    let changes = client.changes(&cursor).await.unwrap();
    assert!(!replica.install_change_page(changes).unwrap());
    assert_eq!(replica.status().unwrap().cursor.as_deref(), Some(&*cursor));

    daemon.stop().await;
}

#[tokio::test]
async fn replica_sync_bootstraps_and_drains_a_restarted_outbox_in_sequence() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(&daemon.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let replica_path = daemon._directory.path().join("coordinated-replica.sqlite");
    let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
    let sync = ReplicaSync::new(&client);

    let bootstrap = sync.synchronize(&mut replica).await.unwrap();
    assert!(bootstrap.snapshot_pages > 0);
    let asset = replica
        .record_asset(OfflineAssetCreate {
            operation_id: "coordinated-asset".into(),
            original_name: "image.png".into(),
            media_type: "image/png".into(),
            bytes: b"coordinated image".to_vec(),
            authored_at_us: 10,
        })
        .unwrap();
    replica
        .record_document(OfflineDocumentEdit {
            operation_id: "coordinated-document".into(),
            document_id: "coordinated-document".into(),
            logical_path: "coordinated.org".into(),
            org: format!("#+title: Coordinated\n[[file:{}]]\n", asset.logical_path),
            authored_at_us: 11,
        })
        .unwrap();
    drop(replica);

    let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
    let reconciled = sync.synchronize(&mut replica).await.unwrap();
    assert_eq!(reconciled.uploaded_operations, 2);
    assert_eq!(reconciled.staged_upload_blobs, 1);
    assert!(replica.next_device_upload().unwrap().is_none());
    assert!(
        replica
            .projection("document", "coordinated-document")
            .unwrap()
            .is_some()
    );
    assert!(matches!(
        replica.asset(&asset.logical_path).unwrap(),
        gnosis_core::client_replica::ReplicaAssetState::Available(_)
    ));

    let settled = sync.synchronize(&mut replica).await.unwrap();
    assert_eq!(settled.snapshot_pages, 0);
    assert_eq!(settled.uploaded_operations, 0);
    daemon.stop().await;
}

#[tokio::test]
async fn replica_sync_recovers_an_expired_delta_cursor_with_a_fresh_snapshot() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let direct = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let replica_path = daemon._directory.path().join("recovering-replica.sqlite");
    let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
    ReplicaSync::new(&direct)
        .synchronize(&mut replica)
        .await
        .unwrap();

    let proxy = spawn_snapshot_required_proxy(daemon.http_addr).await;
    let client = HttpClient::new(&proxy.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let report = ReplicaSync::new(&client)
        .synchronize(&mut replica)
        .await
        .unwrap();

    assert_eq!(report.snapshot_recoveries, 1);
    assert!(report.snapshot_pages > 0);
    assert_eq!(proxy.stop().await, 1);
    assert!(replica.status().unwrap().cursor.is_some());
    daemon.stop().await;
}

#[tokio::test]
async fn replica_sync_recovers_when_snapshot_cursor_expires_before_delta_pull() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let replica_path = daemon
        ._directory
        .path()
        .join("expired-snapshot-replica.sqlite");
    let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
    let proxy = spawn_snapshot_required_proxy(daemon.http_addr).await;
    let client = HttpClient::new(&proxy.base_url, SecretToken::new(raw_token).unwrap()).unwrap();

    let report = ReplicaSync::new(&client)
        .synchronize(&mut replica)
        .await
        .unwrap();

    assert_eq!(report.snapshot_recoveries, 1);
    assert!(report.snapshot_pages >= 2);
    assert_eq!(proxy.stop().await, 1);
    assert!(replica.status().unwrap().cursor.is_some());
    daemon.stop().await;
}

#[tokio::test]
async fn replica_sync_stops_on_unknown_delivery_and_replays_the_same_operation() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let direct = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let replica_path = daemon._directory.path().join("uncertain-replica.sqlite");
    let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
    ReplicaSync::new(&direct)
        .synchronize(&mut replica)
        .await
        .unwrap();
    replica
        .record_document(OfflineDocumentEdit {
            operation_id: "uncertain-operation".into(),
            document_id: "uncertain-document".into(),
            logical_path: "uncertain.org".into(),
            org: "#+title: Uncertain\n".into(),
            authored_at_us: 10,
        })
        .unwrap();

    let proxy = spawn_drop_operation_response_proxy(daemon.http_addr).await;
    let client = HttpClient::new(&proxy.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let error = ReplicaSync::new(&client)
        .synchronize(&mut replica)
        .await
        .unwrap_err();
    let ReplicaSyncError::Upload {
        operation,
        delivery,
        ..
    } = error
    else {
        panic!("unexpected synchronization error: {error:?}");
    };
    assert_eq!(operation.operation_id, "uncertain-operation");
    assert_eq!(operation.device_sequence, 1);
    assert_eq!(delivery, DeliveryStatus::Unknown);
    assert_eq!(proxy.stop().await, 1);
    assert_eq!(
        replica
            .next_device_upload()
            .unwrap()
            .unwrap()
            .operation_id(),
        "uncertain-operation"
    );
    drop(replica);

    let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
    let recovered = ReplicaSync::new(&direct)
        .synchronize(&mut replica)
        .await
        .unwrap();
    assert_eq!(recovered.uploaded_operations, 1);
    assert!(replica.next_device_upload().unwrap().is_none());
    daemon.stop().await;
}

#[tokio::test]
async fn replica_sync_resumes_an_interrupted_snapshot_after_replica_restart() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let direct = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let mut expected_document = None;
    for index in 0..20 {
        let result = direct
            .create_note(
                &format!("snapshot-seed-{index}"),
                &NoteWriteRequestV1 {
                    logical_path: format!("snapshot-{index}.org"),
                    org: format!("#+title: Snapshot {index}\n"),
                },
            )
            .await
            .unwrap();
        if expected_document.is_none() {
            let DocumentMutationResultV1::Applied { authoritative, .. } = result else {
                panic!("seed document was not applied");
            };
            expected_document = Some(authoritative.document_id);
        }
    }
    let expected_document = expected_document.unwrap();
    let replica_path = daemon._directory.path().join("resumable-replica.sqlite");
    let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
    let proxy = spawn_drop_path_response_proxy(daemon.http_addr, "/v1/sync/snapshot", 2).await;
    let client = HttpClient::new(&proxy.base_url, SecretToken::new(raw_token).unwrap()).unwrap();

    assert!(
        ReplicaSync::new(&client)
            .synchronize(&mut replica)
            .await
            .is_err()
    );
    assert!(replica.status().unwrap().snapshot_next.is_some());
    assert!(
        replica
            .projection("document", &expected_document)
            .unwrap()
            .is_none()
    );
    assert_eq!(proxy.stop().await, 2);
    drop(replica);

    let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
    let report = ReplicaSync::new(&direct)
        .synchronize(&mut replica)
        .await
        .unwrap();
    assert!(report.snapshot_pages > 0);
    assert!(replica.status().unwrap().snapshot_next.is_none());
    assert!(
        replica
            .projection("document", &expected_document)
            .unwrap()
            .is_some()
    );
    daemon.stop().await;
}

#[tokio::test]
async fn client_rejects_noncanonical_blob_digests_before_transport() {
    let client = HttpClient::new("http://127.0.0.1:9", token()).unwrap();
    for digest in ["short", &"A".repeat(64), &format!("{}!", "a".repeat(63))] {
        assert!(matches!(
            client.blob(digest).await,
            Err(HttpClientError::InvalidDigest)
        ));
    }
}

#[tokio::test]
async fn client_rejects_malformed_and_oversized_json_responses() {
    let malformed = spawn_raw_server(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{]",
    )
    .await;
    let client = HttpClient::new(&malformed.base_url, token()).unwrap();
    assert!(matches!(
        client.replica_state().await,
        Err(HttpClientError::InvalidResponse { .. })
    ));
    malformed.task.await.unwrap();

    let oversized = spawn_raw_server(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 99999999\r\nConnection: close\r\n\r\n",
    )
    .await;
    let client = HttpClient::new(&oversized.base_url, token()).unwrap();
    assert!(matches!(
        client.replica_state().await,
        Err(HttpClientError::ResponseTooLarge { .. })
    ));
    oversized.task.await.unwrap();

    let wrong_type = spawn_raw_server(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json-seq\r\nContent-Length: 129\r\nConnection: close\r\n\r\n{\"version\":1,\"server_id\":\"0123456789abcdef0123456789abcdef\",\"cursor\":\"cursor\",\"next_review_sequence\":1,\"snapshot_required\":false}",
    )
    .await;
    let client = HttpClient::new(&wrong_type.base_url, token()).unwrap();
    assert!(matches!(
        client.replica_state().await,
        Err(HttpClientError::InvalidResponse { .. })
    ));
    wrong_type.task.await.unwrap();
}

#[tokio::test]
async fn lost_response_is_indeterminate_and_is_not_retried() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        read_one_request(&listener).await;
        timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_ok()
    });
    let client = HttpClient::new(&format!("http://{address}"), token()).unwrap();

    let error = client.replica_state().await.unwrap_err();
    assert!(matches!(
        error,
        HttpClientError::Transport {
            delivery: gnosis_core::client_http::DeliveryStatus::Unknown
        }
    ));
    assert!(!server.await.unwrap(), "client retried a lost response");
}

#[tokio::test]
async fn invalid_mutation_success_keeps_unknown_delivery() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(&daemon.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let mut replica = bootstrap_replica(&daemon, &client, &device_id).await;
    replica
        .record_document(OfflineDocumentEdit {
            operation_id: "invalid-response-operation".into(),
            document_id: "document".into(),
            logical_path: "note.org".into(),
            org: "#+title: Offline\n".into(),
            authored_at_us: 10,
        })
        .unwrap();
    let upload = replica.next_device_upload().unwrap().unwrap();
    let raw = spawn_raw_server(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
    )
    .await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();

    assert!(matches!(
        client.upload(&upload).await,
        Err(HttpClientError::InvalidResponse {
            delivery: DeliveryStatus::Unknown
        })
    ));
    assert_eq!(
        replica
            .next_device_upload()
            .unwrap()
            .unwrap()
            .operation_id(),
        "invalid-response-operation"
    );
    raw.task.await.unwrap();
    daemon.stop().await;
}

#[tokio::test]
async fn lost_mutation_response_restarts_and_replays_exactly_once() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let direct = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let replica_path = daemon._directory.path().join("replica.sqlite");
    let mut replica = bootstrap_replica(&daemon, &direct, &device_id).await;
    replica
        .record_document(OfflineDocumentEdit {
            operation_id: "lost-content-response".into(),
            document_id: "document".into(),
            logical_path: "note.org".into(),
            org: "#+title: Offline\n".into(),
            authored_at_us: 10,
        })
        .unwrap();
    let upload = replica.next_device_upload().unwrap().unwrap();
    let proxy = spawn_drop_response_proxy(daemon.http_addr).await;
    let client = HttpClient::new(&proxy.base_url, SecretToken::new(raw_token).unwrap()).unwrap();

    assert!(matches!(
        client.upload(&upload).await,
        Err(HttpClientError::Transport {
            delivery: DeliveryStatus::Unknown
        })
    ));
    assert!(proxy.task.await.unwrap().starts_with(b"HTTP/1.1 200"));
    drop(replica);

    let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
    let retry = replica.next_device_upload().unwrap().unwrap();
    assert_eq!(retry.operation_id(), "lost-content-response");
    assert_eq!(retry.device_sequence(), 1);
    replica
        .record_device_result(direct.upload(&retry).await.unwrap())
        .unwrap();
    assert!(replica.next_device_upload().unwrap().is_none());
    assert_eq!(replica.content_results(None, 10).unwrap().items.len(), 1);

    daemon.stop().await;
}

#[tokio::test]
async fn client_uploads_and_records_a_durable_content_operation() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(&daemon.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let mut replica = bootstrap_replica(&daemon, &client, &device_id).await;
    replica
        .record_document(OfflineDocumentEdit {
            operation_id: "content-operation".into(),
            document_id: "document".into(),
            logical_path: "note.org".into(),
            org: "#+title: Offline\n* Heading\n".into(),
            authored_at_us: 10,
        })
        .unwrap();
    let upload = replica.next_device_upload().unwrap().unwrap();

    let result = client.upload(&upload).await.unwrap();
    assert_eq!(result.kind(), DeviceUploadKind::Content);
    replica.record_device_result(result).unwrap();
    assert!(replica.next_device_upload().unwrap().is_none());
    assert_eq!(replica.content_results(None, 10).unwrap().items.len(), 1);

    daemon.stop().await;
}

#[tokio::test]
async fn client_stages_content_addressed_assets_before_retrying_upload() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(&daemon.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let mut replica = bootstrap_replica(&daemon, &client, &device_id).await;
    let bytes = b"offline image".to_vec();
    let recorded = replica
        .record_asset(OfflineAssetCreate {
            operation_id: "asset-operation".into(),
            original_name: "image.png".into(),
            media_type: "image/png".into(),
            bytes: bytes.clone(),
            authored_at_us: 10,
        })
        .unwrap();
    let upload = replica.next_device_upload().unwrap().unwrap();

    let missing = client.upload(&upload).await.unwrap_err();
    let HttpClientError::Server(server) = missing else {
        panic!("unexpected error: {missing:?}");
    };
    assert_eq!(server.body().error.code(), ApiErrorCodeV1::BlobRequired);
    let PendingDeviceUpload::Asset(asset) = &upload else {
        panic!("expected asset upload");
    };
    client.stage_blob(asset).await.unwrap();
    let result = client.upload(&upload).await.unwrap();
    assert_eq!(result.kind(), DeviceUploadKind::Asset);
    replica.record_device_result(result).unwrap();
    assert_eq!(client.blob(&recorded.content_sha256).await.unwrap(), bytes);

    daemon.stop().await;
}

async fn bootstrap_replica(
    daemon: &RunningDaemon,
    client: &HttpClient,
    device_id: &str,
) -> LocalReplica {
    let mut replica =
        LocalReplica::open(daemon._directory.path().join("replica.sqlite"), device_id).unwrap();
    replica
        .observe_state(client.replica_state().await.unwrap())
        .unwrap();
    replica
        .install_content_capabilities(client.content_capabilities().await.unwrap())
        .unwrap();
    replica
        .install_asset_capabilities(client.asset_capabilities().await.unwrap())
        .unwrap();
    let snapshot = client.snapshot(None).await.unwrap();
    replica.install_snapshot_page(None, snapshot).unwrap();
    replica
}

async fn create_device(socket: &Path) -> (String, String) {
    let response = request(
        socket,
        &RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "create-device".into(),
            request: Request::DeviceCreate {
                name: "client".into(),
                scopes: vec!["read".into(), "review".into(), "content:write".into()],
            },
        },
    )
    .await
    .unwrap();
    let ResponseOutcome::Success { result } = response.outcome else {
        panic!("device creation failed");
    };
    let ResponseResult::DeviceCreated(created) = *result else {
        panic!("unexpected device response");
    };
    (created.device.device_id, created.token)
}

async fn graded_attempt(daemon: &RunningDaemon, client: &HttpClient, token: &str) -> String {
    let thema_id = create_review_thema(&daemon.base_url, token).await;
    let session = client
        .create_review_session(&ReviewSessionRequestV1 {
            source: manual_source(thema_id),
            target_count: None,
        })
        .await
        .unwrap();
    let attempt = client
        .begin_review_attempt(&session.session_id)
        .await
        .unwrap();
    client
        .answer_review_attempt(&attempt.attempt_id, &ReviewAnswerV1::Text("Answer".into()))
        .await
        .unwrap();
    attempt.attempt_id
}

fn manual_source(thema_id: String) -> ReviewSourceInputV1 {
    ReviewSourceInputV1 {
        kind: ReviewSourceKindV1::Manual,
        tags: Vec::new(),
        exclude_tags: Vec::new(),
        topic_node_id: None,
        topic_forward_depth: None,
        topic_backward_depth: None,
        manual_ids: vec![thema_id],
    }
}

async fn run_cli(base_url: &str, token_path: &Path, args: &[&str]) -> Output {
    run_cli_output(base_url, Some(token_path), args, false).await
}

async fn run_cli_to_full(base_url: &str, token_path: &Path, args: &[&str]) -> Output {
    run_cli_output(base_url, Some(token_path), args, true).await
}

async fn run_cli_without_token(base_url: &str, args: &[&str]) -> Output {
    run_cli_output(base_url, None, args, false).await
}

async fn run_cli_output(
    base_url: &str,
    token_path: Option<&Path>,
    args: &[&str],
    fail_output: bool,
) -> Output {
    let base_url = base_url.to_owned();
    let token_path = token_path.map(Path::to_owned);
    let args: Vec<String> = args.iter().map(|value| (*value).to_owned()).collect();
    tokio::task::spawn_blocking(move || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gnosis"));
        command.arg("--http-url").arg(base_url);
        if let Some(path) = token_path {
            command.arg("--token-file").arg(path);
        }
        if fail_output {
            let full = fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap();
            command.stdout(Stdio::from(full));
        }
        command.args(args).output().unwrap()
    })
    .await
    .unwrap()
}

fn review_thema_request() -> ThemaWriteRequestV1 {
    ThemaWriteRequestV1 {
        kind: "basic".into(),
        keimenon: "Question".into(),
        hypothesis: Vec::new(),
        answer: vec!["Answer".into()],
        source_guid: None,
        parathema: None,
        review_image: None,
        tags: Vec::new(),
        node_ids: Vec::new(),
        asset_paths: Vec::new(),
    }
}

async fn put_review_asset(base_url: &str, token: &str, bytes: &[u8]) -> String {
    put_review_asset_with_media(base_url, token, bytes, "image/png").await
}

async fn put_review_asset_with_media(
    base_url: &str,
    token: &str,
    bytes: &[u8],
    media_type: &str,
) -> String {
    let digest = format!("{:x}", Sha256::digest(bytes));
    let path = format!("assets/{digest}/image.png");
    let response = raw_http_client()
        .put(format!("{base_url}/v1/assets/{digest}/image.png"))
        .bearer_auth(token)
        .header("content-type", media_type)
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    path
}

async fn assert_no_active_session(socket: &Path) {
    let response = request(
        socket,
        &RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "active-session".into(),
            request: Request::ReviewSessionActive {},
        },
    )
    .await
    .unwrap();
    let ResponseOutcome::Success { result } = response.outcome else {
        panic!("active session query failed");
    };
    let ResponseResult::ActiveReviewSession(active) = *result else {
        panic!("unexpected active session response");
    };
    assert!(active.session.is_none());
}

async fn create_review_thema_with_asset(base_url: &str, token: &str, asset_path: &str) -> String {
    let mut input = review_thema_request();
    input.review_image = Some(asset_path.to_owned());
    input.asset_paths = vec![asset_path.to_owned()];
    create_review_thema_from(base_url, token, &input).await
}

async fn create_review_thema(base_url: &str, token: &str) -> String {
    create_review_thema_from(base_url, token, &review_thema_request()).await
}

async fn create_review_thema_from(
    base_url: &str,
    token: &str,
    input: &ThemaWriteRequestV1,
) -> String {
    let response = raw_http_client()
        .post(format!("{base_url}/v1/themata"))
        .bearer_auth(token)
        .header("content-type", "application/json")
        .body(serde_json::to_vec(input).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    serde_json::from_slice::<serde_json::Value>(&response.bytes().await.unwrap())
        .unwrap()["thema_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn raw_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .tls_backend_rustls()
        .tls_certs_only(std::iter::empty::<reqwest::Certificate>())
        .build()
        .unwrap()
}

struct RawServer {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

struct DropResponseProxy {
    base_url: String,
    task: tokio::task::JoinHandle<Vec<u8>>,
}

struct SnapshotRequiredProxy {
    base_url: String,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<usize>,
}

struct DropPathResponseProxy {
    base_url: String,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<usize>,
}

impl DropPathResponseProxy {
    async fn stop(self) -> usize {
        self.shutdown.send(()).unwrap();
        self.task.await.unwrap()
    }
}

async fn spawn_drop_operation_response_proxy(upstream: SocketAddr) -> DropPathResponseProxy {
    spawn_drop_path_response_proxy(upstream, "/v1/content/offline-operations", 1).await
}

async fn spawn_drop_path_response_proxy(
    upstream: SocketAddr,
    path: &'static str,
    drop_match: usize,
) -> DropPathResponseProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, mut receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut matches = 0;
        loop {
            tokio::select! {
                _ = &mut receiver => return matches,
                accepted = listener.accept() => {
                    let (mut client, _) = accepted.unwrap();
                    let request = read_http_request(&mut client).await;
                    if request_target(&request).contains(path) {
                        matches += 1;
                    }
                    if matches == drop_match && request_target(&request).contains(path) {
                        upstream_http_response(upstream, &request).await;
                    } else {
                        forward_http_request(upstream, &mut client, &request).await;
                    }
                }
            }
        }
    });
    DropPathResponseProxy {
        base_url: format!("http://{address}"),
        shutdown,
        task,
    }
}

impl SnapshotRequiredProxy {
    async fn stop(self) -> usize {
        self.shutdown.send(()).unwrap();
        self.task.await.unwrap()
    }
}

async fn spawn_snapshot_required_proxy(upstream: SocketAddr) -> SnapshotRequiredProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (shutdown, mut receiver) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut intercepted = 0;
        loop {
            tokio::select! {
                _ = &mut receiver => return intercepted,
                accepted = listener.accept() => {
                    let (mut client, _) = accepted.unwrap();
                    let request = read_http_request(&mut client).await;
                    if intercepted == 0 && request_target(&request).contains("/v1/sync/changes") {
                        client.write_all(snapshot_required_response()).await.unwrap();
                        intercepted += 1;
                    } else {
                        forward_http_request(upstream, &mut client, &request).await;
                    }
                }
            }
        }
    });
    SnapshotRequiredProxy {
        base_url: format!("http://{address}"),
        shutdown,
        task,
    }
}

async fn forward_http_request(upstream: SocketAddr, client: &mut TcpStream, request: &[u8]) {
    let response = upstream_http_response(upstream, request).await;
    client.write_all(&response).await.unwrap();
}

async fn upstream_http_response(upstream: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut server = TcpStream::connect(upstream).await.unwrap();
    server.write_all(request).await.unwrap();
    read_http_request(&mut server).await
}

fn request_target(request: &[u8]) -> &str {
    std::str::from_utf8(request)
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
}

fn snapshot_required_response() -> &'static [u8] {
    b"HTTP/1.1 409 Conflict\r\nContent-Type: application/json\r\nContent-Length: 123\r\nConnection: close\r\n\r\n{\"error\":{\"code\":\"snapshot_required\",\"message\":\"replica snapshot is required\",\"meta\":{\"snapshot_url\":\"/v1/sync/snapshot\"}}}"
}

async fn spawn_drop_response_proxy(upstream: SocketAddr) -> DropResponseProxy {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut client, _) = listener.accept().await.unwrap();
        let request = read_http_request(&mut client).await;
        let mut server = TcpStream::connect(upstream).await.unwrap();
        server.write_all(&request).await.unwrap();
        read_http_headers(&mut server).await
    });
    DropResponseProxy {
        base_url: format!("http://{address}"),
        task,
    }
}

async fn spawn_raw_server(response: &'static [u8]) -> RawServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request_head(&mut stream).await;
        stream.write_all(response).await.unwrap();
    });
    RawServer {
        base_url: format!("http://{address}"),
        task,
    }
}

async fn spawn_json_server(value: serde_json::Value) -> RawServer {
    spawn_json_response("200 OK", value).await
}

async fn spawn_json_response(status: &'static str, value: serde_json::Value) -> RawServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        read_request_head(&mut stream).await;
        let body = serde_json::to_vec(&value).unwrap();
        let headers = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream.write_all(headers.as_bytes()).await.unwrap();
        stream.write_all(&body).await.unwrap();
    });
    RawServer {
        base_url: format!("http://{address}"),
        task,
    }
}

fn review_operation_json(
    version: u32,
    operation_id: &str,
    attempt_id: &str,
    attempt_revision: u64,
) -> serde_json::Value {
    json!({
        "version":version,
        "operation_id":operation_id,
        "changed":true,
        "attempt_revision":attempt_revision,
        "attempt":{
            "attempt_id":attempt_id,
            "session_id":"session",
            "thema_id":"thema",
            "presentation":{"kind":"basic","question":"Question",
                "response":{"kind":"text","input_script":null},
                "source_guid":null,"review_image":null,"tags":[]},
            "revealed_answer":["Answer"],
            "revealed_parathema":null,
            "matcher_version":1,
            "response":{"kind":"text","value":"Answer"},
            "detected_success":true,
            "accepted_success":true,
            "override_count":0,
            "proposed_transition":null,
            "desired_suspended":false,
            "status":"graded"
        }
    })
}

fn document_mutation_json(
    format_version: u32,
    operation_id: &str,
    document_id: &str,
) -> serde_json::Value {
    json!({
        "status": "applied",
        "format_version": format_version,
        "operation_id": operation_id,
        "changed": true,
        "authoritative": {
            "document_id": document_id,
            "logical_path": "note.org",
            "revision": 1,
            "content_sha256": null,
            "active": true
        },
        "cursor": "cursor"
    })
}

fn note_request() -> NoteWriteRequestV1 {
    NoteWriteRequestV1 {
        logical_path: "note.org".into(),
        org: "#+title: Note\n".into(),
    }
}

async fn read_one_request(listener: &TcpListener) {
    let (mut stream, _) = listener.accept().await.unwrap();
    read_request_head(&mut stream).await;
}

async fn read_request_head(stream: &mut tokio::net::TcpStream) {
    read_http_headers(stream).await;
}

async fn read_http_request(stream: &mut TcpStream) -> Vec<u8> {
    let mut request = read_http_headers(stream).await;
    let header_end = header_end(&request).unwrap();
    let body_length = content_length(&request[..header_end]);
    let target = header_end.checked_add(body_length).unwrap();
    while request.len() < target {
        read_http_chunk(stream, &mut request).await;
    }
    request.truncate(target);
    request
}

async fn read_http_headers(stream: &mut TcpStream) -> Vec<u8> {
    let mut message = Vec::new();
    loop {
        read_http_chunk(stream, &mut message).await;
        assert!(message.len() <= 16 * 1024, "HTTP headers are too large");
        if header_end(&message).is_some() {
            return message;
        }
    }
}

async fn read_http_chunk(stream: &mut TcpStream, message: &mut Vec<u8>) {
    let mut chunk = [0; 1024];
    let read = stream.read(&mut chunk).await.unwrap();
    assert!(read > 0, "peer closed before completing HTTP message");
    message.extend_from_slice(&chunk[..read]);
}

fn header_end(message: &[u8]) -> Option<usize> {
    message
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| position + 4)
}

fn content_length(headers: &[u8]) -> usize {
    String::from_utf8_lossy(headers)
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
        .unwrap_or(0)
}
