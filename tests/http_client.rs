use std::{
    fs,
    io::Write as _,
    net::SocketAddr,
    os::unix::fs::PermissionsExt as _,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use gnosis_core::{
    client_http::{DeliveryStatus, HttpClient, HttpClientError, SecretToken},
    client_replica::{
        DeviceUploadKind, LocalReplica, OfflineAnswer, OfflineAssetCreate, OfflineDocumentEdit,
        OfflineResponse, OfflineReviewOutcome, PendingDeviceUpload,
    },
    client_review::{ReviewPackAcquisition, ReviewPackAcquisitionError},
    client_sync::{ReplicaSync, ReplicaSyncError},
    protocol::{
        DaemonConfig, DaemonServer, PROTOCOL_VERSION, Request, RequestEnvelope, ResponseOutcome,
        ResponseResult, ServerError,
        client_v1::{ClientDeliveryV1, ClientFailureCodeV1, ClientFailureV1},
        http_v1::{
            ApiErrorCodeV1, AssetHeadV1, DesiredStateMutationProjectionV1,
            DesiredStateMutationResultV1, DocumentImportRequestV1, DocumentMutationResultV1,
            NoteRenameRequestV1, NoteWriteRequestV1, ReviewAnswerV1, ReviewPackAssetV2,
            ReviewQueueSummaryRequestV1, ReviewSessionRequestV1, ReviewSourceInputV1,
            ReviewSourceKindV1, SetAcceptedSuccessRequestV1, SetSuspensionRequestV1,
            ThemaWriteRequestV1,
        },
        request,
    },
};
use serde_json::{Value, json};
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

fn schema_accepts_null(schema: &Value, components: &Value) -> bool {
    if schema == &Value::Bool(true) {
        return true;
    }
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        let name = reference.strip_prefix("#/components/schemas/").unwrap();
        return schema_accepts_null(&components[name], components);
    }
    if schema.get("type").is_some_and(|value| {
        value == "null"
            || value
                .as_array()
                .is_some_and(|types| types.iter().any(|item| item == "null"))
    }) {
        return true;
    }
    ["anyOf", "oneOf"].into_iter().any(|keyword| {
        schema[keyword].as_array().is_some_and(|variants| {
            variants
                .iter()
                .any(|variant| schema_accepts_null(variant, components))
        })
    })
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
async fn maximum_document_source_crosses_direct_and_offline_http_envelopes() {
    const MAX_DOCUMENT_SOURCE_BYTES: usize = 1_048_576;

    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(&daemon.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let prefix = "#+title: Maximum\n";
    let source = format!(
        "{prefix}{}",
        "\n".repeat(MAX_DOCUMENT_SOURCE_BYTES - prefix.len())
    );
    assert_eq!(source.len(), MAX_DOCUMENT_SOURCE_BYTES);

    client
        .import_document(
            "direct-maximum",
            "direct-maximum-operation",
            &DocumentImportRequestV1 {
                content_type: "text/org; charset=utf-8".into(),
                source: source.clone(),
                expected_revision: None,
            },
        )
        .await
        .unwrap();

    let mut replica = bootstrap_replica(&daemon, &client, &device_id).await;
    replica
        .record_document(OfflineDocumentEdit {
            operation_id: "offline-maximum-operation".into(),
            document_id: "offline-maximum".into(),
            logical_path: "offline-maximum.org".into(),
            org: source,
            authored_at_us: 10,
        })
        .unwrap();
    let upload = replica.next_device_upload().unwrap().unwrap();
    let result = client.upload(&upload).await.unwrap();
    assert_eq!(result.operation_id(), "offline-maximum-operation");

    daemon.stop().await;
}

#[tokio::test]
async fn client_runs_managed_asset_lifecycle_and_retains_blob() {
    let daemon = RunningDaemon::start().await;
    let (_, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(&daemon.base_url, SecretToken::new(raw_token).unwrap()).unwrap();
    let bytes = b"managed asset";
    let digest = format!("{:x}", Sha256::digest(bytes));

    let put = client
        .put_asset(&digest, "image.png", "asset-put", None, "image/png", bytes)
        .await
        .unwrap();
    let head = applied_asset(&put);
    assert_eq!(head.revision, 1);
    assert_eq!(client.asset(head, bytes.len() as u64).await.unwrap(), bytes);
    assert_eq!(client.blob(&digest).await.unwrap(), bytes);

    for (alias, put_operation_id, delete_operation_id) in [
        (
            "nested/image.png",
            "alias-nested-asset",
            "delete-nested-asset",
        ),
        ("./image.png", "alias-dot-asset", "delete-dot-asset"),
    ] {
        let put_alias = client
            .put_asset(
                &digest,
                alias,
                put_operation_id,
                Some(1),
                "image/png",
                bytes,
            )
            .await;
        assert!(
            matches!(put_alias, Err(HttpClientError::InvalidUpload)),
            "{alias}: {put_alias:?}"
        );
        assert!(matches!(
            client
                .tombstone_asset(&digest, alias, delete_operation_id, 1)
                .await,
            Err(HttpClientError::InvalidUpload)
        ));
    }
    assert_eq!(client.asset(head, bytes.len() as u64).await.unwrap(), bytes);

    let rejected = client
        .put_asset(
            &digest,
            "image.png",
            "asset-stale-put",
            Some(99),
            "image/png",
            bytes,
        )
        .await
        .unwrap();
    assert!(matches!(
        rejected,
        DesiredStateMutationResultV1::Rejected { operation_id, .. }
            if operation_id == "asset-stale-put"
    ));

    let deleted = client
        .tombstone_asset(&digest, "image.png", "asset-delete", head.revision)
        .await
        .unwrap();
    let deleted_head = applied_asset(&deleted);
    assert!(!deleted_head.active);
    assert_eq!(deleted_head.revision, 2);
    assert!(matches!(
        client.asset(head, bytes.len() as u64).await,
        Err(HttpClientError::Server(error))
            if error.body().error.code() == ApiErrorCodeV1::NotFound
    ));
    assert_eq!(client.blob(&digest).await.unwrap(), bytes);

    daemon.stop().await;
}

#[tokio::test]
async fn managed_asset_fetch_rejects_malformed_success_metadata() {
    let bytes = b"asset";
    let digest = format!("{:x}", Sha256::digest(bytes));
    let head = AssetHeadV1 {
        logical_path: format!("assets/{digest}/image.png"),
        revision: 7,
        content_sha256: Some(digest),
        media_type: Some("image/png".into()),
        active: true,
    };

    for response in [
        b"HTTP/1.1 201 Created\r\nContent-Type: image/png\r\nETag: \"7\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\nasset".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nETag: \"7\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\nasset".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nETag: \"8\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\nasset".as_slice(),
        b"HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nETag: \"7\"\r\nContent-Length: 5\r\nConnection: close\r\n\r\nother".as_slice(),
    ] {
        let raw = spawn_raw_server(response).await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();
        assert!(matches!(
            client.asset(&head, bytes.len() as u64).await,
            Err(HttpClientError::InvalidResponse { .. })
        ));
        raw.task.await.unwrap();
    }
}

#[tokio::test]
async fn managed_asset_mutations_reject_invalid_local_input_before_delivery() {
    let client = HttpClient::new("http://127.0.0.1:1", token()).unwrap();
    let bytes = b"asset";
    let digest = format!("{:x}", Sha256::digest(bytes));

    for result in [
        client
            .put_asset(&digest, "image.png", "put", None, "", bytes)
            .await,
        client
            .put_asset(&digest, "image.png", "put", None, "image/png", b"")
            .await,
        client
            .put_asset(&digest, "image.png", "put", None, "image/png ", bytes)
            .await,
    ] {
        assert!(matches!(result, Err(HttpClientError::InvalidUpload)));
    }
    assert!(matches!(
        client
            .put_asset(&digest, "image.png", "put", Some(0), "image/png", bytes)
            .await,
        Err(HttpClientError::InvalidMutation)
    ));
    assert!(matches!(
        client
            .tombstone_asset(&digest, "image.png", "delete", 0)
            .await,
        Err(HttpClientError::InvalidMutation)
    ));

    let oversized = vec![b'x'; 1024 * 1024 + 1];
    let oversized_digest = format!("{:x}", Sha256::digest(&oversized));
    assert!(matches!(
        client
            .put_asset(
                &oversized_digest,
                "image.png",
                "put-large",
                None,
                "image/png",
                &oversized,
            )
            .await,
        Err(HttpClientError::InvalidUpload)
    ));
}

#[tokio::test]
async fn dashboard_null_response_matches_required_nullable_openapi_contract() {
    let daemon = RunningDaemon::start().await;
    let (_, raw_token) = create_device(&daemon.socket).await;
    let _typed_client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let client = raw_http_client();
    let dashboard_response = client
        .get(format!("{}/v1/dashboard", daemon.base_url))
        .bearer_auth(&raw_token)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let dashboard: Value =
        serde_json::from_slice(&dashboard_response.bytes().await.unwrap()).unwrap();
    let openapi_response = client
        .get(format!("{}/v1/openapi.json", daemon.base_url))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    let openapi: Value = serde_json::from_slice(&openapi_response.bytes().await.unwrap()).unwrap();
    let schemas = &openapi["components"]["schemas"];
    let dashboard_schema = &schemas["DashboardV1"];
    let active_session_schema = &dashboard_schema["properties"]["active_session"];

    assert_eq!(dashboard["active_session"], Value::Null);
    assert!(
        dashboard_schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .any(|field| field == "active_session")
    );
    assert!(schema_accepts_null(active_session_schema, schemas));
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
        .unwrap()
        .attempt
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
async fn client_reports_the_appended_online_relearning_item() {
    let daemon = RunningDaemon::start().await;
    let (_, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let thema_id = create_review_thema(&daemon.base_url, &raw_token).await;
    let session = client
        .create_review_session(&ReviewSessionRequestV1 {
            source: manual_source(thema_id.clone()),
            target_count: Some(1),
        })
        .await
        .unwrap();
    let attempt = client
        .begin_review_attempt(&session.session_id)
        .await
        .unwrap()
        .attempt
        .unwrap();
    let graded = client
        .answer_review_attempt(&attempt.attempt_id, &ReviewAnswerV1::Text("wrong".into()))
        .await
        .unwrap();
    assert_eq!(graded.detected_success, Some(false));
    client
        .commit_review_attempt(&attempt.attempt_id)
        .await
        .unwrap();

    let active = client
        .active_review_session()
        .await
        .unwrap()
        .session
        .unwrap();
    assert_eq!(active.session_id, session.session_id);
    assert_eq!(active.target_count, Some(1));
    assert_eq!((active.current_position, active.queue_length), (1, 2));
    assert_eq!(
        client
            .begin_review_attempt(&active.session_id)
            .await
            .unwrap()
            .attempt
            .unwrap()
            .thema_id,
        thema_id
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
    assert_no_active_session(&client).await;
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
            "--json-errors",
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
    let error: ClientFailureV1 = serde_json::from_slice(&uncertain.stderr).unwrap();
    assert_eq!(error.code, ClientFailureCodeV1::Http);
    assert_eq!(error.delivery, ClientDeliveryV1::Unknown);
    assert_eq!(error.operation_id.as_deref(), Some("cli-accept"));
    assert!(error.api_error.is_none());
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
            "--json-errors",
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
    let error: ClientFailureV1 = serde_json::from_slice(&output_failure.stderr).unwrap();
    assert_eq!(error.code, ClientFailureCodeV1::Output);
    assert_eq!(error.delivery, ClientDeliveryV1::Confirmed);
    assert_eq!(error.operation_id.as_deref(), Some("cli-suspend"));

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
        &["--json-errors", "review-attempt-state", "missing-attempt"],
    )
    .await;
    assert!(!missing.status.success());
    let error: ClientFailureV1 = serde_json::from_slice(&missing.stderr).unwrap();
    assert_eq!(error.code, ClientFailureCodeV1::Server);
    assert_eq!(error.delivery, ClientDeliveryV1::Confirmed);
    assert_eq!(
        error.api_error.unwrap().error.code(),
        ApiErrorCodeV1::NotFound
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

    let invalid_operation = "x".repeat(129);
    let invalid = run_cli(
        &daemon.base_url,
        &token_path,
        &[
            "--json-errors",
            "review-attempt-set-accepted",
            &attempt_id,
            "--operation-id",
            &invalid_operation,
            "--expected-revision",
            "0",
            "--accepted-success",
            "true",
        ],
    )
    .await;
    assert!(!invalid.status.success());
    let failure: ClientFailureV1 = serde_json::from_slice(&invalid.stderr).unwrap();
    assert_eq!(failure.code, ClientFailureCodeV1::Input);
    assert_eq!(failure.delivery, ClientDeliveryV1::NotSent);
    assert!(failure.operation_id.is_none());

    let invalid_token_path = daemon._directory.path().join("invalid.token");
    write_token(&invalid_token_path, "invalid");
    let invalid_token = run_cli(
        &daemon.base_url,
        &invalid_token_path,
        &["--json-errors", "review-attempt-state", &attempt_id],
    )
    .await;
    let failure: ClientFailureV1 = serde_json::from_slice(&invalid_token.stderr).unwrap();
    assert_eq!(failure.code, ClientFailureCodeV1::Credentials);
    assert_eq!(failure.delivery, ClientDeliveryV1::NotSent);
    daemon.stop().await;
}

#[tokio::test]
async fn cli_sync_bootstraps_and_reuses_a_durable_http_replica() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let _client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let thema_id = create_review_thema(&daemon.base_url, &raw_token).await;
    let token_path = daemon._directory.path().join("sync.token");
    let replica_path = daemon._directory.path().join("cli-replica.sqlite");
    fs::write(&token_path, format!("{raw_token}\n")).unwrap();
    fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
    let replica = replica_path.to_str().unwrap();

    let first = run_cli(
        &daemon.base_url,
        &token_path,
        &["sync", "--replica", replica],
    )
    .await;
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    assert!(first["snapshot_pages"].as_u64().unwrap() > 0);
    assert_eq!(first["snapshot_recoveries"], 0);
    let replica_store = LocalReplica::open(&replica_path, &device_id).unwrap();
    assert!(
        replica_store
            .projection("thema", &thema_id)
            .unwrap()
            .is_some()
    );
    drop(replica_store);

    let output_failure = run_cli_to_full(
        &daemon.base_url,
        &token_path,
        &["--json-errors", "sync", "--replica", replica],
    )
    .await;
    assert!(!output_failure.status.success());
    let failure: ClientFailureV1 = serde_json::from_slice(&output_failure.stderr).unwrap();
    assert_eq!(failure.code, ClientFailureCodeV1::Output);
    assert_eq!(failure.delivery, ClientDeliveryV1::Confirmed);
    assert!(failure.operation_id.is_none());

    let second = run_cli(
        &daemon.base_url,
        &token_path,
        &["sync", "--replica", replica],
    )
    .await;
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let second: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(second["snapshot_pages"], 0);
    assert_eq!(second["snapshot_recoveries"], 0);
    assert_eq!(second["downloaded_blobs"], 0);
    assert_eq!(second["uploaded_operations"], 0);
    daemon.stop().await;
}

#[tokio::test]
async fn cli_review_pack_answers_offline_replays_and_reconciles() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let _client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let first_thema = create_review_thema(&daemon.base_url, &raw_token).await;
    let second_thema = create_review_thema(&daemon.base_url, &raw_token).await;
    let token_path = daemon._directory.path().join("review-pack.token");
    let replica_path = daemon._directory.path().join("review-pack.sqlite");
    fs::write(&token_path, format!("{raw_token}\n")).unwrap();
    fs::set_permissions(&token_path, fs::Permissions::from_mode(0o600)).unwrap();
    let replica = replica_path.to_str().unwrap();

    let synced = run_cli(
        &daemon.base_url,
        &token_path,
        &["sync", "--replica", replica],
    )
    .await;
    assert!(synced.status.success());
    let acquire = json!({
        "version": 1,
        "session": {
            "source": {
                "kind": "manual",
                "tags": [],
                "exclude_tags": [],
                "topic_node_id": null,
                "topic_forward_depth": null,
                "topic_backward_depth": null,
                "manual_ids": [first_thema, second_thema]
            },
            "target_count": null
        }
    });
    let lost_acquire = run_cli_with_input_to_full(
        &daemon.base_url,
        &token_path,
        &[
            "--json-errors",
            "review-pack",
            "acquire",
            "--replica",
            replica,
        ],
        &serde_json::to_vec(&acquire).unwrap(),
    )
    .await;
    assert!(!lost_acquire.status.success());
    let failure: ClientFailureV1 = serde_json::from_slice(&lost_acquire.stderr).unwrap();
    assert_eq!(failure.code, ClientFailureCodeV1::Output);
    assert_eq!(failure.delivery, ClientDeliveryV1::Confirmed);
    assert!(failure.operation_id.is_none());

    let acquired = run_cli(
        &daemon.base_url,
        &token_path,
        &["review-pack", "resume", "--replica", replica],
    )
    .await;
    assert!(
        acquired.status.success(),
        "{}",
        String::from_utf8_lossy(&acquired.stderr)
    );
    let current: serde_json::Value = serde_json::from_slice(&acquired.stdout).unwrap();
    assert_eq!(current["question"]["session_position"], 0);
    assert!(current["question"]["presentation"].get("answer").is_none());
    let pack_id = current["question"]["pack_id"].as_str().unwrap();

    let offline = "http://127.0.0.1:1";
    let reopened =
        run_cli_without_token(offline, &["review-pack", "current", "--replica", replica]).await;
    assert!(reopened.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&reopened.stdout).unwrap(),
        current
    );
    let answered_at_us = u64::try_from(chrono::Utc::now().timestamp_micros()).unwrap();
    let first_answer = json!({
        "version": 1,
        "pack_id": pack_id,
        "attempt_id": "cli-offline-attempt-1",
        "session_position": 0,
        "response": {"kind": "text", "value": "Answer"},
        "accepted_success": null,
        "desired_suspended": false,
        "answered_at_us": answered_at_us
    });
    let first_bytes = serde_json::to_vec(&first_answer).unwrap();
    let lost = run_cli_without_token_with_input_to_full(
        offline,
        &[
            "--json-errors",
            "review-pack",
            "answer",
            "--replica",
            replica,
        ],
        &first_bytes,
    )
    .await;
    assert!(!lost.status.success());
    let failure: ClientFailureV1 = serde_json::from_slice(&lost.stderr).unwrap();
    assert_eq!(failure.code, ClientFailureCodeV1::Output);
    assert_eq!(failure.delivery, ClientDeliveryV1::Confirmed);
    assert_eq!(
        failure.operation_id.as_deref(),
        Some("cli-offline-attempt-1")
    );

    let replayed = run_cli_without_token_with_input(
        offline,
        &["review-pack", "answer", "--replica", replica],
        &first_bytes,
    )
    .await;
    assert!(replayed.status.success());
    let first_receipt: serde_json::Value = serde_json::from_slice(&replayed.stdout).unwrap();
    assert_eq!(first_receipt["attempt_id"], "cli-offline-attempt-1");
    assert_eq!(first_receipt["question"]["session_position"], 1);
    let mut changed_answer = first_answer.clone();
    changed_answer["response"]["value"] = json!("changed");
    let changed = run_cli_without_token_with_input(
        offline,
        &["review-pack", "answer", "--replica", replica],
        &serde_json::to_vec(&changed_answer).unwrap(),
    )
    .await;
    assert!(!changed.status.success());
    let unchanged =
        run_cli_without_token(offline, &["review-pack", "current", "--replica", replica]).await;
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&unchanged.stdout).unwrap()["question"]["session_position"],
        1
    );

    let second_answer = json!({
        "version": 1,
        "pack_id": pack_id,
        "attempt_id": "cli-offline-attempt-2",
        "session_position": 1,
        "response": {"kind": "text", "value": "Answer"},
        "accepted_success": null,
        "desired_suspended": false,
        "answered_at_us": answered_at_us + 1
    });
    let second = run_cli_without_token_with_input(
        offline,
        &["review-pack", "answer", "--replica", replica],
        &serde_json::to_vec(&second_answer).unwrap(),
    )
    .await;
    assert!(second.status.success());
    let second_receipt: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(
        second_receipt["device_sequence"].as_u64().unwrap(),
        first_receipt["device_sequence"].as_u64().unwrap() + 1
    );
    assert!(second_receipt["question"].is_null());

    let reconciled = run_cli(
        &daemon.base_url,
        &token_path,
        &["sync", "--replica", replica],
    )
    .await;
    assert!(reconciled.status.success());
    let result_request = json!({
        "version": 1,
        "after": null,
        "limit": 10,
        "conflicts_only": false
    });
    let local_results = run_cli_without_token_with_input(
        offline,
        &["review-pack", "results", "--replica", replica],
        &serde_json::to_vec(&result_request).unwrap(),
    )
    .await;
    assert!(local_results.status.success());
    let page: serde_json::Value = serde_json::from_slice(&local_results.stdout).unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 2);
    assert!(page["items"].as_array().unwrap().iter().all(|item| {
        item["status"] == "accepted"
            && item.get("operation").is_none()
            && item.get("presentation").is_none()
            && item.get("answer").is_none()
    }));
    let replica = LocalReplica::open(&replica_path, &device_id).unwrap();
    assert!(replica.next_device_upload().unwrap().is_none());
    let results = replica.review_results(None, 10).unwrap();
    assert_eq!(results.items.len(), 2);
    assert!(
        results
            .items
            .iter()
            .all(|result| result.outcome == OfflineReviewOutcome::Accepted)
    );
    daemon.stop().await;
}

#[tokio::test]
async fn cli_review_pack_surfaces_conflicts_without_captured_answers() {
    let daemon = RunningDaemon::start().await;
    let (_, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let thema_id = create_review_thema(&daemon.base_url, &raw_token).await;
    let token_path = daemon._directory.path().join("conflict.token");
    let replica_path = daemon._directory.path().join("conflict.sqlite");
    write_token(&token_path, &raw_token);
    let replica = replica_path.to_str().unwrap();
    assert!(
        run_cli(
            &daemon.base_url,
            &token_path,
            &["sync", "--replica", replica]
        )
        .await
        .status
        .success()
    );
    let acquire = json!({
        "version": 1,
        "session": {"source": manual_source(thema_id), "target_count": null}
    });
    let acquired = run_cli_with_input(
        &daemon.base_url,
        &token_path,
        &["review-pack", "acquire", "--replica", replica],
        &serde_json::to_vec(&acquire).unwrap(),
    )
    .await;
    assert!(acquired.status.success());
    let current: serde_json::Value = serde_json::from_slice(&acquired.stdout).unwrap();
    let pack_id = current["question"]["pack_id"].as_str().unwrap();
    let answer = json!({
        "version": 1,
        "pack_id": pack_id,
        "attempt_id": "cli-conflicting-attempt",
        "session_position": 0,
        "response": {"kind": "text", "value": "Answer"},
        "accepted_success": null,
        "desired_suspended": false,
        "answered_at_us": u64::try_from(chrono::Utc::now().timestamp_micros()).unwrap()
    });
    assert!(
        run_cli_without_token_with_input(
            "http://127.0.0.1:1",
            &["review-pack", "answer", "--replica", replica],
            &serde_json::to_vec(&answer).unwrap(),
        )
        .await
        .status
        .success()
    );
    client.abandon_review_pack(pack_id).await.unwrap();
    assert!(
        run_cli(
            &daemon.base_url,
            &token_path,
            &["sync", "--replica", replica]
        )
        .await
        .status
        .success()
    );
    let request = json!({
        "version": 1,
        "after": null,
        "limit": 10,
        "conflicts_only": true
    });
    let result = run_cli_without_token_with_input(
        "http://127.0.0.1:1",
        &["review-pack", "results", "--replica", replica],
        &serde_json::to_vec(&request).unwrap(),
    )
    .await;

    assert!(result.status.success());
    let page: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(page["items"][0]["status"], "conflict");
    assert_eq!(page["items"][0]["conflict_reason"], "lease_ended");
    assert_eq!(page["items"][0]["attempt_id"], "cli-conflicting-attempt");
    assert!(
        !result
            .stdout
            .windows(b"presentation".len())
            .any(|value| value == b"presentation")
    );
    assert!(
        !result
            .stdout
            .windows(b"answer".len())
            .any(|value| value == b"answer")
    );
    daemon.stop().await;
}

#[tokio::test]
async fn cli_review_pack_rejects_another_device_before_creating_a_lease() {
    let daemon = RunningDaemon::start().await;
    let (first_id, first_token) = create_device(&daemon.socket).await;
    let (_, second_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(first_token.clone()).unwrap(),
    )
    .unwrap();
    let thema_id = create_review_thema(&daemon.base_url, &first_token).await;
    let first_path = daemon._directory.path().join("first.token");
    let second_path = daemon._directory.path().join("second.token");
    let replica_path = daemon._directory.path().join("identity-replica.sqlite");
    write_token(&first_path, &first_token);
    write_token(&second_path, &second_token);
    let replica = replica_path.to_str().unwrap();
    assert!(
        run_cli(
            &daemon.base_url,
            &first_path,
            &["sync", "--replica", replica]
        )
        .await
        .status
        .success()
    );
    let before = LocalReplica::open(&replica_path, &first_id)
        .unwrap()
        .status()
        .unwrap();
    let input = json!({
        "version": 1,
        "session": {"source": manual_source(thema_id), "target_count": null}
    });

    let rejected = run_cli_with_input(
        &daemon.base_url,
        &second_path,
        &["review-pack", "acquire", "--replica", replica],
        &serde_json::to_vec(&input).unwrap(),
    )
    .await;

    assert!(!rejected.status.success());
    assert_eq!(
        LocalReplica::open(&replica_path, &first_id)
            .unwrap()
            .status()
            .unwrap(),
        before
    );
    assert_no_active_session(&client).await;
    daemon.stop().await;
}

#[tokio::test]
async fn cli_review_pack_resumes_unknown_creation_and_abandons_the_active_lease() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let asset = b"cli review pack asset".to_vec();
    let asset_path = put_review_asset(&daemon.base_url, &raw_token, &asset).await;
    let thema_id = create_review_thema_with_asset(&daemon.base_url, &raw_token, &asset_path).await;
    let token_path = daemon._directory.path().join("resume.token");
    let replica_path = daemon._directory.path().join("resume-replica.sqlite");
    write_token(&token_path, &raw_token);
    let replica = replica_path.to_str().unwrap();
    assert!(
        run_cli(
            &daemon.base_url,
            &token_path,
            &["sync", "--replica", replica]
        )
        .await
        .status
        .success()
    );
    let mut local = LocalReplica::open(&replica_path, &device_id).unwrap();
    local
        .record_document(OfflineDocumentEdit {
            operation_id: "pack-lifecycle-pending-operation".into(),
            document_id: "pack-lifecycle-pending-document".into(),
            logical_path: "pack-lifecycle-pending.org".into(),
            org: "#+title: Pending during pack lifecycle\n".into(),
            authored_at_us: 10,
        })
        .unwrap();
    drop(local);
    let input = json!({
        "version": 1,
        "session": {"source": manual_source(thema_id), "target_count": null}
    });
    let proxy = spawn_drop_path_response_proxy(daemon.http_addr, "/v1/review/packs", 1).await;
    let interrupted = run_cli_with_input(
        &proxy.base_url,
        &token_path,
        &["review-pack", "acquire", "--replica", replica],
        &serde_json::to_vec(&input).unwrap(),
    )
    .await;
    assert!(!interrupted.status.success());
    assert_eq!(proxy.stop().await, 1);
    assert_eq!(
        LocalReplica::open(&replica_path, &device_id)
            .unwrap()
            .next_device_upload()
            .unwrap()
            .unwrap()
            .operation_id(),
        "pack-lifecycle-pending-operation"
    );

    let resumed = run_cli(
        &daemon.base_url,
        &token_path,
        &["review-pack", "resume", "--replica", replica],
    )
    .await;
    assert!(resumed.status.success());
    let replica_store = LocalReplica::open(&replica_path, &device_id).unwrap();
    let pack = replica_store.active_review_pack().unwrap().unwrap();
    assert_eq!(
        replica_store.blob(&pack.assets[0].content_sha256).unwrap(),
        Some(asset.clone())
    );
    drop(replica_store);
    let asset_request = json!({
        "version": 1,
        "pack_id": pack.pack_id.clone(),
        "logical_path": asset_path
    });
    let local_asset = run_cli_without_token_with_input(
        "http://127.0.0.1:1",
        &["review-pack", "asset", "--replica", replica],
        &serde_json::to_vec(&asset_request).unwrap(),
    )
    .await;
    assert!(local_asset.status.success());
    let local_asset: serde_json::Value = serde_json::from_slice(&local_asset.stdout).unwrap();
    assert_eq!(local_asset["content_sha256"], pack.assets[0].content_sha256);
    assert_eq!(
        BASE64
            .decode(local_asset["data_base64"].as_str().unwrap())
            .unwrap(),
        asset
    );

    let abandoned = run_cli(
        &daemon.base_url,
        &token_path,
        &["review-pack", "abandon", "--replica", replica],
    )
    .await;
    assert!(abandoned.status.success());
    let receipt: serde_json::Value = serde_json::from_slice(&abandoned.stdout).unwrap();
    assert_eq!(receipt["pack_id"], pack.pack_id);
    assert!(
        LocalReplica::open(&replica_path, &device_id)
            .unwrap()
            .active_review_pack()
            .unwrap()
            .is_none()
    );
    assert_no_active_session(&client).await;
    assert_eq!(
        LocalReplica::open(&replica_path, &device_id)
            .unwrap()
            .next_device_upload()
            .unwrap()
            .unwrap()
            .operation_id(),
        "pack-lifecycle-pending-operation"
    );
    assert!(
        run_cli(
            &daemon.base_url,
            &token_path,
            &["sync", "--replica", replica]
        )
        .await
        .status
        .success()
    );
    assert!(
        LocalReplica::open(&replica_path, &device_id)
            .unwrap()
            .next_device_upload()
            .unwrap()
            .is_none()
    );
    daemon.stop().await;
}

#[tokio::test]
async fn cli_review_pack_abandon_releases_a_lease_without_fetching_assets() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let asset = b"unavailable during abandon".to_vec();
    let asset_path = put_review_asset(&daemon.base_url, &raw_token, &asset).await;
    let thema_id = create_review_thema_with_asset(&daemon.base_url, &raw_token, &asset_path).await;
    let replica = bootstrap_replica(&daemon, &client, &device_id).await;
    drop(replica);
    let pack = client
        .create_review_pack(&ReviewSessionRequestV1 {
            source: manual_source(thema_id),
            target_count: None,
        })
        .await
        .unwrap();
    let pack_id = pack.pack.pack_id;
    let token_path = daemon._directory.path().join("abandon.token");
    write_token(&token_path, &raw_token);
    let replica_path = daemon._directory.path().join("replica.sqlite");
    let proxy = spawn_drop_path_response_proxy(daemon.http_addr, "/assets/", 1).await;

    let abandoned = run_cli(
        &proxy.base_url,
        &token_path,
        &[
            "review-pack",
            "abandon",
            "--replica",
            replica_path.to_str().unwrap(),
        ],
    )
    .await;

    assert!(
        abandoned.status.success(),
        "{}",
        String::from_utf8_lossy(&abandoned.stderr)
    );
    assert_eq!(proxy.stop().await, 0);
    assert_eq!(
        LocalReplica::open(&replica_path, &device_id)
            .unwrap()
            .review_pack(&pack_id)
            .unwrap()
            .unwrap()
            .status,
        "abandoned"
    );
    assert_no_active_session(&client).await;
    daemon.stop().await;
}

#[tokio::test]
async fn cli_review_pack_rejects_oversized_input_before_opening_a_replica() {
    let input = vec![b'x'; 1024 * 1024 + 1];
    let result = run_cli_without_token_with_input(
        "http://127.0.0.1:1",
        &[
            "--json-errors",
            "review-pack",
            "answer",
            "--replica",
            "/definitely/missing/replica.sqlite",
        ],
        &input,
    )
    .await;

    assert!(!result.status.success());
    let failure: ClientFailureV1 = serde_json::from_slice(&result.stderr).unwrap();
    assert_eq!(failure.code, ClientFailureCodeV1::Input);
    assert_eq!(failure.delivery, ClientDeliveryV1::NotSent);
    assert!(failure.operation_id.is_none());
    assert!(failure.api_error.is_none());

    let invalid_cursor = json!({
        "version": 1,
        "after": i64::MAX as u64 + 1,
        "limit": 10,
        "conflicts_only": false
    });
    let result = run_cli_without_token_with_input(
        "http://127.0.0.1:1",
        &[
            "--json-errors",
            "review-pack",
            "results",
            "--replica",
            "/definitely/missing/replica.sqlite",
        ],
        &serde_json::to_vec(&invalid_cursor).unwrap(),
    )
    .await;
    let failure: ClientFailureV1 = serde_json::from_slice(&result.stderr).unwrap();
    assert_eq!(failure.code, ClientFailureCodeV1::Input);
}

#[tokio::test]
async fn authenticated_sync_identity_matches_the_token_owner() {
    let daemon = RunningDaemon::start().await;
    let (device_id, raw_token) = create_device(&daemon.socket).await;
    let _client = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(raw_token.clone()).unwrap(),
    )
    .unwrap();
    let response = raw_http_client()
        .get(format!("{}/v1/sync/identity", daemon.base_url))
        .bearer_auth(raw_token)
        .send()
        .await
        .unwrap();

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: serde_json::Value = serde_json::from_slice(&response.bytes().await.unwrap()).unwrap();
    assert_eq!(body, json!({"version": 1, "device_id": device_id}));
    daemon.stop().await;
}

#[tokio::test]
async fn projection_clients_reject_non_strict_responses() {
    let mut import = document_mutation_json(1, "operation", "document");
    let mut import_missing = import.clone();
    import_missing.as_object_mut().unwrap().remove("cursor");
    import["unexpected"] = json!(true);
    for response in [import, import_missing] {
        let raw = spawn_json_server(response).await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();
        assert!(matches!(
            client
                .import_document(
                    "document",
                    "operation",
                    &DocumentImportRequestV1 {
                        content_type: "text/org; charset=utf-8".into(),
                        source: "#+TITLE: Document\n".into(),
                        expected_revision: None,
                    },
                )
                .await,
            Err(HttpClientError::InvalidResponse { .. })
        ));
        raw.task.await.unwrap();
    }

    let mut document = json!({
        "document_id": "document",
        "revision": 1,
        "title": "Document",
        "file_node_id": null,
        "file_tags": [],
        "nodes": [],
        "link_occurrences": [],
        "semantic_edges": [],
        "content_sha256": "0".repeat(64)
    });
    let mut document_missing = document.clone();
    document_missing
        .as_object_mut()
        .unwrap()
        .remove("file_node_id");
    let mut document_duplicate_tags = document.clone();
    document_duplicate_tags["file_tags"] = json!(["tag", "tag"]);
    document["unexpected"] = json!(true);
    for response in [document, document_missing, document_duplicate_tags] {
        let raw = spawn_json_server(response).await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();
        assert!(matches!(
            client.document("document").await,
            Err(HttpClientError::InvalidResponse { .. })
        ));
        raw.task.await.unwrap();
    }

    let mut thema = json!({
        "thema_id": "thema",
        "revision": 1,
        "kind": "basic",
        "keimenon": "Question",
        "hypothesis": [null],
        "answer": ["Answer"],
        "source_guid": null,
        "parathema": null,
        "review_image": null,
        "tags": [],
        "concepts": [],
        "asset_paths": [],
        "content_sha256": "0".repeat(64),
        "archived": false
    });
    let mut thema_missing = thema.clone();
    thema_missing.as_object_mut().unwrap().remove("source_guid");
    thema["unexpected"] = json!(true);
    for response in [thema, thema_missing] {
        let raw = spawn_json_server(response).await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();
        assert!(matches!(
            client.thema("thema").await,
            Err(HttpClientError::InvalidResponse { .. })
        ));
        raw.task.await.unwrap();
    }

    for due in [
        json!({"items": []}),
        json!({"items": [], "next": null, "unexpected": true}),
    ] {
        let raw = spawn_json_server(due).await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();
        assert!(matches!(
            client.due_page(20260101, 10, None).await,
            Err(HttpClientError::InvalidResponse { .. })
        ));
        raw.task.await.unwrap();
    }
}

#[tokio::test]
async fn review_session_client_enforces_request_and_normalized_response_targets() {
    let source = ReviewSourceInputV1 {
        kind: ReviewSourceKindV1::Manual,
        tags: vec![],
        exclude_tags: vec![],
        topic_node_id: None,
        topic_forward_depth: None,
        topic_backward_depth: None,
        manual_ids: vec!["thema".into()],
    };
    let unavailable = HttpClient::new("http://127.0.0.1:1/", token()).unwrap();
    for target_count in [0, 501] {
        let error = unavailable
            .create_review_session(&ReviewSessionRequestV1 {
                source: source.clone(),
                target_count: Some(target_count),
            })
            .await
            .unwrap_err();
        assert!(matches!(error, HttpClientError::InvalidReview));
        assert_eq!(error.delivery(), DeliveryStatus::NotSent);
    }

    let oversized_source = ReviewSourceInputV1 {
        manual_ids: (0..=500).map(|index| format!("thema-{index}")).collect(),
        ..source.clone()
    };
    let error = unavailable
        .create_review_session(&ReviewSessionRequestV1 {
            source: oversized_source,
            target_count: Some(1),
        })
        .await
        .unwrap_err();
    assert!(matches!(error, HttpClientError::InvalidReview));
    assert_eq!(error.delivery(), DeliveryStatus::NotSent);

    for target_count in [Some(10), None, Some(9)] {
        let raw = spawn_json_response(
            "201 Created",
            json!({
                "session_id": "session",
                "mode": "online",
                "source": {
                    "kind": "manual",
                    "tags": [],
                    "exclude_tags": [],
                    "topic_node_id": null,
                    "topic_forward_depth": null,
                    "topic_backward_depth": null,
                    "manual_ids": ["thema"]
                },
                "target_count": target_count,
                "status": "active",
                "current_position": 0,
                "queue_length": 1,
                "started_at_us": 1,
                "owner_device_id": null
            }),
        )
        .await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();
        let result = client
            .create_review_session(&ReviewSessionRequestV1 {
                source: source.clone(),
                target_count: Some(10),
            })
            .await;
        if target_count == Some(10) {
            assert_eq!(result.unwrap().target_count, Some(10));
        } else {
            assert!(matches!(
                result.unwrap_err(),
                HttpClientError::InvalidResponse {
                    delivery: DeliveryStatus::Unknown
                }
            ));
        }
        raw.task.await.unwrap();
    }
}

#[tokio::test]
async fn review_clients_reject_uncorrelated_success_responses() {
    let source = manual_source("thema".into());
    let session = |source_id: &str, status: &str| {
        json!({
            "session_id": "session",
            "mode": "online",
            "source": {
                "kind": "manual",
                "tags": [],
                "exclude_tags": [],
                "topic_node_id": null,
                "topic_forward_depth": null,
                "topic_backward_depth": null,
                "manual_ids": [source_id]
            },
            "target_count": 1,
            "status": status,
            "current_position": 0,
            "queue_length": 1,
            "started_at_us": 1,
            "owner_device_id": null
        })
    };

    for response in [session("other", "active"), session("thema", "quit")] {
        let raw = spawn_json_response("201 Created", response).await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();
        assert_invalid_response(
            client
                .create_review_session(&ReviewSessionRequestV1 {
                    source: source.clone(),
                    target_count: Some(1),
                })
                .await,
        );
        raw.task.await.unwrap();
    }

    let raw = spawn_json_server(json!({
        "source": {
            "kind": "manual",
            "tags": [],
            "exclude_tags": [],
            "topic_node_id": null,
            "topic_forward_depth": null,
            "topic_backward_depth": null,
            "manual_ids": ["other"]
        },
        "matched_count": 1
    }))
    .await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    assert_invalid_response(
        client
            .review_queue_summary(&ReviewQueueSummaryRequestV1 {
                source: source.clone(),
            })
            .await,
    );
    raw.task.await.unwrap();

    let mut attempt = review_operation_json(1, "operation", "attempt", 2)["attempt"].clone();
    attempt["response"] = json!({"kind": "text", "value": "other"});
    let raw = spawn_json_server(attempt).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    assert_invalid_response(
        client
            .answer_review_attempt("attempt", &ReviewAnswerV1::Text("Answer".into()))
            .await,
    );
    raw.task.await.unwrap();

    let raw = spawn_json_server(json!({
        "attempt_id": "other-attempt",
        "event_id": "event"
    }))
    .await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    assert_invalid_response(client.commit_review_attempt("attempt").await);
    raw.task.await.unwrap();

    let raw = spawn_json_server(review_operation_json(1, "operation", "attempt", 2)).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    assert_invalid_response(
        client
            .set_review_accepted_success(
                "attempt",
                &SetAcceptedSuccessRequestV1 {
                    version: 1,
                    operation_id: "operation".into(),
                    expected_attempt_revision: 1,
                    accepted_success: false,
                },
            )
            .await,
    );
    raw.task.await.unwrap();

    let raw = spawn_json_server(review_operation_json(1, "operation", "attempt", 2)).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    assert_invalid_response(
        client
            .set_review_suspension(
                "attempt",
                &SetSuspensionRequestV1 {
                    version: 1,
                    operation_id: "operation".into(),
                    expected_attempt_revision: 1,
                    desired_suspended: true,
                },
            )
            .await,
    );
    raw.task.await.unwrap();
}

#[tokio::test]
async fn review_client_rejects_oversized_cloze_answer_before_transport() {
    let client = HttpClient::new("http://127.0.0.1:1", token()).unwrap();
    let answer = ReviewAnswerV1::Texts(vec!["answer".into(); 501]);

    assert!(matches!(
        client.answer_review_attempt("attempt", &answer).await,
        Err(HttpClientError::InvalidReview)
    ));
}

#[tokio::test]
async fn projection_clients_reject_semantic_mismatches() {
    let input = DocumentImportRequestV1 {
        content_type: "text/org; charset=utf-8".into(),
        source: "#+TITLE: Expected\n".into(),
        expected_revision: None,
    };
    for response in [
        document_mutation_json(1, "operation", "other"),
        document_mutation_json(1, "other-operation", "document"),
    ] {
        let raw = spawn_json_server(response).await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();
        assert!(matches!(
            client
                .import_document("document", "operation", &input)
                .await,
            Err(HttpClientError::InvalidResponse { .. })
        ));
        raw.task.await.unwrap();
    }

    for response in [
        json!({
            "document_id": "other",
            "revision": 1,
            "title": "Title",
            "file_node_id": null,
            "file_tags": [],
            "nodes": [],
            "link_occurrences": [],
            "semantic_edges": [],
            "content_sha256": "0".repeat(64)
        }),
        json!({
            "document_id": "document",
            "revision": 0,
            "title": "Title",
            "file_node_id": null,
            "file_tags": [],
            "nodes": [],
            "link_occurrences": [],
            "semantic_edges": [],
            "content_sha256": "0".repeat(64)
        }),
        json!({
            "document_id": "document",
            "revision": 1,
            "title": " ",
            "file_node_id": null,
            "file_tags": [],
            "nodes": [],
            "link_occurrences": [],
            "semantic_edges": [],
            "content_sha256": "0".repeat(64)
        }),
        json!({
            "document_id": "document",
            "revision": 1,
            "title": "Title",
            "file_node_id": null,
            "file_tags": [],
            "nodes": [{
                "id": "node",
                "title": "Node",
                "title_path": ["Other"],
                "tags": [],
                "parent_id": null,
                "level": 1
            }],
            "link_occurrences": [],
            "semantic_edges": [],
            "content_sha256": "0".repeat(64)
        }),
        json!({
            "document_id": "document",
            "revision": 1,
            "title": "x".repeat(1024 * 1024),
            "file_node_id": null,
            "file_tags": [],
            "nodes": [],
            "link_occurrences": [],
            "semantic_edges": [],
            "content_sha256": "0".repeat(64)
        }),
    ] {
        let raw = spawn_json_server(response).await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();
        assert!(matches!(
            client.document("document").await,
            Err(HttpClientError::InvalidResponse { .. })
                | Err(HttpClientError::ResponseTooLarge { .. })
        ));
        raw.task.await.unwrap();
    }

    let thema = json!({
        "thema_id": "other",
        "revision": 1,
        "kind": "basic",
        "keimenon": "Question",
        "hypothesis": [],
        "answer": ["Answer"],
        "source_guid": null,
        "parathema": null,
        "review_image": null,
        "tags": [],
        "concepts": [],
        "asset_paths": [],
        "content_sha256": "0".repeat(64),
        "archived": false
    });
    let raw = spawn_json_server(thema).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();
    assert!(matches!(
        client.thema("thema").await,
        Err(HttpClientError::InvalidResponse { .. })
    ));
    raw.task.await.unwrap();

    let item = |id: &str, day: u32| {
        json!({
            "id": id,
            "kind": "basic",
            "keimenon": "Question",
            "hypothesis": [],
            "answer": ["Answer"],
            "next_review_day": day,
            "review_count": 0
        })
    };
    for response in [
        json!({"items": [item("b", 1), item("a", 1)], "next": null}),
        json!({"items": [item("a", 1), item("b", 1)], "next": null}),
    ] {
        let raw = spawn_json_server(response).await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();
        assert!(matches!(
            client.due_page(20260101, 1, None).await,
            Err(HttpClientError::InvalidResponse { .. })
        ));
        raw.task.await.unwrap();
    }
}

#[tokio::test]
async fn device_identity_client_rejects_invalid_responses() {
    for body in [
        json!({"version": 2, "device_id": "device"}),
        json!({"version": 1, "device_id": ""}),
        json!({"version": 1, "device_id": "x".repeat(129)}),
        json!({"version": 1, "device_id": "bad\0device"}),
    ] {
        let raw = spawn_json_server(body).await;
        let client = HttpClient::new(&raw.base_url, token()).unwrap();
        assert!(matches!(
            client.device_identity().await,
            Err(HttpClientError::InvalidResponse { .. })
        ));
        raw.task.await.unwrap();
    }
}

#[tokio::test]
async fn replica_sync_rejects_another_device_before_changing_local_state() {
    let daemon = RunningDaemon::start().await;
    let (first_id, first_token) = create_device(&daemon.socket).await;
    let (second_id, second_token) = create_device(&daemon.socket).await;
    let first_client =
        HttpClient::new(&daemon.base_url, SecretToken::new(first_token).unwrap()).unwrap();
    let second_client =
        HttpClient::new(&daemon.base_url, SecretToken::new(second_token).unwrap()).unwrap();

    let mut wrong = LocalReplica::open(
        daemon._directory.path().join("wrong-device.sqlite"),
        &second_id,
    )
    .unwrap();
    let before = wrong.status().unwrap();
    let error = ReplicaSync::new(&first_client)
        .synchronize(&mut wrong)
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("authenticated device {first_id} does not match replica device {second_id}")
    );
    assert_eq!(wrong.status().unwrap(), before);

    let replica_path = daemon._directory.path().join("reopened-device.sqlite");
    let mut replica = LocalReplica::open(&replica_path, &first_id).unwrap();
    ReplicaSync::new(&first_client)
        .synchronize(&mut replica)
        .await
        .unwrap();
    replica
        .record_document(OfflineDocumentEdit {
            operation_id: "preserved-device-operation".into(),
            document_id: "preserved-device-document".into(),
            logical_path: "preserved-device.org".into(),
            org: "#+title: Preserved\n".into(),
            authored_at_us: 10,
        })
        .unwrap();
    drop(replica);

    let mut reopened = LocalReplica::open(&replica_path, &first_id).unwrap();
    ReplicaSync::new(&second_client)
        .synchronize(&mut reopened)
        .await
        .unwrap_err();
    assert_eq!(
        reopened
            .next_device_upload()
            .unwrap()
            .unwrap()
            .operation_id(),
        "preserved-device-operation"
    );
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
async fn review_operation_client_rejects_pending_attempt_with_grading_fields() {
    let mut response = review_operation_json(1, "operation", "attempt", 2);
    response["attempt"]["status"] = json!("pending");
    let raw = spawn_json_server(response).await;
    let client = HttpClient::new(&raw.base_url, token()).unwrap();

    assert!(matches!(
        client
            .set_review_accepted_success(
                "attempt",
                &SetAcceptedSuccessRequestV1 {
                    version: 1,
                    operation_id: "operation".into(),
                    expected_attempt_revision: 1,
                    accepted_success: true,
                },
            )
            .await,
        Err(HttpClientError::InvalidResponse {
            delivery: DeliveryStatus::Unknown
        })
    ));
    raw.task.await.unwrap();
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
        content_type: "text/org; charset=utf-8".into(),
        source: "#+title: Direct\n".into(),
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
        content_type: "text/org; charset=utf-8".into(),
        source: "#+title: Changed\n".into(),
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
        content_type: "text/org; charset=utf-8".into(),
        source: "#+title: Original\n".into(),
    };
    let created = client
        .create_note("create-replace", &original)
        .await
        .unwrap();
    let DocumentMutationResultV1::Applied { authoritative, .. } = created else {
        panic!("expected applied document mutation");
    };
    let replacement = NoteWriteRequestV1 {
        content_type: "text/org; charset=utf-8".into(),
        source: "#+title: Replacement\n".into(),
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
    let unchanged = client
        .replace_note(
            &authoritative.document_id,
            "unchanged-document",
            authoritative.revision,
            &replacement,
        )
        .await
        .unwrap();
    assert!(matches!(
        unchanged,
        DocumentMutationResultV1::Applied {
            changed: false,
            authoritative: ref head,
            ..
        } if head.revision == 2
    ));

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
            &authoritative.content_type,
            &rename,
        )
        .await
        .unwrap();
    let rename_retry = client
        .rename_note(
            &authoritative.document_id,
            "lifecycle-rename",
            authoritative.revision,
            &authoritative.content_type,
            &rename,
        )
        .await
        .unwrap();
    assert_eq!(rename_retry, renamed);
    let DocumentMutationResultV1::Applied { authoritative, .. } = renamed else {
        panic!("expected applied rename");
    };
    assert_eq!(authoritative.logical_path, "renamed.org");
    let unchanged_rename = client
        .rename_note(
            &authoritative.document_id,
            "unchanged-rename",
            authoritative.revision,
            &authoritative.content_type,
            &rename,
        )
        .await
        .unwrap();
    assert!(matches!(
        unchanged_rename,
        DocumentMutationResultV1::Applied {
            changed: false,
            authoritative: ref head,
            ..
        } if head.revision == authoritative.revision
    ));

    let deleted = client
        .tombstone_note(
            &authoritative.document_id,
            "lifecycle-delete",
            authoritative.revision,
            &authoritative.content_type,
        )
        .await
        .unwrap();
    let delete_retry = client
        .tombstone_note(
            &authoritative.document_id,
            "lifecycle-delete",
            authoritative.revision,
            &authoritative.content_type,
        )
        .await
        .unwrap();
    assert_eq!(delete_retry, deleted);
    let DocumentMutationResultV1::Applied { authoritative, .. } = deleted else {
        panic!("expected applied tombstone");
    };
    assert!(!authoritative.active);
    let unchanged_tombstone = client
        .tombstone_note(
            &authoritative.document_id,
            "unchanged-tombstone",
            authoritative.revision,
            &authoritative.content_type,
        )
        .await
        .unwrap();
    assert!(matches!(
        unchanged_tombstone,
        DocumentMutationResultV1::Applied {
            changed: false,
            authoritative: ref head,
            ..
        } if head.revision == authoritative.revision
    ));
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
        content_type: "text/org; charset=utf-8".into(),
        source: "#+title: Once\n".into(),
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
        content_type: "text/org; charset=utf-8".into(),
        source: "#+title: Note\n".into(),
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
        content_type: "text/org; charset=utf-8".into(),
        source: "#+title: Note\n".into(),
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
    assert_eq!(
        error.to_string(),
        "failed to upload Content operation uncertain-operation at device sequence 1 (unknown delivery)"
    );
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
                    content_type: "text/org; charset=utf-8".into(),
                    source: format!("#+title: Snapshot {index}\n"),
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

#[tokio::test]
async fn ordinary_projection_commands_require_authenticated_http_and_never_fall_back() {
    let daemon = RunningDaemon::start().await;
    let (_, token) = create_device(&daemon.socket).await;
    let (_, read_only_token) = create_device_with_scopes(&daemon.socket, vec!["read".into()]).await;
    let (_, review_only_token) =
        create_device_with_scopes(&daemon.socket, vec!["review".into()]).await;
    let client =
        HttpClient::new(&daemon.base_url, SecretToken::new(token.clone()).unwrap()).unwrap();
    let invalid_document = raw_http_client()
        .get(format!("{}/v1/documents/%20", daemon.base_url))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(invalid_document.status().as_u16(), 422);
    let invalid_document: serde_json::Value =
        serde_json::from_slice(&invalid_document.bytes().await.unwrap()).unwrap();
    assert_eq!(invalid_document["error"]["code"], "invalid_content");
    let input = DocumentImportRequestV1 {
        content_type: "text/org; charset=utf-8".into(),
        source: include_str!("fixtures/org/no-file-id.org").to_owned(),
        expected_revision: None,
    };

    let imported = client
        .import_document("http-only-doc", "import-http-only-doc", &input)
        .await
        .unwrap();
    assert!(matches!(
        &imported,
        DocumentMutationResultV1::Applied { authoritative, .. }
            if authoritative.document_id == "http-only-doc"
    ));
    let retry = client
        .import_document("http-only-doc", "import-http-only-doc", &input)
        .await
        .unwrap();
    assert_eq!(retry, imported);
    let unchanged_import = client
        .import_document(
            "http-only-doc",
            "unchanged-http-only-doc",
            &DocumentImportRequestV1 {
                content_type: input.content_type.clone(),
                source: input.source.clone(),
                expected_revision: Some(1),
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        unchanged_import,
        DocumentMutationResultV1::Applied {
            changed: false,
            authoritative: ref head,
            ..
        } if head.revision == 1
    ));
    client
        .rename_note(
            "http-only-doc",
            "rename-http-only-doc",
            1,
            "text/org; charset=utf-8",
            &NoteRenameRequestV1 {
                logical_path: "renamed-http-only.org".into(),
            },
        )
        .await
        .unwrap();
    let retry_after_rename = client
        .import_document("http-only-doc", "import-http-only-doc", &input)
        .await
        .unwrap();
    assert_eq!(retry_after_rename, imported);
    let changed_input = DocumentImportRequestV1 {
        content_type: "text/org; charset=utf-8".into(),
        source: "#+TITLE: Changed\n".into(),
        expected_revision: None,
    };
    assert!(matches!(
        client
            .import_document(
                "http-only-doc",
                "import-http-only-doc",
                &changed_input,
            )
            .await,
        Err(HttpClientError::Server(error)) if error.status() == 409
    ));
    assert_eq!(client.document("http-only-doc").await.unwrap().revision, 2);

    let thema_id = create_review_thema(&daemon.base_url, &token).await;
    assert_eq!(client.thema(&thema_id).await.unwrap().thema_id, thema_id);
    let catalog = client.thema_catalog("", 500, None, None).await.unwrap();
    assert!(catalog.items.iter().any(|item| item.thema_id == thema_id
        && item.thema_revision == 1
        && item.state_revision == 0
        && !item.suspended));
    assert!(matches!(
        client
            .thema_catalog("", 500, None, Some("different-generation"))
            .await,
        Err(HttpClientError::InvalidResponse { .. })
    ));
    let suspended = client
        .set_thema_suspended(&thema_id, "typed-suspend", 0, true)
        .await
        .unwrap();
    assert!(matches!(
        suspended,
        DesiredStateMutationResultV1::Applied {
            changed: true,
            authoritative:
                DesiredStateMutationProjectionV1::ReviewState {
                    thema_id: ref projection_id,
                    ref state,
                },
            ..
        } if projection_id == &thema_id
            && state.state_revision == 1
            && state.suspended
    ));
    let retry = client
        .set_thema_suspended(&thema_id, "typed-suspend", 0, true)
        .await
        .unwrap();
    assert_eq!(retry, suspended);
    assert!(matches!(
        client
            .set_thema_suspended(&thema_id, "typed-stale", 0, false)
            .await
            .unwrap(),
        DesiredStateMutationResultV1::Rejected { .. }
    ));
    client
        .replace_thema(&thema_id, "typed-content-edit", 1, &review_thema_request())
        .await
        .unwrap();
    let review_only = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(review_only_token.clone()).unwrap(),
    )
    .unwrap();
    let unsuspended = review_only
        .set_thema_suspended(&thema_id, "typed-unsuspend", 1, false)
        .await
        .unwrap();
    assert!(matches!(
        unsuspended,
        DesiredStateMutationResultV1::Applied {
            authoritative:
                DesiredStateMutationProjectionV1::ReviewState { ref state, .. },
            ..
        } if state.state_revision == 2 && !state.suspended
    ));
    let read_only = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(read_only_token.clone()).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        read_only
            .set_thema_suspended(&thema_id, "typed-forbidden", 2, true)
            .await,
        Err(HttpClientError::Server(error)) if error.status() == 403
    ));
    create_review_thema(&daemon.base_url, &token).await;
    create_review_thema(&daemon.base_url, &token).await;
    let due = client.due_page(20991231, 100, None).await.unwrap();
    assert!(due.items.len() > 1);
    assert!(due.items.iter().any(|item| item.id == thema_id));
    assert!(due.next.is_none());

    let exact = client
        .due_page(20991231, due.items.len().try_into().unwrap(), None)
        .await
        .unwrap();
    assert_eq!(exact.items, due.items);
    assert!(exact.next.is_none());

    let first_page = client.due_page(20991231, 1, None).await.unwrap();
    let cursor = first_page.next.as_deref().unwrap();
    assert!(!cursor.contains(&first_page.items[0].id));
    assert!(!cursor.contains(&first_page.items[0].next_review_day.to_string()));
    let mut tampered = cursor.as_bytes().to_vec();
    let middle = tampered.len() / 2;
    tampered[middle] = if tampered[middle] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(tampered).unwrap();
    let tampered_result = client.due_page(20991231, 1, Some(&tampered)).await;
    assert!(
        matches!(
            &tampered_result,
            Err(HttpClientError::Server(error)) if error.status() == 400
        ),
        "{tampered_result:?}"
    );

    let mut paged = Vec::new();
    let mut after = None;
    loop {
        let page = client
            .due_page(20991231, 1, after.as_deref())
            .await
            .unwrap();
        assert_eq!(page.items.len(), 1);
        paged.extend(page.items);
        let Some(next) = page.next else {
            break;
        };
        after = Some(next);
    }
    assert_eq!(paged, due.items);

    let unauthenticated = raw_http_client()
        .get(format!("{}/v1/documents/http-only-doc", daemon.base_url))
        .send()
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), reqwest::StatusCode::UNAUTHORIZED);

    let read_only = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(read_only_token.clone()).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        read_only
            .import_document("forbidden-doc", "forbidden-import", &input)
            .await,
        Err(HttpClientError::Server(error)) if error.status() == 403
    ));
    assert!(matches!(
        read_only.due_page(20991231, 10, None).await,
        Err(HttpClientError::Server(error)) if error.status() == 403
    ));

    let review_only = HttpClient::new(
        &daemon.base_url,
        SecretToken::new(review_only_token.clone()).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        review_only.document("http-only-doc").await,
        Err(HttpClientError::Server(error)) if error.status() == 403
    ));
    assert!(matches!(
        review_only.thema(&thema_id).await,
        Err(HttpClientError::Server(error)) if error.status() == 403
    ));
    assert!(matches!(
        review_only.due_page(20991231, 10, None).await,
        Err(HttpClientError::Server(error)) if error.status() == 403
    ));

    let token_path = daemon._directory.path().join("ordinary.token");
    write_token(&token_path, &token);
    let read_only_token_path = daemon._directory.path().join("read-only.token");
    write_token(&read_only_token_path, &read_only_token);
    let review_only_token_path = daemon._directory.path().join("review-only.token");
    write_token(&review_only_token_path, &review_only_token);
    let socket = daemon.socket.to_str().unwrap();
    let cli_import = run_cli_with_input(
        &daemon.base_url,
        &token_path,
        &[
            "import",
            "http-only-doc",
            "--operation-id",
            "cli-import",
            "--content-type",
            "text/org; charset=utf-8",
            "--if-revision",
            "1",
        ],
        include_bytes!("fixtures/org/no-file-id.org"),
    )
    .await;
    assert!(
        cli_import.status.success(),
        "{}",
        String::from_utf8_lossy(&cli_import.stderr)
    );
    for args in [
        vec!["get-document", "http-only-doc"],
        vec!["get-thema", thema_id.as_str()],
        vec!["due-themata", "20991231", "--limit", "10"],
        vec!["due-page", "20991231", "--limit", "10"],
    ] {
        let output = run_cli(&daemon.base_url, &token_path, &args).await;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    for args in [
        vec!["get-document", "http-only-doc"],
        vec!["get-thema", thema_id.as_str()],
        vec!["due-themata", "20991231", "--limit", "10"],
        vec!["due-page", "20991231", "--limit", "10"],
    ] {
        assert!(
            !run_cli_without_token(&daemon.base_url, &args)
                .await
                .status
                .success()
        );
        assert!(
            !run_cli(&daemon.base_url, &review_only_token_path, &args)
                .await
                .status
                .success()
        );
    }
    assert!(
        !run_cli_without_token_with_input(
            &daemon.base_url,
            &[
                "import",
                "unauthenticated-doc",
                "--operation-id",
                "unauthenticated-import",
                "--content-type",
                "text/org; charset=utf-8",
            ],
            include_bytes!("fixtures/org/no-file-id.org"),
        )
        .await
        .status
        .success()
    );
    assert!(
        !run_cli_with_input(
            &daemon.base_url,
            &read_only_token_path,
            &[
                "import",
                "forbidden-doc",
                "--operation-id",
                "forbidden-cli-import",
                "--content-type",
                "text/org; charset=utf-8",
            ],
            include_bytes!("fixtures/org/no-file-id.org"),
        )
        .await
        .status
        .success()
    );
    let ordinary_with_socket = run_cli(
        &daemon.base_url,
        &token_path,
        &["--socket", socket, "get-document", "http-only-doc"],
    )
    .await;
    assert!(!ordinary_with_socket.status.success());
    assert!(
        String::from_utf8_lossy(&ordinary_with_socket.stderr)
            .contains("--socket is only valid for local operator commands")
    );

    let operator_without_socket = run_cli(&daemon.base_url, &token_path, &["system-info"]).await;
    assert!(!operator_without_socket.status.success());
    let operator = run_cli(
        &daemon.base_url,
        &token_path,
        &["--socket", socket, "system-info"],
    )
    .await;
    assert!(operator.status.success());

    daemon.stop().await;
}

#[tokio::test]
async fn oversized_http_projection_is_rejected_before_commit() {
    let daemon = RunningDaemon::start().await;
    let (_, raw_token) = create_device(&daemon.socket).await;
    let client = HttpClient::new(&daemon.base_url, SecretToken::new(raw_token).unwrap()).unwrap();

    let error = client
        .import_document(
            "oversized",
            "oversized-import",
            &DocumentImportRequestV1 {
                content_type: "text/org; charset=utf-8".into(),
                source: deeply_nested_org(),
                expected_revision: None,
            },
        )
        .await
        .unwrap_err();
    let HttpClientError::Server(error) = error else {
        panic!("unexpected error: {error:?}");
    };
    assert_eq!(error.body().error.code(), ApiErrorCodeV1::InvalidContent);

    let missing = client.document("oversized").await.unwrap_err();
    let HttpClientError::Server(missing) = missing else {
        panic!("unexpected error: {missing:?}");
    };
    assert_eq!(missing.body().error.code(), ApiErrorCodeV1::NotFound);
    daemon.stop().await;
}

async fn create_device(socket: &Path) -> (String, String) {
    create_device_with_scopes(
        socket,
        vec!["read".into(), "review".into(), "content:write".into()],
    )
    .await
}

async fn create_device_with_scopes(socket: &Path, scopes: Vec<String>) -> (String, String) {
    let response = request(
        socket,
        &RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "create-device".into(),
            request: Request::DeviceCreate {
                name: "client".into(),
                scopes,
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
        .unwrap()
        .attempt
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

fn write_token(path: &Path, token: &str) {
    fs::write(path, format!("{token}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
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

async fn run_cli(base_url: &str, token_path: &Path, args: &[&str]) -> Output {
    run_cli_output(base_url, Some(token_path), args, None, false).await
}

async fn run_cli_to_full(base_url: &str, token_path: &Path, args: &[&str]) -> Output {
    run_cli_output(base_url, Some(token_path), args, None, true).await
}

async fn run_cli_without_token(base_url: &str, args: &[&str]) -> Output {
    run_cli_output(base_url, None, args, None, false).await
}

async fn run_cli_with_input(
    base_url: &str,
    token_path: &Path,
    args: &[&str],
    input: &[u8],
) -> Output {
    run_cli_output(base_url, Some(token_path), args, Some(input), false).await
}

async fn run_cli_with_input_to_full(
    base_url: &str,
    token_path: &Path,
    args: &[&str],
    input: &[u8],
) -> Output {
    run_cli_output(base_url, Some(token_path), args, Some(input), true).await
}

async fn run_cli_without_token_with_input(base_url: &str, args: &[&str], input: &[u8]) -> Output {
    run_cli_output(base_url, None, args, Some(input), false).await
}

async fn run_cli_without_token_with_input_to_full(
    base_url: &str,
    args: &[&str],
    input: &[u8],
) -> Output {
    run_cli_output(base_url, None, args, Some(input), true).await
}

async fn run_cli_output(
    base_url: &str,
    token_path: Option<&Path>,
    args: &[&str],
    input: Option<&[u8]>,
    fail_output: bool,
) -> Output {
    let base_url = base_url.to_owned();
    let token_path = token_path.map(Path::to_owned);
    let args: Vec<String> = args.iter().map(|value| (*value).to_owned()).collect();
    let input = input.map(<[u8]>::to_vec);
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
        command.args(args);
        if let Some(input) = input {
            command.stdin(Stdio::piped()).stderr(Stdio::piped());
            if !fail_output {
                command.stdout(Stdio::piped());
            }
            let mut child = command.spawn().unwrap();
            child.stdin.take().unwrap().write_all(&input).unwrap();
            child.wait_with_output().unwrap()
        } else {
            command.output().unwrap()
        }
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
        concepts: Vec::new(),
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
        .header("idempotency-key", next_operation_id("test-asset"))
        .header("content-type", media_type)
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    path
}

async fn assert_no_active_session(client: &HttpClient) {
    assert!(
        client
            .active_review_session()
            .await
            .unwrap()
            .session
            .is_none()
    );
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
        .header("idempotency-key", next_operation_id("test-thema"))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(input).unwrap())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    serde_json::from_slice::<serde_json::Value>(&response.bytes().await.unwrap())
        .unwrap()["authoritative"]["value"]["thema_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn applied_asset(result: &DesiredStateMutationResultV1) -> &AssetHeadV1 {
    match result {
        DesiredStateMutationResultV1::Applied {
            authoritative: DesiredStateMutationProjectionV1::Asset(asset),
            ..
        } => asset,
        _ => panic!("expected applied asset mutation"),
    }
}

fn next_operation_id(prefix: &str) -> String {
    static NEXT_OPERATION_ID: AtomicU64 = AtomicU64::new(1);
    format!(
        "{prefix}-{}",
        NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed)
    )
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

#[test]
fn forwarded_proxy_responses_close_the_client_connection() {
    let response =
        b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: keep-alive\r\n\r\n\xff\x00\x01";

    let closed = close_forwarded_response(response.to_vec());

    assert!(closed.starts_with(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n"));
    assert!(
        closed
            .windows(19)
            .any(|value| value == b"Connection: close\r\n")
    );
    assert!(!closed.windows(10).any(|value| value == b"keep-alive"));
    assert!(closed.ends_with(b"\xff\x00\x01"));
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
    let response = close_forwarded_response(upstream_http_response(upstream, request).await);
    client.write_all(&response).await.unwrap();
}

fn close_forwarded_response(response: Vec<u8>) -> Vec<u8> {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let headers = std::str::from_utf8(&response[..header_end]).unwrap();
    let mut closed = Vec::with_capacity(response.len() + 19);
    for line in headers.lines().filter(|line| !is_connection_header(line)) {
        closed.extend_from_slice(line.as_bytes());
        closed.extend_from_slice(b"\r\n");
    }
    closed.extend_from_slice(b"Connection: close\r\n\r\n");
    closed.extend_from_slice(&response[header_end + 4..]);
    closed
}

fn is_connection_header(line: &str) -> bool {
    line.split_once(':')
        .is_some_and(|(name, _)| name.eq_ignore_ascii_case("connection"))
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
            "presentation":{"kind":"basic","question":"Question","cloze_spans":[],
                "hint":null,
                "response":{"kind":"text","input_script":null},
                "source_guid":null,"review_image":null,"tags":[]},
            "revealed_answer":["Answer"],
            "revealed_parathema":null,
            "matcher_version":1,
            "response":{"kind":"text","value":"Answer"},
            "detected_success":true,
            "accepted_success":true,
            "override_count":0,
            "proposed_transition":review_transition_json(),
            "desired_suspended":false,
            "status":"graded"
        }
    })
}

fn review_transition_json() -> serde_json::Value {
    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/protocol/offline-review-v1.json")).unwrap();
    fixture["submission"]["operation"]["proposed_transition"].clone()
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
        content_type: "text/org; charset=utf-8".into(),
        source: "#+title: Note\n".into(),
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
