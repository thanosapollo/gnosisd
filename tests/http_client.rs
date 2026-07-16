use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};

use gnosis_core::{
    client_http::{DeliveryStatus, HttpClient, HttpClientError, SecretToken},
    client_replica::{
        DeviceUploadKind, LocalReplica, OfflineAssetCreate, OfflineDocumentEdit,
        PendingDeviceUpload,
    },
    protocol::{
        DaemonConfig, DaemonServer, PROTOCOL_VERSION, Request, RequestEnvelope, ResponseOutcome,
        ResponseResult, ServerError, http_v1::ApiErrorCodeV1, request,
    },
};
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

struct RawServer {
    base_url: String,
    task: tokio::task::JoinHandle<()>,
}

struct DropResponseProxy {
    base_url: String,
    task: tokio::task::JoinHandle<Vec<u8>>,
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
