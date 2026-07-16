//! Authenticated HTTP transport for durable Gnosis replicas.
//!
//! HTTPS is required except for literal loopback addresses and `localhost`.
//! The client ignores proxy environment variables, follows no redirects, and
//! performs no automatic retries. Requests have connect and total timeouts,
//! and every response body is bounded before decoding. A timeout, transport
//! failure, or invalid response after dispatch has unknown delivery: callers
//! must retain the durable outbox item and retry its same operation identity.

use std::{fmt, net::IpAddr, time::Duration};

use reqwest::{
    RequestBuilder, Response, StatusCode, Url,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::de::DeserializeOwned;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    application::MAX_OFFLINE_ASSET_BYTES,
    client_replica::{
        DeviceUploadKind, DeviceUploadResult, PendingAssetUpload, PendingDeviceUpload,
    },
    is_canonical_sha256,
    protocol::{
        MAX_OFFLINE_ASSET_OPERATION_BYTES, MAX_OFFLINE_ASSET_RESULT_BYTES,
        MAX_OFFLINE_CONTENT_OPERATION_BYTES, MAX_OFFLINE_CONTENT_RESULT_BYTES,
        MAX_OFFLINE_REVIEW_OPERATION_BYTES, MAX_OFFLINE_REVIEW_RESULT_BYTES,
        MAX_REPLICA_CURSOR_BYTES, MAX_REPLICA_PAGE_BYTES,
        http_v1::{
            ApiErrorResponseV1, AssetCapabilitiesV1, ContentCapabilitiesV1, ReplicaChangePageV1,
            ReplicaSnapshotPageV1, ReplicaStateV1, StagedBlobV1, valid_bearer_token,
        },
    },
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_ERROR_BODY_BYTES: usize = 64 * 1024;

pub struct SecretToken(String);

impl SecretToken {
    pub fn new(value: String) -> Result<Self, HttpClientError> {
        if !valid_bearer_token(&value) {
            return Err(HttpClientError::InvalidToken);
        }
        Ok(Self(value))
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretToken([redacted])")
    }
}

pub struct HttpClient {
    client: reqwest::Client,
    base_url: Url,
}

impl HttpClient {
    pub fn new(base_url: &str, token: SecretToken) -> Result<Self, HttpClientError> {
        let base_url = Url::parse(base_url).map_err(|_| HttpClientError::InvalidBaseUrl)?;
        validate_base_url(&base_url)?;
        let headers = authorization_headers(token)?;
        install_tls_provider()?;
        let builder = reqwest::Client::builder()
            .default_headers(headers)
            .redirect(Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .no_proxy()
            .tls_backend_rustls()
            .user_agent(concat!("gnosis-core/", env!("CARGO_PKG_VERSION")));
        let builder = if base_url.scheme() == "http" {
            builder.tls_certs_only(std::iter::empty::<reqwest::Certificate>())
        } else {
            builder
        };
        let client = builder.build().map_err(HttpClientError::ClientBuild)?;
        Ok(Self { client, base_url })
    }

    pub async fn replica_state(&self) -> Result<ReplicaStateV1, HttpClientError> {
        self.get_json("v1/sync/state").await
    }

    pub async fn content_capabilities(&self) -> Result<ContentCapabilitiesV1, HttpClientError> {
        self.get_json("v1/sync/content-capabilities").await
    }

    pub async fn asset_capabilities(&self) -> Result<AssetCapabilitiesV1, HttpClientError> {
        self.get_json("v1/sync/asset-capabilities").await
    }

    pub async fn snapshot(
        &self,
        cursor: Option<&str>,
    ) -> Result<ReplicaSnapshotPageV1, HttpClientError> {
        let mut url = self.endpoint("v1/sync/snapshot")?;
        if let Some(cursor) = cursor {
            append_cursor(&mut url, cursor)?;
        }
        execute_json(self.client.get(url), MAX_REPLICA_PAGE_BYTES).await
    }

    pub async fn changes(&self, cursor: &str) -> Result<ReplicaChangePageV1, HttpClientError> {
        let mut url = self.endpoint("v1/sync/changes")?;
        append_cursor(&mut url, cursor)?;
        execute_json(self.client.get(url), MAX_REPLICA_PAGE_BYTES).await
    }

    pub async fn blob(&self, digest: &str) -> Result<Vec<u8>, HttpClientError> {
        require_digest(digest)?;
        let url = self.endpoint(&format!("v1/blobs/{digest}"))?;
        let limit = MAX_OFFLINE_ASSET_BYTES as usize;
        let bytes = execute_bytes(self.client.get(url), limit).await?;
        if format!("{:x}", Sha256::digest(&bytes)) != digest {
            return Err(invalid_response());
        }
        Ok(bytes)
    }

    pub async fn stage_blob(&self, upload: &PendingAssetUpload) -> Result<(), HttpClientError> {
        let url = self.endpoint(&format!("v1/sync/blobs/{}", upload.content_sha256()))?;
        let media_type = HeaderValue::from_str(upload.media_type())
            .map_err(|_| HttpClientError::InvalidUpload)?;
        let request = self
            .client
            .put(url)
            .header(CONTENT_TYPE, media_type)
            .body(upload.bytes().to_vec());
        let response: StagedBlobV1 = execute_json(request, MAX_ERROR_BODY_BYTES).await?;
        if response.content_sha256 != upload.content_sha256()
            || response.size_bytes != upload.size_bytes()
        {
            return Err(invalid_response());
        }
        Ok(())
    }

    pub async fn upload(
        &self,
        upload: &PendingDeviceUpload,
    ) -> Result<DeviceUploadResult, HttpClientError> {
        let (kind, path, body) = upload_request(upload)?;
        let url = self.endpoint(path)?;
        let request = self
            .client
            .post(url)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body);
        let value = execute_json(request, upload_response_limit(kind)).await?;
        decode_upload_result(upload, value)
    }

    async fn get_json<T: DeserializeOwned>(&self, path: &str) -> Result<T, HttpClientError> {
        let url = self.endpoint(path)?;
        execute_json(self.client.get(url), MAX_REPLICA_PAGE_BYTES).await
    }

    fn endpoint(&self, path: &str) -> Result<Url, HttpClientError> {
        self.base_url
            .join(path)
            .map_err(|_| HttpClientError::InvalidBaseUrl)
    }
}

impl fmt::Debug for HttpClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HttpClient")
            .field("base_url", &self.base_url.as_str())
            .finish_non_exhaustive()
    }
}

fn authorization_headers(token: SecretToken) -> Result<HeaderMap, HttpClientError> {
    let mut value = HeaderValue::from_str(&format!("Bearer {}", token.0))
        .map_err(|_| HttpClientError::InvalidToken)?;
    value.set_sensitive(true);
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, value);
    Ok(headers)
}

fn install_tls_provider() -> Result<(), HttpClientError> {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    rustls::crypto::CryptoProvider::get_default()
        .is_some()
        .then_some(())
        .ok_or(HttpClientError::TlsProvider)
}

fn validate_base_url(url: &Url) -> Result<(), HttpClientError> {
    let plain_loopback = url.scheme() == "http" && is_loopback(url);
    if (url.scheme() != "https" && !plain_loopback)
        || url.username() != ""
        || url.password().is_some()
        || url.host_str().is_none()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(HttpClientError::InvalidBaseUrl);
    }
    Ok(())
}

fn is_loopback(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    if host == "localhost" {
        return true;
    }
    host.trim_matches(|character| matches!(character, '[' | ']'))
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

fn append_cursor(url: &mut Url, cursor: &str) -> Result<(), HttpClientError> {
    if cursor.is_empty() || cursor.len() > MAX_REPLICA_CURSOR_BYTES {
        return Err(HttpClientError::InvalidCursor);
    }
    url.query_pairs_mut().append_pair("cursor", cursor);
    Ok(())
}

fn require_digest(digest: &str) -> Result<(), HttpClientError> {
    is_canonical_sha256(digest)
        .then_some(())
        .ok_or(HttpClientError::InvalidDigest)
}

fn upload_request(
    upload: &PendingDeviceUpload,
) -> Result<(DeviceUploadKind, &'static str, String), HttpClientError> {
    let kind = upload.kind();
    let body = upload.canonical_payload();
    if body.len() > upload_request_limit(kind)? {
        return Err(HttpClientError::InvalidUpload);
    }
    Ok((kind, upload_path(kind), body.to_owned()))
}

const fn upload_path(kind: DeviceUploadKind) -> &'static str {
    match kind {
        DeviceUploadKind::Review => "v1/review/offline-operations",
        DeviceUploadKind::Content => "v1/content/offline-operations",
        DeviceUploadKind::Asset => "v1/content/offline-asset-operations",
    }
}

fn upload_request_limit(kind: DeviceUploadKind) -> Result<usize, HttpClientError> {
    match kind {
        DeviceUploadKind::Review => Ok(MAX_OFFLINE_REVIEW_OPERATION_BYTES),
        DeviceUploadKind::Content => Ok(MAX_OFFLINE_CONTENT_OPERATION_BYTES),
        DeviceUploadKind::Asset => Ok(MAX_OFFLINE_ASSET_OPERATION_BYTES),
    }
}

const fn upload_response_limit(kind: DeviceUploadKind) -> usize {
    match kind {
        DeviceUploadKind::Review => MAX_OFFLINE_REVIEW_RESULT_BYTES,
        DeviceUploadKind::Content => MAX_OFFLINE_CONTENT_RESULT_BYTES,
        DeviceUploadKind::Asset => MAX_OFFLINE_ASSET_RESULT_BYTES,
    }
}

fn decode_upload_result(
    upload: &PendingDeviceUpload,
    value: serde_json::Value,
) -> Result<DeviceUploadResult, HttpClientError> {
    let result =
        DeviceUploadResult::decode(upload.kind(), value).map_err(|_| invalid_response())?;
    (result.operation_id() == upload.operation_id())
        .then_some(result)
        .ok_or_else(invalid_response)
}

async fn execute_json<T: DeserializeOwned>(
    request: RequestBuilder,
    max_bytes: usize,
) -> Result<T, HttpClientError> {
    let response = request.send().await.map_err(transport_error)?;
    let status = response.status();
    if !is_json(&response) {
        return Err(invalid_response());
    }
    let limit = if status.is_success() {
        max_bytes
    } else {
        MAX_ERROR_BODY_BYTES
    };
    let bytes = read_bounded(response, limit).await?;
    if status.is_success() {
        serde_json::from_slice(&bytes).map_err(|_| invalid_response())
    } else {
        server_error(status, &bytes)
    }
}

async fn execute_bytes(
    request: RequestBuilder,
    max_bytes: usize,
) -> Result<Vec<u8>, HttpClientError> {
    let response = request.send().await.map_err(transport_error)?;
    if response.status().is_success() {
        return read_bounded(response, max_bytes).await;
    }
    if !is_json(&response) {
        return Err(invalid_response());
    }
    let status = response.status();
    let bytes = read_bounded(response, MAX_ERROR_BODY_BYTES).await?;
    server_error(status, &bytes)
}

fn is_json(response: &Response) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
}

async fn read_bounded(mut response: Response, limit: usize) -> Result<Vec<u8>, HttpClientError> {
    if response
        .content_length()
        .is_some_and(|length| usize::try_from(length).map_or(true, |length| length > limit))
    {
        return Err(response_too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport_error)? {
        let length = body
            .len()
            .checked_add(chunk.len())
            .ok_or_else(response_too_large)?;
        if length > limit {
            return Err(response_too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn server_error<T>(status: StatusCode, bytes: &[u8]) -> Result<T, HttpClientError> {
    let body = serde_json::from_slice(bytes).map_err(|_| invalid_response())?;
    Err(HttpClientError::Server(HttpServerError {
        status: status.as_u16(),
        body,
    }))
}

fn transport_error(error: reqwest::Error) -> HttpClientError {
    let delivery = if error.is_connect() {
        DeliveryStatus::NotSent
    } else {
        DeliveryStatus::Unknown
    };
    if error.is_timeout() {
        HttpClientError::Timeout { delivery }
    } else {
        HttpClientError::Transport { delivery }
    }
}

const fn invalid_response() -> HttpClientError {
    HttpClientError::InvalidResponse {
        delivery: DeliveryStatus::Unknown,
    }
}

const fn response_too_large() -> HttpClientError {
    HttpClientError::ResponseTooLarge {
        delivery: DeliveryStatus::Unknown,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryStatus {
    NotSent,
    Unknown,
}

#[derive(Debug, Error)]
#[error("Gnosis server returned HTTP {status}")]
pub struct HttpServerError {
    status: u16,
    body: ApiErrorResponseV1,
}

impl HttpServerError {
    pub const fn status(&self) -> u16 {
        self.status
    }

    pub const fn body(&self) -> &ApiErrorResponseV1 {
        &self.body
    }
}

#[derive(Debug, Error)]
pub enum HttpClientError {
    #[error("Gnosis HTTP base URL is invalid")]
    InvalidBaseUrl,
    #[error("Gnosis device token is invalid")]
    InvalidToken,
    #[error("Gnosis replica cursor is invalid")]
    InvalidCursor,
    #[error("Gnosis content digest is invalid")]
    InvalidDigest,
    #[error("Gnosis pending upload is invalid")]
    InvalidUpload,
    #[error("Gnosis HTTP client initialization failed")]
    ClientBuild(#[source] reqwest::Error),
    #[error("Gnosis TLS provider initialization failed")]
    TlsProvider,
    #[error("Gnosis HTTP request timed out")]
    Timeout { delivery: DeliveryStatus },
    #[error("Gnosis HTTP transport failed")]
    Transport { delivery: DeliveryStatus },
    #[error("Gnosis HTTP response exceeds its protocol limit")]
    ResponseTooLarge { delivery: DeliveryStatus },
    #[error("Gnosis HTTP response is invalid")]
    InvalidResponse { delivery: DeliveryStatus },
    #[error(transparent)]
    Server(HttpServerError),
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use serde_json::Value;
    use tempfile::TempDir;
    use tokio::{io::AsyncReadExt as _, net::TcpListener, sync::oneshot, time::timeout};

    use super::*;
    use crate::{
        application::{
            ReviewSource, ReviewSourceKind, generate_device, prepare_thema, thema_content,
        },
        client_replica::{LocalReplica, OfflineAnswer, OfflineResponse},
        protocol::{
            DaemonConfig, DaemonServer, offline_asset_capabilities, offline_content_capabilities,
            replica_snapshot_page, replica_state_response, review_pack_json,
        },
        service::ServiceRuntime,
    };

    #[test]
    fn base_url_policy_requires_https_outside_loopback() {
        for value in [
            "https://gnosis.example",
            "http://localhost:7842",
            "http://127.0.0.1:7842",
            "http://[::1]:7842",
        ] {
            let url = Url::parse(value).unwrap();
            assert!(validate_base_url(&url).is_ok(), "rejected {value}");
        }
        for value in ["http://gnosis.example", "ftp://127.0.0.1"] {
            let url = Url::parse(value).unwrap();
            assert!(validate_base_url(&url).is_err(), "accepted {value}");
        }
    }

    #[tokio::test]
    async fn timeout_after_dispatch_has_unknown_delivery() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (dispatched, observed) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            let read = stream.read(&mut request).await.unwrap();
            assert!(read > 0);
            dispatched.send(()).unwrap();
            std::future::pending::<()>().await;
        });
        let token = SecretToken::new(format!("gnosis_{}", "a".repeat(43))).unwrap();
        let client = HttpClient::new(&format!("http://{address}"), token).unwrap();
        let request = client
            .client
            .get(client.endpoint("v1/sync/state").unwrap())
            .timeout(Duration::from_millis(100));

        let response = tokio::spawn(execute_json::<ReplicaStateV1>(
            request,
            MAX_REPLICA_PAGE_BYTES,
        ));
        timeout(Duration::from_secs(1), observed)
            .await
            .unwrap()
            .unwrap();
        let error = response.await.unwrap().unwrap_err();
        assert!(matches!(
            error,
            HttpClientError::Timeout {
                delivery: DeliveryStatus::Unknown
            }
        ));
        server.abort();
    }

    #[tokio::test]
    async fn review_upload_is_decoded_and_recorded() {
        let fixture = prepare_review_fixture().await;
        let mut replica = install_review_fixture(&fixture);
        let upload = replica.next_device_upload().unwrap().unwrap();
        assert_eq!(upload.kind(), DeviceUploadKind::Review);
        let daemon = TestDaemon::start(&fixture).await;
        let token = SecretToken::new(fixture.token.clone()).unwrap();
        let client = HttpClient::new(&daemon.base_url, token).unwrap();

        let result = client.upload(&upload).await.unwrap();
        assert_eq!(result.kind(), DeviceUploadKind::Review);
        replica.record_device_result(result).unwrap();
        assert!(replica.next_device_upload().unwrap().is_none());

        daemon.stop().await;
    }

    struct ReviewFixture {
        _directory: TempDir,
        database: PathBuf,
        socket: PathBuf,
        replica: PathBuf,
        device_id: String,
        token: String,
        state: ReplicaStateV1,
        snapshot: ReplicaSnapshotPageV1,
        pack: Value,
    }

    async fn prepare_review_fixture() -> ReviewFixture {
        const ISSUED_AT_US: u64 = 1_784_106_000_000_000;
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let runtime = ServiceRuntime::start(database.clone(), database.with_extension("blobs"), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let device = generate_device("review-client".into(), vec!["review".into()]).unwrap();
        let device_id = device.device_id.clone();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        seed_thema(&service).await;
        let pack = service
            .create_review_pack(1, manual_source(), Some(1), device_id.clone(), ISSUED_AT_US)
            .await
            .unwrap();
        let state = service.replica_state(device_id.clone()).await.unwrap();
        let snapshot = service
            .create_replica_snapshot(device_id.clone(), ISSUED_AT_US, 100)
            .await
            .unwrap();
        drop(service);
        runtime.finish().await.unwrap();
        ReviewFixture {
            socket: directory.path().join("gnosis.sock"),
            replica: directory.path().join("replica.sqlite"),
            _directory: directory,
            database,
            device_id,
            token,
            state: replica_state_response(&state).unwrap(),
            snapshot: replica_snapshot_page(&snapshot).unwrap(),
            pack: review_pack_json(&pack).unwrap(),
        }
    }

    async fn seed_thema(service: &crate::service::ServiceHandle) {
        let content = thema_content(
            "basic",
            "Question".into(),
            vec![None],
            vec!["answer".into()],
            None,
            None,
            None,
        )
        .unwrap();
        let thema =
            prepare_thema(Some("thema".into()), content, vec![], vec![], vec![], None).unwrap();
        service.put_thema(thema, 20260715, 2, None).await.unwrap();
    }

    fn manual_source() -> ReviewSource {
        ReviewSource {
            kind: ReviewSourceKind::Manual,
            tags: vec![],
            exclude_tags: vec![],
            topic_node_id: None,
            topic_forward_depth: None,
            topic_backward_depth: None,
            manual_ids: vec!["thema".into()],
        }
    }

    fn install_review_fixture(fixture: &ReviewFixture) -> LocalReplica {
        let mut replica = LocalReplica::open(&fixture.replica, &fixture.device_id).unwrap();
        replica.observe_state(fixture.state.clone()).unwrap();
        let server_id = &fixture.state.server_id;
        replica
            .install_content_capabilities(offline_content_capabilities(server_id).unwrap())
            .unwrap();
        replica
            .install_asset_capabilities(offline_asset_capabilities(server_id).unwrap())
            .unwrap();
        replica
            .install_snapshot_page(None, fixture.snapshot.clone())
            .unwrap();
        let pack_id = replica.install_review_pack(fixture.pack.clone()).unwrap();
        replica.answer(&pack_id, review_answer()).unwrap();
        replica
    }

    fn review_answer() -> OfflineAnswer {
        OfflineAnswer {
            attempt_id: "review-attempt".into(),
            session_position: 0,
            response: OfflineResponse::Text("answer".into()),
            accepted_success: None,
            desired_suspended: false,
            answered_at_us: 1_784_106_000_001_000,
            fuzz_sample: -0.25,
        }
    }

    struct TestDaemon {
        base_url: String,
        shutdown: oneshot::Sender<()>,
        task: tokio::task::JoinHandle<Result<(), crate::protocol::ServerError>>,
    }

    impl TestDaemon {
        async fn start(fixture: &ReviewFixture) -> Self {
            let mut config = DaemonConfig::new(&fixture.socket, &fixture.database);
            config.http_addr = Some("127.0.0.1:0".parse().unwrap());
            let server = DaemonServer::bind(config).await.unwrap();
            let address = server.http_addr().unwrap();
            let (shutdown, receiver) = oneshot::channel();
            let task = tokio::spawn(server.run(async {
                let _ = receiver.await;
            }));
            Self {
                base_url: format!("http://{address}"),
                shutdown,
                task,
            }
        }

        async fn stop(self) {
            self.shutdown.send(()).unwrap();
            self.task.await.unwrap().unwrap();
        }
    }
}
