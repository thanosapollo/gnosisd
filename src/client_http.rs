//! Authenticated HTTP transport for durable Gnosis replicas.
//!
//! HTTPS is required except for literal loopback addresses and `localhost`.
//! The client ignores proxy environment variables, follows no redirects, and
//! performs no automatic retries. Requests have connect and total timeouts,
//! and every response body is bounded before decoding. A timeout, transport
//! failure, or invalid response after dispatch has unknown delivery: callers
//! must retain the durable outbox item and retry its same operation identity.

use std::{collections::HashSet, fmt, net::IpAddr, time::Duration};

use chrono::NaiveDate;
use reqwest::{
    RequestBuilder, Response, StatusCode, Url,
    header::{AUTHORIZATION, CONTENT_TYPE, ETAG, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    application::{
        DEFAULT_ONLINE_REVIEW_TARGET_COUNT, DeviceDesiredStateMutation, MAX_DOCUMENT_SOURCE_BYTES,
        MAX_DUE_CURSOR_BYTES, MAX_OFFLINE_ASSET_BYTES, MAX_ONLINE_REVIEW_TARGET_COUNT,
        canonical_asset_path, desired_state_wire_request_sha256, prepare_asset, prepare_document,
        prepare_thema, thema_content, valid_operation_id, validate_asset_path,
        validate_blob_metadata,
    },
    client_replica::{
        DeviceUploadKind, DeviceUploadResult, PendingAssetUpload, PendingDeviceUpload,
    },
    domain::{
        DocumentId, DocumentProjection, DocumentProjectionInput, LinkOccurrence, Node, NodeId,
        SourceSpan, ThemaId, TitlePath,
    },
    is_canonical_sha256,
    parser::{supported_content_type, validate_document_path},
    protocol::{
        DocumentView, DuePageView, MAX_FRAME_SIZE, MAX_OFFLINE_ASSET_OPERATION_BYTES,
        MAX_OFFLINE_ASSET_RESULT_BYTES, MAX_OFFLINE_CONTENT_OPERATION_BYTES,
        MAX_OFFLINE_CONTENT_RESULT_BYTES, MAX_OFFLINE_REVIEW_OPERATION_BYTES,
        MAX_OFFLINE_REVIEW_RESULT_BYTES, MAX_REPLICA_CURSOR_BYTES, MAX_REPLICA_PAGE_BYTES,
        decode_review_pack,
        http_v1::{
            ActiveReviewPackEnvelopeV2, ActiveReviewSessionV1, ApiErrorCodeV1, ApiErrorResponseV1,
            AssetCapabilitiesV1, AssetHeadV1, ContentCapabilitiesV1, DashboardV1,
            DesiredStateMutationProjectionV1, DesiredStateMutationResultV1, DeviceIdentityV1,
            DocumentImportRequestV1, DocumentMutationRejectionV1, DocumentMutationResultV1,
            FsrsConfigV1, NodePageV1, NodeThemaAssociationPageV1, NodeV1, NoteRenameRequestV1,
            NoteWriteRequestV1, ReplicaChangePageV1, ReplicaSnapshotPageV1, ReplicaStateV1,
            ReviewAnswerV1, ReviewAttemptBeginResponseV1, ReviewAttemptOperationResponseV1,
            ReviewAttemptStateResponseV1, ReviewAttemptV1, ReviewClozeCheckRequestV1,
            ReviewClozeCheckV1, ReviewCommitResponseV1, ReviewHistoryPageV1,
            ReviewHistoryRequestV1, ReviewPackAssetV2, ReviewPackEnvelopeV2, ReviewPackStatusV2,
            ReviewQueueSummaryRequestV1, ReviewQueueSummaryV1, ReviewSessionListRequestV1,
            ReviewSessionModeV1, ReviewSessionPageV1, ReviewSessionRequestV1,
            ReviewSessionStatusV1, ReviewSessionSummaryV1, ReviewSessionV1, ReviewStateV1,
            SetAcceptedSuccessRequestV1, SetSuspensionRequestV1, SetThemaSuspensionRequestV1,
            SettingsV1, StagedBlobV1, ThemaCatalogItemV1, ThemaCatalogPageV1, ThemaKindV1,
            ThemaPageV1, ThemaReviewStateV1, ThemaV1, ThemaWriteRequestV1, UpdateSettingsRequestV1,
            valid_bearer_token,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentSource {
    document_id: String,
    revision: u64,
    content_type: String,
    content_sha256: String,
    source: String,
}

impl DocumentSource {
    pub fn document_id(&self) -> &str {
        &self.document_id
    }

    pub const fn revision(&self) -> u64 {
        self.revision
    }

    pub fn content_type(&self) -> &str {
        &self.content_type
    }

    pub fn content_sha256(&self) -> &str {
        &self.content_sha256
    }

    pub fn source(&self) -> &str {
        &self.source
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NodeSource {
    node: NodeV1,
    document: DocumentSource,
}

impl NodeSource {
    pub const fn node(&self) -> &NodeV1 {
        &self.node
    }

    pub const fn document(&self) -> &DocumentSource {
        &self.document
    }

    pub fn selected_source(&self) -> &str {
        self.document
            .source
            .get(self.node.start_byte.unwrap()..self.node.end_byte.unwrap())
            .expect("node source range was validated")
    }
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

    pub async fn device_identity(&self) -> Result<DeviceIdentityV1, HttpClientError> {
        let identity: DeviceIdentityV1 = self.get_json("v1/sync/identity").await?;
        let length = identity.device_id.chars().count();
        if identity.version == 1
            && (1..=128).contains(&length)
            && !identity.device_id.contains('\0')
        {
            Ok(identity)
        } else {
            Err(invalid_response())
        }
    }

    pub async fn content_capabilities(&self) -> Result<ContentCapabilitiesV1, HttpClientError> {
        self.get_json("v1/sync/content-capabilities").await
    }

    pub async fn asset_capabilities(&self) -> Result<AssetCapabilitiesV1, HttpClientError> {
        self.get_json("v1/sync/asset-capabilities").await
    }

    pub async fn create_review_pack(
        &self,
        input: &ReviewSessionRequestV1,
    ) -> Result<ReviewPackEnvelopeV2, HttpClientError> {
        let request = self.post_review_json("v1/review/packs", input)?;
        let response = execute_json_exact(
            request,
            MAX_OFFLINE_REVIEW_RESULT_BYTES,
            StatusCode::CREATED,
        )
        .await?;
        checked_review_pack(response, None, Some(ReviewPackStatusV2::Active))
    }

    pub async fn active_review_pack(&self) -> Result<ActiveReviewPackEnvelopeV2, HttpClientError> {
        let url = self.endpoint("v1/review/packs/active")?;
        let response = execute_json_exact(
            self.client.get(url),
            MAX_OFFLINE_REVIEW_RESULT_BYTES,
            StatusCode::OK,
        )
        .await?;
        checked_active_review_pack(response)
    }

    pub async fn review_pack(
        &self,
        pack_id: &str,
    ) -> Result<ReviewPackEnvelopeV2, HttpClientError> {
        let url = self.review_resource_endpoint("packs", pack_id, None)?;
        let response = execute_json_exact(
            self.client.get(url),
            MAX_OFFLINE_REVIEW_RESULT_BYTES,
            StatusCode::OK,
        )
        .await?;
        checked_review_pack(response, Some(pack_id), None)
    }

    pub async fn abandon_review_pack(
        &self,
        pack_id: &str,
    ) -> Result<ReviewPackEnvelopeV2, HttpClientError> {
        let url = self.review_resource_endpoint("packs", pack_id, Some("abandon"))?;
        let response = execute_json_exact(
            self.client.post(url),
            MAX_OFFLINE_REVIEW_RESULT_BYTES,
            StatusCode::OK,
        )
        .await?;
        checked_review_pack(response, Some(pack_id), Some(ReviewPackStatusV2::Abandoned))
    }

    pub async fn review_pack_asset(
        &self,
        pack_id: &str,
        asset: &ReviewPackAssetV2,
    ) -> Result<Vec<u8>, HttpClientError> {
        let url = self.review_pack_asset_endpoint(pack_id, asset)?;
        let limit = usize::try_from(asset.size_bytes).map_err(|_| invalid_response())?;
        let bytes = execute_pack_asset(self.client.get(url), limit, &asset.media_type).await?;
        checked_pack_asset(bytes, asset)
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
        if bytes.is_empty() || format!("{:x}", Sha256::digest(&bytes)) != digest {
            return Err(invalid_response());
        }
        Ok(bytes)
    }

    pub async fn asset(
        &self,
        head: &AssetHeadV1,
        size_bytes: u64,
    ) -> Result<Vec<u8>, HttpClientError> {
        let (digest, name, media_type) = managed_asset_identity(head)?;
        let limit = usize::try_from(size_bytes).map_err(|_| HttpClientError::InvalidUpload)?;
        if size_bytes == 0 || size_bytes > MAX_OFFLINE_ASSET_BYTES {
            return Err(HttpClientError::InvalidUpload);
        }
        let bytes = execute_managed_asset(
            self.client.get(self.asset_endpoint(digest, name)?),
            limit,
            media_type,
            head.revision,
        )
        .await?;
        let actual_size = u64::try_from(bytes.len()).map_err(|_| invalid_response())?;
        if actual_size != size_bytes || format!("{:x}", Sha256::digest(&bytes)) != digest {
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
        let response: StagedBlobV1 =
            execute_json_exact(request, MAX_ERROR_BODY_BYTES, StatusCode::CREATED).await?;
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

    pub async fn create_note(
        &self,
        operation_id: &str,
        input: &NoteWriteRequestV1,
    ) -> Result<DocumentMutationResultV1, HttpClientError> {
        require_operation_id(operation_id)?;
        validate_note_input(input)?;
        let body = encode_document_mutation(input)?;
        let url = self.endpoint("v1/document-mutations")?;
        let request = self
            .client
            .post(url)
            .header("Idempotency-Key", operation_id)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body);
        execute_document_mutation(
            request,
            StatusCode::OK,
            operation_id,
            DocumentMutationExpectation {
                document_id: None,
                active: true,
                logical_path: Some(input.logical_path.clone()),
                content_type: Some(input.content_type.clone()),
                content_sha256: Some(format!("{:x}", Sha256::digest(input.source.as_bytes()))),
                prior_revision: None,
            },
        )
        .await
    }

    pub async fn replace_note(
        &self,
        document_id: &str,
        operation_id: &str,
        expected_revision: u64,
        input: &NoteWriteRequestV1,
    ) -> Result<DocumentMutationResultV1, HttpClientError> {
        validate_document_mutation_request(document_id, Some(expected_revision))?;
        require_operation_id(operation_id)?;
        validate_note_input(input)?;
        let body = encode_document_mutation(input)?;
        let url = self.document_mutation_endpoint(document_id)?;
        let request = self
            .client
            .put(url)
            .header("Idempotency-Key", operation_id)
            .header("If-Match", expected_revision)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body);
        execute_document_mutation(
            request,
            StatusCode::OK,
            operation_id,
            DocumentMutationExpectation {
                document_id: Some(document_id.to_owned()),
                active: true,
                logical_path: Some(input.logical_path.clone()),
                content_type: Some(input.content_type.clone()),
                content_sha256: Some(format!("{:x}", Sha256::digest(input.source.as_bytes()))),
                prior_revision: Some(expected_revision),
            },
        )
        .await
    }

    pub async fn rename_note(
        &self,
        document_id: &str,
        operation_id: &str,
        expected_revision: u64,
        expected_content_type: &str,
        input: &NoteRenameRequestV1,
    ) -> Result<DocumentMutationResultV1, HttpClientError> {
        validate_document_mutation_request(document_id, Some(expected_revision))?;
        require_operation_id(operation_id)?;
        let body = encode_document_mutation(input)?;
        let url = self.document_mutation_action_endpoint(document_id, "rename")?;
        let request = self
            .client
            .post(url)
            .header("Idempotency-Key", operation_id)
            .header("If-Match", expected_revision)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body);
        execute_document_mutation(
            request,
            StatusCode::OK,
            operation_id,
            DocumentMutationExpectation {
                document_id: Some(document_id.to_owned()),
                active: true,
                logical_path: Some(input.logical_path.clone()),
                content_type: Some(expected_content_type.to_owned()),
                content_sha256: None,
                prior_revision: Some(expected_revision),
            },
        )
        .await
    }

    pub async fn tombstone_note(
        &self,
        document_id: &str,
        operation_id: &str,
        expected_revision: u64,
        expected_content_type: &str,
    ) -> Result<DocumentMutationResultV1, HttpClientError> {
        validate_document_mutation_request(document_id, Some(expected_revision))?;
        require_operation_id(operation_id)?;
        let url = self.document_mutation_endpoint(document_id)?;
        let request = self
            .client
            .delete(url)
            .header("Idempotency-Key", operation_id)
            .header("If-Match", expected_revision);
        execute_document_mutation(
            request,
            StatusCode::OK,
            operation_id,
            DocumentMutationExpectation {
                document_id: Some(document_id.to_owned()),
                active: false,
                logical_path: None,
                content_type: Some(expected_content_type.to_owned()),
                content_sha256: None,
                prior_revision: Some(expected_revision),
            },
        )
        .await
    }

    pub async fn put_asset(
        &self,
        digest: &str,
        name: &str,
        operation_id: &str,
        expected_revision: Option<u64>,
        media_type: &str,
        bytes: &[u8],
    ) -> Result<DesiredStateMutationResultV1, HttpClientError> {
        require_digest(digest)?;
        require_operation_id(operation_id)?;
        if expected_revision == Some(0) {
            return Err(HttpClientError::InvalidMutation);
        }
        let logical_path = canonical_http_asset_path(digest, name)?;
        let size_bytes = u64::try_from(bytes.len()).map_err(|_| HttpClientError::InvalidUpload)?;
        if validate_blob_metadata(size_bytes, media_type).is_err()
            || format!("{:x}", Sha256::digest(bytes)) != digest
        {
            return Err(HttpClientError::InvalidUpload);
        }
        let media_type =
            HeaderValue::from_str(media_type).map_err(|_| HttpClientError::InvalidUpload)?;
        let mutation = DeviceDesiredStateMutation::PutAsset {
            prepared: prepare_asset(logical_path.clone(), name.to_owned(), expected_revision)
                .map_err(|_| HttpClientError::InvalidUpload)?,
            digest: digest.to_owned(),
            body_sha256: digest.to_owned(),
            size_bytes,
            media_type: media_type
                .to_str()
                .map_err(|_| HttpClientError::InvalidUpload)?
                .to_owned(),
        };
        let request_sha256 = wire_request_sha256(&mutation)?;
        let mut request = self
            .client
            .put(self.asset_endpoint(digest, name)?)
            .header("Idempotency-Key", operation_id)
            .header(CONTENT_TYPE, media_type.clone())
            .body(bytes.to_vec());
        if let Some(revision) = expected_revision {
            request = request.header("If-Match", revision);
        }
        execute_desired_state_mutation(
            request,
            operation_id,
            &request_sha256,
            DesiredMutationExpectation::Asset {
                logical_path,
                digest: Some(digest.to_owned()),
                media_type: Some(
                    media_type
                        .to_str()
                        .map_err(|_| HttpClientError::InvalidUpload)?
                        .to_owned(),
                ),
                active: true,
                expected_revision,
            },
        )
        .await
    }

    pub async fn tombstone_asset(
        &self,
        digest: &str,
        name: &str,
        operation_id: &str,
        expected_revision: u64,
    ) -> Result<DesiredStateMutationResultV1, HttpClientError> {
        require_digest(digest)?;
        require_operation_id(operation_id)?;
        if expected_revision == 0 {
            return Err(HttpClientError::InvalidMutation);
        }
        let logical_path = canonical_http_asset_path(digest, name)?;
        let request_sha256 = wire_request_sha256(&DeviceDesiredStateMutation::TombstoneAsset {
            asset_path: logical_path.clone(),
            expected_revision,
        })?;
        let request = self
            .client
            .delete(self.asset_endpoint(digest, name)?)
            .header("Idempotency-Key", operation_id)
            .header("If-Match", expected_revision);
        execute_desired_state_mutation(
            request,
            operation_id,
            &request_sha256,
            DesiredMutationExpectation::Asset {
                logical_path,
                digest: None,
                media_type: None,
                active: false,
                expected_revision: Some(expected_revision),
            },
        )
        .await
    }

    pub async fn create_thema(
        &self,
        operation_id: &str,
        input: &ThemaWriteRequestV1,
    ) -> Result<DesiredStateMutationResultV1, HttpClientError> {
        let mutation = desired_thema_mutation(None, input, None)?;
        let request_sha256 = wire_request_sha256(&mutation)?;
        self.send_desired_state_json(
            self.client.post(self.endpoint("v1/themata")?),
            operation_id,
            None,
            input,
            &request_sha256,
            DesiredMutationExpectation::Thema {
                thema_id: None,
                input: Some(input.clone()),
                archived: Some(false),
                expected_revision: None,
            },
        )
        .await
    }

    pub async fn replace_thema(
        &self,
        thema_id: &str,
        operation_id: &str,
        expected_revision: u64,
        input: &ThemaWriteRequestV1,
    ) -> Result<DesiredStateMutationResultV1, HttpClientError> {
        let mutation = desired_thema_mutation(Some(thema_id), input, Some(expected_revision))?;
        let request_sha256 = wire_request_sha256(&mutation)?;
        self.send_desired_state_json(
            self.client.put(self.thema_endpoint(thema_id, None)?),
            operation_id,
            Some(expected_revision),
            input,
            &request_sha256,
            DesiredMutationExpectation::Thema {
                thema_id: Some(thema_id.to_owned()),
                input: Some(input.clone()),
                archived: None,
                expected_revision: Some(expected_revision),
            },
        )
        .await
    }

    pub async fn set_thema_archived(
        &self,
        thema_id: &str,
        operation_id: &str,
        expected_revision: u64,
        archived: bool,
    ) -> Result<DesiredStateMutationResultV1, HttpClientError> {
        require_operation_id(operation_id)?;
        let request_sha256 = wire_request_sha256(&DeviceDesiredStateMutation::SetThemaArchived {
            thema_id: ThemaId::new(thema_id.to_owned())
                .map_err(|_| HttpClientError::InvalidMutation)?,
            expected_revision,
            archived,
        })?;
        let suffix = if archived { "archive" } else { "unarchive" };
        let request = self
            .client
            .post(self.thema_endpoint(thema_id, Some(suffix))?)
            .header("Idempotency-Key", operation_id)
            .header("If-Match", expected_revision);
        execute_desired_state_mutation(
            request,
            operation_id,
            &request_sha256,
            DesiredMutationExpectation::Thema {
                thema_id: Some(thema_id.to_owned()),
                input: None,
                archived: Some(archived),
                expected_revision: Some(expected_revision),
            },
        )
        .await
    }

    pub async fn set_thema_suspended(
        &self,
        thema_id: &str,
        operation_id: &str,
        expected_state_revision: u64,
        suspended: bool,
    ) -> Result<DesiredStateMutationResultV1, HttpClientError> {
        require_operation_id(operation_id)?;
        let thema_id_value =
            ThemaId::new(thema_id.to_owned()).map_err(|_| HttpClientError::InvalidMutation)?;
        let request_sha256 = wire_request_sha256(&DeviceDesiredStateMutation::SetThemaSuspended {
            thema_id: thema_id_value,
            expected_state_revision,
            suspended,
        })?;
        let request = self
            .client
            .put(self.thema_endpoint(thema_id, Some("suspension"))?)
            .header("Idempotency-Key", operation_id)
            .header("If-Match", expected_state_revision)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(encode_document_mutation(&SetThemaSuspensionRequestV1 {
                suspended,
            })?);
        execute_desired_state_mutation(
            request,
            operation_id,
            &request_sha256,
            DesiredMutationExpectation::ReviewState {
                thema_id: thema_id.to_owned(),
                suspended,
                expected_state_revision,
            },
        )
        .await
    }

    pub async fn update_settings(
        &self,
        operation_id: &str,
        expected_revision: u64,
        input: &UpdateSettingsRequestV1,
    ) -> Result<DesiredStateMutationResultV1, HttpClientError> {
        let request_sha256 = wire_request_sha256(&DeviceDesiredStateMutation::ReplaceSettings {
            input: input.clone().into_application(),
            expected_revision,
        })?;
        self.send_desired_state_json(
            self.client.put(self.endpoint("v1/settings")?),
            operation_id,
            Some(expected_revision),
            input,
            &request_sha256,
            DesiredMutationExpectation::Settings {
                input: input.clone(),
                expected_revision,
            },
        )
        .await
    }

    pub async fn create_review_session(
        &self,
        input: &ReviewSessionRequestV1,
    ) -> Result<ReviewSessionV1, HttpClientError> {
        if input.source.clone().into_application().validate().is_err()
            || input
                .target_count
                .is_some_and(|count| !(1..=MAX_ONLINE_REVIEW_TARGET_COUNT).contains(&count))
        {
            return Err(HttpClientError::InvalidReview);
        }
        let request = self.post_review_json("v1/review/sessions", input)?;
        let session = checked_online_session(
            execute_json_exact(
                request,
                MAX_OFFLINE_REVIEW_RESULT_BYTES,
                StatusCode::CREATED,
            )
            .await?,
            None,
        )?;
        if session.target_count
            != Some(
                input
                    .target_count
                    .unwrap_or(DEFAULT_ONLINE_REVIEW_TARGET_COUNT),
            )
        {
            return Err(invalid_response());
        }
        Ok(session)
    }

    pub async fn dashboard(&self, days: Option<u16>) -> Result<DashboardV1, HttpClientError> {
        let mut url = self.endpoint("v1/dashboard")?;
        if let Some(days) = days {
            if !(1..=365).contains(&days) {
                return Err(HttpClientError::InvalidReview);
            }
            url.query_pairs_mut().append_pair("days", &days.to_string());
        }
        execute_json(self.client.get(url), MAX_REPLICA_PAGE_BYTES).await
    }

    pub async fn node(&self, node_id: &str) -> Result<NodeV1, HttpClientError> {
        let node_id =
            NodeId::new(node_id.to_owned()).map_err(|_| HttpClientError::InvalidMutation)?;
        let url = self.resource_endpoint("v1/nodes", node_id.as_str())?;
        let node = execute_json_exact(self.client.get(url), MAX_FRAME_SIZE, StatusCode::OK).await?;
        checked_node(node, Some(&node_id))
    }

    pub async fn nodes(
        &self,
        query: &str,
        limit: u16,
        after: Option<&str>,
    ) -> Result<NodePageV1, HttpClientError> {
        if !(1..=500).contains(&limit) {
            return Err(HttpClientError::InvalidMutation);
        }
        let after = after
            .map(|value| NodeId::new(value.to_owned()))
            .transpose()
            .map_err(|_| HttpClientError::InvalidCursor)?;
        let mut url = self.endpoint("v1/nodes")?;
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("q", query);
        pairs.append_pair("limit", &limit.to_string());
        if let Some(after) = &after {
            pairs.append_pair("after", after.as_str());
        }
        drop(pairs);
        let page = execute_json_exact(self.client.get(url), MAX_FRAME_SIZE, StatusCode::OK).await?;
        checked_node_page(page, limit, after.as_ref())
    }

    pub async fn themata(
        &self,
        query: &str,
        limit: u16,
        after: Option<&str>,
    ) -> Result<ThemaPageV1, HttpClientError> {
        if !(1..=500).contains(&limit) {
            return Err(HttpClientError::InvalidMutation);
        }
        let after = after
            .map(|value| ThemaId::new(value.to_owned()))
            .transpose()
            .map_err(|_| HttpClientError::InvalidCursor)?;
        let mut url = self.endpoint("v1/themata")?;
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("q", query);
        pairs.append_pair("limit", &limit.to_string());
        if let Some(after) = &after {
            pairs.append_pair("after", after.as_str());
        }
        drop(pairs);
        let page = execute_json_exact(self.client.get(url), MAX_FRAME_SIZE, StatusCode::OK).await?;
        checked_thema_page(page, limit, after.as_ref())
    }

    pub async fn thema_catalog(
        &self,
        query: &str,
        limit: u16,
        after: Option<&str>,
        expected_generation: Option<&str>,
    ) -> Result<ThemaCatalogPageV1, HttpClientError> {
        if !(1..=500).contains(&limit)
            || expected_generation.is_some_and(|value| !valid_result_cursor(value))
        {
            return Err(HttpClientError::InvalidMutation);
        }
        let after = after
            .map(|value| ThemaId::new(value.to_owned()))
            .transpose()
            .map_err(|_| HttpClientError::InvalidCursor)?;
        let mut url = self.endpoint("v1/themata/catalog")?;
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("q", query);
        pairs.append_pair("limit", &limit.to_string());
        if let Some(after) = &after {
            pairs.append_pair("after", after.as_str());
        }
        drop(pairs);
        let page = execute_json_exact(self.client.get(url), MAX_FRAME_SIZE, StatusCode::OK).await?;
        checked_thema_catalog_page(page, limit, after.as_ref(), expected_generation)
    }

    pub async fn document_source(
        &self,
        document_id: &str,
        revision: u64,
    ) -> Result<DocumentSource, HttpClientError> {
        if revision == 0 {
            return Err(HttpClientError::InvalidMutation);
        }
        let document_id = DocumentId::new(document_id.to_owned())
            .map_err(|_| HttpClientError::InvalidMutation)?;
        let url = self.document_revision_endpoint(&document_id, revision)?;
        execute_document_source(self.client.get(url), &document_id, revision).await
    }

    pub async fn node_source(&self, node_id: &str) -> Result<NodeSource, HttpClientError> {
        let node = self.node(node_id).await?;
        let span = required_node_span(&node)?;
        let document_id =
            DocumentId::new(node.document_id.clone()).map_err(|_| invalid_response())?;
        let document = self
            .document_source(document_id.as_str(), node.document_revision)
            .await?;
        if document.content_sha256 != node.content_sha256
            || validate_document_path(&document.content_type, &node.logical_path).is_err()
        {
            return Err(invalid_response());
        }
        SourceSpan::for_source(&document.source, span.start(), span.end())
            .map_err(|_| invalid_response())?;
        Ok(NodeSource { node, document })
    }

    pub async fn node_themata(
        &self,
        node_id: &str,
        limit: u16,
        after: Option<&str>,
    ) -> Result<NodeThemaAssociationPageV1, HttpClientError> {
        if !(1..=500).contains(&limit) {
            return Err(HttpClientError::InvalidMutation);
        }
        let node_id =
            NodeId::new(node_id.to_owned()).map_err(|_| HttpClientError::InvalidMutation)?;
        if let Some(after) = after {
            ThemaId::new(after.to_owned()).map_err(|_| HttpClientError::InvalidCursor)?;
        }
        let mut url = self.endpoint("v1/nodes/")?;
        url.path_segments_mut()
            .map_err(|_| HttpClientError::InvalidBaseUrl)?
            .pop_if_empty()
            .push(node_id.as_str())
            .push("themata");
        url.query_pairs_mut()
            .append_pair("limit", &limit.to_string());
        if let Some(after) = after {
            url.query_pairs_mut().append_pair("after", after);
        }
        let page = execute_json(self.client.get(url), MAX_FRAME_SIZE).await?;
        checked_node_themata_page(page, limit, after)
    }

    pub async fn import_document(
        &self,
        document_id: &str,
        operation_id: &str,
        input: &DocumentImportRequestV1,
    ) -> Result<DocumentMutationResultV1, HttpClientError> {
        validate_document_mutation_request(document_id, input.expected_revision)?;
        require_operation_id(operation_id)?;
        if input.source.len() > MAX_DOCUMENT_SOURCE_BYTES {
            return Err(HttpClientError::InvalidMutation);
        }
        let url = self.resource_endpoint("v1/documents", document_id)?;
        let body = serde_json::to_vec(input).map_err(|_| HttpClientError::InvalidMutation)?;
        if body.len() > MAX_OFFLINE_CONTENT_OPERATION_BYTES {
            return Err(HttpClientError::InvalidMutation);
        }
        let request = self
            .client
            .put(url)
            .header("Idempotency-Key", operation_id)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(body);
        execute_document_mutation(
            request,
            StatusCode::OK,
            operation_id,
            DocumentMutationExpectation {
                document_id: Some(document_id.to_owned()),
                active: true,
                logical_path: None,
                content_type: Some(input.content_type.clone()),
                content_sha256: Some(format!("{:x}", Sha256::digest(input.source.as_bytes()))),
                prior_revision: input.expected_revision,
            },
        )
        .await
    }

    pub async fn document(&self, document_id: &str) -> Result<DocumentView, HttpClientError> {
        let url = self.resource_endpoint("v1/documents", document_id)?;
        let document =
            execute_json_exact(self.client.get(url), MAX_FRAME_SIZE, StatusCode::OK).await?;
        validate_document_view(&document, document_id)?;
        Ok(document)
    }

    pub async fn thema(&self, thema_id: &str) -> Result<ThemaV1, HttpClientError> {
        let url = self.resource_endpoint("v1/themata", thema_id)?;
        let thema: ThemaV1 =
            execute_json_exact(self.client.get(url), MAX_FRAME_SIZE, StatusCode::OK).await?;
        if !validate_thema(&thema, thema_id) {
            return Err(invalid_response());
        }
        Ok(thema)
    }

    pub async fn due_page(
        &self,
        day: u32,
        limit: u16,
        after: Option<&str>,
    ) -> Result<DuePageView, HttpClientError> {
        let year = i32::try_from(day / 10_000).map_err(|_| HttpClientError::InvalidMutation)?;
        let month = (day / 100) % 100;
        let day_of_month = day % 100;
        if year == 0
            || NaiveDate::from_ymd_opt(year, month, day_of_month).is_none()
            || !(1..=100).contains(&limit)
            || after.is_some_and(|cursor| cursor.is_empty() || cursor.len() > MAX_DUE_CURSOR_BYTES)
        {
            return Err(HttpClientError::InvalidMutation);
        }
        let mut url = self.endpoint("v1/review/due")?;
        let mut pairs = url.query_pairs_mut();
        pairs.append_pair("day", &day.to_string());
        pairs.append_pair("limit", &limit.to_string());
        if let Some(after) = after {
            pairs.append_pair("after", after);
        }
        drop(pairs);
        let page = execute_json_exact(self.client.get(url), MAX_FRAME_SIZE, StatusCode::OK).await?;
        validate_due_page(&page, limit)?;
        Ok(page)
    }

    pub async fn review_sessions(
        &self,
        query: &ReviewSessionListRequestV1,
    ) -> Result<ReviewSessionPageV1, HttpClientError> {
        validate_review_page(query.after.as_deref(), query.limit)?;
        let mut url = self.endpoint("v1/review/sessions")?;
        append_review_page(&mut url, query.after.as_deref(), query.limit);
        if let Some(status) = query.status {
            url.query_pairs_mut().append_pair("status", status.as_str());
        }
        if let Some(mode) = query.mode {
            url.query_pairs_mut().append_pair("mode", mode.as_str());
        }
        execute_json(self.client.get(url), MAX_REPLICA_PAGE_BYTES).await
    }

    pub async fn review_session(
        &self,
        session_id: &str,
    ) -> Result<ReviewSessionSummaryV1, HttpClientError> {
        require_review_id(session_id)?;
        let url = self.review_resource_endpoint("sessions", session_id, None)?;
        execute_json(self.client.get(url), MAX_REPLICA_PAGE_BYTES).await
    }

    pub async fn review_history(
        &self,
        query: &ReviewHistoryRequestV1,
    ) -> Result<ReviewHistoryPageV1, HttpClientError> {
        validate_review_history_query(query)?;
        let mut url = self.endpoint("v1/review/history")?;
        append_review_page(&mut url, query.after.as_deref(), query.limit);
        let mut pairs = url.query_pairs_mut();
        if let Some(value) = query.thema_id.as_deref() {
            pairs.append_pair("thema_id", value);
        }
        if let Some(value) = query.session_id.as_deref() {
            pairs.append_pair("session_id", value);
        }
        if let Some(value) = query.from_day {
            pairs.append_pair("from_day", &value.to_string());
        }
        if let Some(value) = query.through_day {
            pairs.append_pair("through_day", &value.to_string());
        }
        if let Some(value) = query.accepted_success {
            pairs.append_pair("accepted_success", if value { "true" } else { "false" });
        }
        drop(pairs);
        execute_json(self.client.get(url), MAX_REPLICA_PAGE_BYTES).await
    }

    pub async fn review_queue_summary(
        &self,
        input: &ReviewQueueSummaryRequestV1,
    ) -> Result<ReviewQueueSummaryV1, HttpClientError> {
        let request = self.post_review_json("v1/review/queue/summary", input)?;
        execute_json(request, MAX_REPLICA_PAGE_BYTES).await
    }

    pub async fn thema_review_state(
        &self,
        thema_id: &str,
    ) -> Result<ThemaReviewStateV1, HttpClientError> {
        require_review_id(thema_id)?;
        let mut url = self.endpoint("v1/themata/")?;
        url.path_segments_mut()
            .map_err(|_| HttpClientError::InvalidBaseUrl)?
            .pop_if_empty()
            .push(thema_id)
            .push("review-state");
        execute_json(self.client.get(url), MAX_REPLICA_PAGE_BYTES).await
    }

    pub async fn active_review_session(&self) -> Result<ActiveReviewSessionV1, HttpClientError> {
        let active: ActiveReviewSessionV1 = self.get_json("v1/review/sessions/active").await?;
        if active.session.as_ref().is_none_or(valid_online_session) {
            Ok(active)
        } else {
            Err(invalid_response())
        }
    }

    pub async fn resume_review_session(
        &self,
        session_id: &str,
    ) -> Result<ReviewSessionV1, HttpClientError> {
        self.review_session_action(session_id, "resume").await
    }

    pub async fn quit_review_session(
        &self,
        session_id: &str,
    ) -> Result<ReviewSessionV1, HttpClientError> {
        self.review_session_action(session_id, "quit").await
    }

    pub async fn abort_review_session(
        &self,
        session_id: &str,
    ) -> Result<ReviewSessionV1, HttpClientError> {
        self.review_session_action(session_id, "abort").await
    }

    pub async fn begin_review_attempt(
        &self,
        session_id: &str,
    ) -> Result<ReviewAttemptBeginResponseV1, HttpClientError> {
        require_review_id(session_id)?;
        let url = self.review_resource_endpoint("sessions", session_id, Some("attempts"))?;
        let result: ReviewAttemptBeginResponseV1 = execute_json_exact(
            self.client.post(url),
            MAX_OFFLINE_REVIEW_RESULT_BYTES,
            StatusCode::CREATED,
        )
        .await?;
        checked_begin_result(result, session_id)
    }

    pub async fn answer_review_attempt(
        &self,
        attempt_id: &str,
        answer: &ReviewAnswerV1,
    ) -> Result<ReviewAttemptV1, HttpClientError> {
        require_review_id(attempt_id)?;
        let url = self.review_resource_endpoint("attempts", attempt_id, Some("answer"))?;
        let body = encode_review_body(answer)?;
        let request = json_request(self.client.post(url), body);
        let attempt = execute_json(request, MAX_OFFLINE_REVIEW_RESULT_BYTES).await?;
        checked_attempt(attempt, Some(attempt_id), None)
    }

    pub async fn check_review_clozes(
        &self,
        attempt_id: &str,
        input: &ReviewClozeCheckRequestV1,
    ) -> Result<ReviewClozeCheckV1, HttpClientError> {
        require_review_id(attempt_id)?;
        if input.inputs.is_empty() || input.inputs.len() > 500 {
            return Err(HttpClientError::InvalidReview);
        }
        let url = self.review_resource_endpoint("attempts", attempt_id, Some("cloze-check"))?;
        let request = json_request(self.client.post(url), encode_review_body(input)?);
        execute_json(request, MAX_OFFLINE_REVIEW_RESULT_BYTES).await
    }

    pub async fn review_attempt_state(
        &self,
        attempt_id: &str,
    ) -> Result<ReviewAttemptStateResponseV1, HttpClientError> {
        require_review_id(attempt_id)?;
        let url = self.review_resource_endpoint("attempts", attempt_id, None)?;
        let state = execute_json(self.client.get(url), MAX_OFFLINE_REVIEW_RESULT_BYTES).await?;
        checked_attempt_state(state, attempt_id)
    }

    pub async fn set_review_accepted_success(
        &self,
        attempt_id: &str,
        input: &SetAcceptedSuccessRequestV1,
    ) -> Result<ReviewAttemptOperationResponseV1, HttpClientError> {
        self.set_review_attempt_state(
            attempt_id,
            "accepted-success",
            input.version,
            &input.operation_id,
            input.expected_attempt_revision,
            input,
        )
        .await
    }

    pub async fn set_review_suspension(
        &self,
        attempt_id: &str,
        input: &SetSuspensionRequestV1,
    ) -> Result<ReviewAttemptOperationResponseV1, HttpClientError> {
        self.set_review_attempt_state(
            attempt_id,
            "suspension",
            input.version,
            &input.operation_id,
            input.expected_attempt_revision,
            input,
        )
        .await
    }

    pub async fn commit_review_attempt(
        &self,
        attempt_id: &str,
    ) -> Result<ReviewCommitResponseV1, HttpClientError> {
        require_review_id(attempt_id)?;
        let url = self.review_resource_endpoint("attempts", attempt_id, Some("commit"))?;
        let result: ReviewCommitResponseV1 =
            execute_json(self.client.post(url), MAX_OFFLINE_REVIEW_RESULT_BYTES).await?;
        require_review_id(&result.event_id)
            .is_ok()
            .then_some(result)
            .ok_or_else(invalid_response)
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

    fn resource_endpoint(&self, collection: &str, id: &str) -> Result<Url, HttpClientError> {
        let mut url = self.endpoint(&format!("{collection}/"))?;
        url.path_segments_mut()
            .map_err(|_| HttpClientError::InvalidBaseUrl)?
            .pop_if_empty()
            .push(id);
        Ok(url)
    }

    fn document_revision_endpoint(
        &self,
        document_id: &DocumentId,
        revision: u64,
    ) -> Result<Url, HttpClientError> {
        let mut url = self.resource_endpoint("v1/notes", document_id.as_str())?;
        url.path_segments_mut()
            .map_err(|_| HttpClientError::InvalidBaseUrl)?
            .push("revisions")
            .push(&revision.to_string());
        Ok(url)
    }

    fn document_mutation_endpoint(&self, document_id: &str) -> Result<Url, HttpClientError> {
        let mut url = self.endpoint("v1/document-mutations/")?;
        url.path_segments_mut()
            .map_err(|_| HttpClientError::InvalidBaseUrl)?
            .pop_if_empty()
            .push(document_id);
        Ok(url)
    }

    fn document_mutation_action_endpoint(
        &self,
        document_id: &str,
        action: &str,
    ) -> Result<Url, HttpClientError> {
        let mut url = self.document_mutation_endpoint(document_id)?;
        url.path_segments_mut()
            .map_err(|_| HttpClientError::InvalidBaseUrl)?
            .push(action);
        Ok(url)
    }

    fn asset_endpoint(&self, digest: &str, name: &str) -> Result<Url, HttpClientError> {
        canonical_http_asset_path(digest, name)?;
        let mut url = self.endpoint("v1/assets/")?;
        url.path_segments_mut()
            .map_err(|_| HttpClientError::InvalidBaseUrl)?
            .pop_if_empty()
            .push(digest)
            .push(name);
        Ok(url)
    }

    fn thema_endpoint(&self, thema_id: &str, action: Option<&str>) -> Result<Url, HttpClientError> {
        if thema_id.is_empty() {
            return Err(HttpClientError::InvalidMutation);
        }
        let mut url = self.endpoint("v1/themata/")?;
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| HttpClientError::InvalidBaseUrl)?;
        segments.pop_if_empty().push(thema_id);
        if let Some(action) = action {
            segments.push(action);
        }
        drop(segments);
        Ok(url)
    }

    async fn send_desired_state_json(
        &self,
        mut request: RequestBuilder,
        operation_id: &str,
        expected_revision: Option<u64>,
        input: &impl Serialize,
        request_sha256: &str,
        expectation: DesiredMutationExpectation,
    ) -> Result<DesiredStateMutationResultV1, HttpClientError> {
        require_operation_id(operation_id)?;
        request = json_request(
            request.header("Idempotency-Key", operation_id),
            encode_document_mutation(input)?,
        );
        if let Some(revision) = expected_revision {
            if revision == 0 {
                return Err(HttpClientError::InvalidMutation);
            }
            request = request.header("If-Match", revision);
        }
        execute_desired_state_mutation(request, operation_id, request_sha256, expectation).await
    }

    fn post_review_json(
        &self,
        path: &str,
        input: &impl Serialize,
    ) -> Result<RequestBuilder, HttpClientError> {
        let url = self.endpoint(path)?;
        Ok(json_request(
            self.client.post(url),
            encode_review_body(input)?,
        ))
    }

    async fn review_session_action(
        &self,
        session_id: &str,
        action: &str,
    ) -> Result<ReviewSessionV1, HttpClientError> {
        require_review_id(session_id)?;
        let url = self.review_resource_endpoint("sessions", session_id, Some(action))?;
        let session: ReviewSessionV1 =
            execute_json(self.client.post(url), MAX_OFFLINE_REVIEW_RESULT_BYTES).await?;
        checked_online_session(session, Some(session_id))
    }

    async fn set_review_attempt_state(
        &self,
        attempt_id: &str,
        action: &str,
        version: u32,
        operation_id: &str,
        expected_revision: u64,
        input: &impl Serialize,
    ) -> Result<ReviewAttemptOperationResponseV1, HttpClientError> {
        require_review_operation(attempt_id, version, operation_id, expected_revision)?;
        let url = self.review_resource_endpoint("attempts", attempt_id, Some(action))?;
        let request = json_request(self.client.put(url), encode_review_body(input)?);
        let result = execute_json(request, MAX_OFFLINE_REVIEW_RESULT_BYTES).await?;
        checked_attempt_operation(result, operation_id, attempt_id, expected_revision)
    }

    fn review_resource_endpoint(
        &self,
        collection: &str,
        id: &str,
        action: Option<&str>,
    ) -> Result<Url, HttpClientError> {
        let mut url = self.endpoint("v1/review/")?;
        let mut segments = url
            .path_segments_mut()
            .map_err(|_| HttpClientError::InvalidBaseUrl)?;
        segments.pop_if_empty().push(collection).push(id);
        if let Some(action) = action {
            segments.push(action);
        }
        drop(segments);
        Ok(url)
    }

    fn review_pack_asset_endpoint(
        &self,
        pack_id: &str,
        asset: &ReviewPackAssetV2,
    ) -> Result<Url, HttpClientError> {
        require_review_id(pack_id)?;
        require_pack_asset(asset)?;
        let name = asset_name(asset)?;
        let mut url = self.endpoint("v1/review/packs/")?;
        url.path_segments_mut()
            .map_err(|_| HttpClientError::InvalidBaseUrl)?
            .pop_if_empty()
            .push(pack_id)
            .push("assets")
            .push(&asset.content_sha256)
            .push(name);
        Ok(url)
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

fn checked_node_themata_page(
    page: NodeThemaAssociationPageV1,
    limit: u16,
    after: Option<&str>,
) -> Result<NodeThemaAssociationPageV1, HttpClientError> {
    if page.items.len() > usize::from(limit)
        || page
            .items
            .iter()
            .any(|item| ThemaId::new(item.thema_id.clone()).is_err())
        || page
            .items
            .windows(2)
            .any(|items| items[0].thema_id >= items[1].thema_id)
        || after.is_some_and(|after| {
            page.items
                .first()
                .is_some_and(|item| item.thema_id.as_str() <= after)
        })
        || page.next.as_deref() != page.items.last().map(|item| item.thema_id.as_str())
    {
        return Err(invalid_response());
    }
    Ok(page)
}

fn checked_node(node: NodeV1, expected: Option<&NodeId>) -> Result<NodeV1, HttpClientError> {
    let node_id = NodeId::new(node.node_id.clone()).map_err(|_| invalid_response())?;
    if expected.is_some_and(|expected| expected != &node_id)
        || DocumentId::new(node.document_id.clone()).is_err()
        || crate::application::validate_document_path(&node.logical_path).is_err()
        || node.document_revision == 0
        || !canonical_hex(&node.content_sha256, 64)
        || !all_unique(&node.tags)
        || node_span(&node).is_err()
        || Node::new(
            node_id,
            node.title.clone(),
            TitlePath::new(node.title_path.clone()).map_err(|_| invalid_response())?,
            node.tags.clone(),
            None,
            node.level,
        )
        .is_err()
    {
        return Err(invalid_response());
    }
    Ok(node)
}

fn checked_node_page(
    page: NodePageV1,
    limit: u16,
    after: Option<&NodeId>,
) -> Result<NodePageV1, HttpClientError> {
    if page.items.len() > usize::from(limit)
        || page
            .items
            .windows(2)
            .any(|items| items[0].node_id >= items[1].node_id)
        || after.is_some_and(|after| {
            page.items
                .first()
                .is_some_and(|item| item.node_id.as_str() <= after.as_str())
        })
        || page.next.as_deref() != page.items.last().map(|item| item.node_id.as_str())
    {
        return Err(invalid_response());
    }
    let items = page
        .items
        .into_iter()
        .map(|node| checked_node(node, None))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(NodePageV1 {
        items,
        next: page.next,
    })
}

fn checked_thema_page(
    page: ThemaPageV1,
    limit: u16,
    after: Option<&ThemaId>,
) -> Result<ThemaPageV1, HttpClientError> {
    if page.items.len() > usize::from(limit)
        || page
            .items
            .iter()
            .any(|thema| !validate_thema(thema, &thema.thema_id))
        || page
            .items
            .windows(2)
            .any(|items| items[0].thema_id >= items[1].thema_id)
        || after.is_some_and(|after| {
            page.items
                .first()
                .is_some_and(|item| item.thema_id.as_str() <= after.as_str())
        })
        || page.next.as_deref() != page.items.last().map(|item| item.thema_id.as_str())
    {
        return Err(invalid_response());
    }
    Ok(page)
}

fn checked_thema_catalog_page(
    page: ThemaCatalogPageV1,
    limit: u16,
    after: Option<&ThemaId>,
    expected_generation: Option<&str>,
) -> Result<ThemaCatalogPageV1, HttpClientError> {
    if !valid_result_cursor(&page.generation)
        || expected_generation.is_some_and(|expected| page.generation != expected)
        || page.items.len() > usize::from(limit)
        || page.items.iter().any(|item| !valid_catalog_item(item))
        || page
            .items
            .windows(2)
            .any(|items| items[0].thema_id >= items[1].thema_id)
        || after.is_some_and(|after| {
            page.items
                .first()
                .is_some_and(|item| item.thema_id.as_str() <= after.as_str())
        })
        || page
            .next
            .as_deref()
            .is_some_and(|next| Some(next) != page.items.last().map(|item| item.thema_id.as_str()))
    {
        return Err(invalid_response());
    }
    Ok(page)
}

fn valid_catalog_item(item: &ThemaCatalogItemV1) -> bool {
    if ThemaId::new(item.thema_id.clone()).is_err()
        || item.thema_revision == 0
        || !all_unique(&item.tags)
    {
        return false;
    }
    let kind = match item.kind {
        ThemaKindV1::Basic => "basic",
        ThemaKindV1::Cloze => "cloze",
        ThemaKindV1::Mcq => "mcq",
        ThemaKindV1::McCloze => "mc-cloze",
    };
    thema_content(
        kind,
        item.keimenon.clone(),
        item.hypothesis.clone(),
        item.answer.clone(),
        None,
        None,
        None,
    )
    .is_ok()
}

fn valid_review_state(state: &ReviewStateV1) -> bool {
    state.total_successes.saturating_add(state.total_failures) == state.review_count
        && state.last_reviewed_at_us.is_some() == state.last_review_day.is_some()
        && state.interval_days.is_some() == state.stability.is_some()
        && state.stability.is_some() == state.difficulty.is_some()
        && state
            .stability
            .is_none_or(|value| value.is_finite() && value > 0.0)
        && state
            .difficulty
            .is_none_or(|value| value.is_finite() && (1.0..=10.0).contains(&value))
}

fn node_span(node: &NodeV1) -> Result<Option<SourceSpan>, HttpClientError> {
    match (node.start_byte, node.end_byte) {
        (None, None) => Ok(None),
        (Some(start), Some(end)) if end <= MAX_DOCUMENT_SOURCE_BYTES => {
            SourceSpan::from_offsets(start, end)
                .map(Some)
                .map_err(|_| invalid_response())
        }
        _ => Err(invalid_response()),
    }
}

fn required_node_span(node: &NodeV1) -> Result<SourceSpan, HttpClientError> {
    node_span(node)?.ok_or(HttpClientError::NodeLocationUnavailable)
}

fn validate_document_view(
    document: &DocumentView,
    document_id: &str,
) -> Result<(), HttpClientError> {
    if document.document_id != document_id
        || document.revision == 0
        || DocumentId::new(document.document_id.clone()).is_err()
        || !canonical_hex(&document.content_sha256, 64)
        || !all_unique(&document.file_tags)
        || document.nodes.iter().any(|node| !all_unique(&node.tags))
    {
        return Err(invalid_response());
    }
    let nodes = document
        .nodes
        .iter()
        .map(|node| {
            let input = (
                NodeId::new(node.id.clone()).map_err(|_| invalid_response())?,
                node.title.clone(),
                TitlePath::new(node.title_path.clone()).map_err(|_| invalid_response())?,
                node.tags.clone(),
                node.parent_id
                    .clone()
                    .map(NodeId::new)
                    .transpose()
                    .map_err(|_| invalid_response())?,
                node.level,
            );
            match (node.start_byte, node.end_byte) {
                (None, None) => Node::new(input.0, input.1, input.2, input.3, input.4, input.5),
                (Some(start), Some(end)) if end <= MAX_DOCUMENT_SOURCE_BYTES => {
                    let span =
                        SourceSpan::from_offsets(start, end).map_err(|_| invalid_response())?;
                    Node::new(input.0, input.1, input.2, input.3, input.4, input.5)
                        .map(|node| node.with_source_span(span))
                }
                _ => return Err(invalid_response()),
            }
            .map_err(|_| invalid_response())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let links = document
        .link_occurrences
        .iter()
        .map(|link| {
            Ok(LinkOccurrence::new(
                NodeId::new(link.owner_id.clone()).map_err(|_| invalid_response())?,
                NodeId::new(link.target_id.clone()).map_err(|_| invalid_response())?,
                SourceSpan::from_offsets(link.start_byte, link.end_byte)
                    .map_err(|_| invalid_response())?,
            ))
        })
        .collect::<Result<Vec<_>, HttpClientError>>()?;
    let projection = DocumentProjection::new(DocumentProjectionInput {
        title: document.title.clone(),
        file_node_id: document
            .file_node_id
            .clone()
            .map(NodeId::new)
            .transpose()
            .map_err(|_| invalid_response())?,
        file_tags: document.file_tags.clone(),
        nodes,
        link_occurrences: links,
        content_sha256: document.content_sha256.clone(),
    })
    .map_err(|_| invalid_response())?;
    let expected_edges = projection
        .semantic_edges()
        .iter()
        .map(|edge| (edge.source().as_str(), edge.target().as_str()));
    let actual_edges = document
        .semantic_edges
        .iter()
        .map(|edge| (edge.source_id.as_str(), edge.target_id.as_str()));
    if !expected_edges.eq(actual_edges) {
        return Err(invalid_response());
    }
    Ok(())
}

fn all_unique(values: &[String]) -> bool {
    let mut seen = HashSet::with_capacity(values.len());
    values.iter().all(|value| seen.insert(value.as_str()))
}

fn validate_due_page(page: &DuePageView, limit: u16) -> Result<(), HttpClientError> {
    if page.items.len() > usize::from(limit)
        || page.items.iter().any(|item| item.id.trim().is_empty())
        || page
            .items
            .windows(2)
            .any(|pair| due_key(&pair[0]) >= due_key(&pair[1]))
    {
        return Err(invalid_response());
    }
    match page.next.as_deref() {
        Some(next)
            if !page.items.is_empty() && !next.is_empty() && next.len() <= MAX_DUE_CURSOR_BYTES =>
        {
            Ok(())
        }
        None => Ok(()),
        _ => Err(invalid_response()),
    }
}

fn due_key(item: &crate::protocol::DueThemaView) -> (bool, u32, &str) {
    (
        item.review_count > 0,
        item.next_review_day,
        item.id.as_str(),
    )
}

fn canonical_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_review_page(after: Option<&str>, limit: Option<u16>) -> Result<(), HttpClientError> {
    if let Some(after) = after {
        require_review_id(after)?;
    }
    if limit.is_some_and(|limit| !(1..=500).contains(&limit)) {
        return Err(HttpClientError::InvalidReview);
    }
    Ok(())
}

fn append_review_page(url: &mut Url, after: Option<&str>, limit: Option<u16>) {
    let mut pairs = url.query_pairs_mut();
    if let Some(after) = after {
        pairs.append_pair("after", after);
    }
    if let Some(limit) = limit {
        pairs.append_pair("limit", &limit.to_string());
    }
}

fn validate_review_history_query(query: &ReviewHistoryRequestV1) -> Result<(), HttpClientError> {
    validate_review_page(query.after.as_deref(), query.limit)?;
    if let Some(value) = query.thema_id.as_deref() {
        require_review_id(value)?;
    }
    if let Some(value) = query.session_id.as_deref() {
        require_review_id(value)?;
    }
    if query.from_day.is_some_and(|day| !valid_review_day(day))
        || query.through_day.is_some_and(|day| !valid_review_day(day))
        || query
            .from_day
            .zip(query.through_day)
            .is_some_and(|(from, through)| from > through)
    {
        return Err(HttpClientError::InvalidReview);
    }
    Ok(())
}

fn valid_review_day(value: u32) -> bool {
    i32::try_from(value / 10_000)
        .is_ok_and(|year| NaiveDate::from_ymd_opt(year, value / 100 % 100, value % 100).is_some())
}

fn canonical_http_asset_path(digest: &str, name: &str) -> Result<String, HttpClientError> {
    let logical_path =
        canonical_asset_path(digest, name).map_err(|_| HttpClientError::InvalidUpload)?;
    if logical_path != format!("assets/{digest}/{name}") {
        return Err(HttpClientError::InvalidUpload);
    }
    Ok(logical_path)
}

fn managed_asset_identity(head: &AssetHeadV1) -> Result<(&str, &str, &str), HttpClientError> {
    validate_asset_path(&head.logical_path).map_err(|_| HttpClientError::InvalidUpload)?;
    let path = head
        .logical_path
        .strip_prefix("assets/")
        .and_then(|value| value.split_once('/'))
        .ok_or(HttpClientError::InvalidUpload)?;
    let digest = head
        .content_sha256
        .as_deref()
        .filter(|digest| *digest == path.0)
        .ok_or(HttpClientError::InvalidUpload)?;
    let media_type = head
        .media_type
        .as_deref()
        .filter(|value| HeaderValue::from_str(value).is_ok())
        .ok_or(HttpClientError::InvalidUpload)?;
    if !head.active || head.revision == 0 || validate_blob_metadata(1, media_type).is_err() {
        return Err(HttpClientError::InvalidUpload);
    }
    Ok((digest, path.1, media_type))
}

fn require_digest(digest: &str) -> Result<(), HttpClientError> {
    is_canonical_sha256(digest)
        .then_some(())
        .ok_or(HttpClientError::InvalidDigest)
}

fn require_operation_id(operation_id: &str) -> Result<(), HttpClientError> {
    valid_operation_id(operation_id)
        .then_some(())
        .ok_or(HttpClientError::InvalidOperationId)
}

pub fn validate_note_input(input: &NoteWriteRequestV1) -> Result<(), HttpClientError> {
    prepare_document(
        None,
        input.logical_path.clone(),
        input.content_type.clone(),
        input.source.clone(),
        None,
    )
    .map_err(|_| HttpClientError::InvalidMutation)?;
    encode_document_mutation(input).map(|_| ())
}

fn validate_document_mutation_request(
    document_id: &str,
    expected_revision: Option<u64>,
) -> Result<(), HttpClientError> {
    DocumentId::new(document_id).map_err(|_| HttpClientError::InvalidMutation)?;
    expected_revision
        .is_none_or(|revision| i64::try_from(revision).is_ok())
        .then_some(())
        .ok_or(HttpClientError::InvalidMutation)
}

fn encode_document_mutation(input: &impl Serialize) -> Result<Vec<u8>, HttpClientError> {
    let body = serde_json::to_vec(input).map_err(|_| HttpClientError::InvalidMutation)?;
    if body.len() > MAX_OFFLINE_CONTENT_OPERATION_BYTES {
        return Err(HttpClientError::InvalidMutation);
    }
    Ok(body)
}

fn encode_review_body(input: &impl Serialize) -> Result<Vec<u8>, HttpClientError> {
    let body = serde_json::to_vec(input).map_err(|_| HttpClientError::InvalidReview)?;
    if body.len() > MAX_OFFLINE_REVIEW_OPERATION_BYTES {
        return Err(HttpClientError::InvalidReview);
    }
    Ok(body)
}

fn json_request(request: RequestBuilder, body: Vec<u8>) -> RequestBuilder {
    request
        .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
        .body(body)
}

fn require_review_id(value: &str) -> Result<(), HttpClientError> {
    (!value.is_empty() && value.len() <= 128 && !value.contains('\0'))
        .then_some(())
        .ok_or(HttpClientError::InvalidReview)
}

fn require_review_operation(
    attempt_id: &str,
    version: u32,
    operation_id: &str,
    expected_revision: u64,
) -> Result<(), HttpClientError> {
    require_review_id(attempt_id)?;
    if version == 1 && valid_operation_id(operation_id) && i64::try_from(expected_revision).is_ok()
    {
        Ok(())
    } else {
        Err(HttpClientError::InvalidReview)
    }
}

fn checked_review_pack(
    response: ReviewPackEnvelopeV2,
    expected_id: Option<&str>,
    expected_status: Option<ReviewPackStatusV2>,
) -> Result<ReviewPackEnvelopeV2, HttpClientError> {
    if expected_id.is_some_and(|expected| response.pack.pack_id != expected)
        || expected_status.is_some_and(|expected| response.pack.status != expected)
        || decode_review_pack(response.clone()).is_err()
    {
        Err(invalid_response())
    } else {
        Ok(response)
    }
}

fn checked_active_review_pack(
    response: ActiveReviewPackEnvelopeV2,
) -> Result<ActiveReviewPackEnvelopeV2, HttpClientError> {
    let valid = response.pack.as_ref().is_none_or(|pack| {
        checked_review_pack(
            ReviewPackEnvelopeV2 {
                version: response.version,
                pack: pack.clone(),
            },
            None,
            Some(ReviewPackStatusV2::Active),
        )
        .is_ok()
    });
    valid.then_some(response).ok_or_else(invalid_response)
}

fn require_pack_asset(asset: &ReviewPackAssetV2) -> Result<(), HttpClientError> {
    let name = asset_name(asset)?;
    let expected = canonical_asset_path(&asset.content_sha256, name)
        .map_err(|_| HttpClientError::InvalidReview)?;
    if expected == asset.logical_path
        && asset.size_bytes <= MAX_OFFLINE_ASSET_BYTES
        && HeaderValue::from_str(&asset.media_type).is_ok()
    {
        Ok(())
    } else {
        Err(HttpClientError::InvalidReview)
    }
}

fn asset_name(asset: &ReviewPackAssetV2) -> Result<&str, HttpClientError> {
    asset
        .logical_path
        .rsplit_once('/')
        .map(|(_, name)| name)
        .filter(|name| !name.is_empty())
        .ok_or(HttpClientError::InvalidReview)
}

fn checked_pack_asset(
    bytes: Vec<u8>,
    asset: &ReviewPackAssetV2,
) -> Result<Vec<u8>, HttpClientError> {
    let size = u64::try_from(bytes.len()).map_err(|_| invalid_response())?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    if size == asset.size_bytes && digest == asset.content_sha256 {
        Ok(bytes)
    } else {
        Err(invalid_response())
    }
}

fn checked_online_session(
    session: ReviewSessionV1,
    expected_id: Option<&str>,
) -> Result<ReviewSessionV1, HttpClientError> {
    if valid_online_session(&session)
        && expected_id.is_none_or(|expected| session.session_id == expected)
    {
        Ok(session)
    } else {
        Err(invalid_response())
    }
}

fn valid_online_session(session: &ReviewSessionV1) -> bool {
    require_review_id(&session.session_id).is_ok()
        && session.mode == ReviewSessionModeV1::Online
        && valid_session_position(session)
        && session.started_at_us > 0
}

fn valid_session_position(session: &ReviewSessionV1) -> bool {
    match session.status {
        ReviewSessionStatusV1::Active => session.current_position < session.queue_length,
        ReviewSessionStatusV1::Completed => session.current_position == session.queue_length,
        ReviewSessionStatusV1::Quit | ReviewSessionStatusV1::Aborted => {
            session.current_position <= session.queue_length
        }
    }
}

fn checked_begin_result(
    result: ReviewAttemptBeginResponseV1,
    session_id: &str,
) -> Result<ReviewAttemptBeginResponseV1, HttpClientError> {
    let skipped_end = u32::try_from(result.skipped.len())
        .ok()
        .and_then(|count| result.start_position.checked_add(count));
    let skipped_are_valid = skipped_end == Some(result.session.current_position)
        && result.skipped.iter().enumerate().all(|(offset, item)| {
            u32::try_from(offset)
                .ok()
                .and_then(|offset| result.start_position.checked_add(offset))
                == Some(item.position)
                && require_review_id(&item.thema_id).is_ok()
        });
    let attempt_is_valid = match (&result.session.status, &result.attempt) {
        (ReviewSessionStatusV1::Active, Some(attempt)) => {
            valid_attempt(attempt) && attempt.session_id == session_id
        }
        (ReviewSessionStatusV1::Completed, None) => true,
        _ => false,
    };
    if result.session.session_id == session_id
        && valid_online_session(&result.session)
        && skipped_are_valid
        && attempt_is_valid
    {
        Ok(result)
    } else {
        Err(invalid_response())
    }
}

fn checked_attempt(
    attempt: ReviewAttemptV1,
    attempt_id: Option<&str>,
    session_id: Option<&str>,
) -> Result<ReviewAttemptV1, HttpClientError> {
    if valid_attempt(&attempt)
        && attempt_id.is_none_or(|expected| attempt.attempt_id == expected)
        && session_id.is_none_or(|expected| attempt.session_id == expected)
    {
        Ok(attempt)
    } else {
        Err(invalid_response())
    }
}

fn valid_attempt(attempt: &ReviewAttemptV1) -> bool {
    attempt.has_valid_status_fields()
        && require_review_id(&attempt.attempt_id).is_ok()
        && require_review_id(&attempt.session_id).is_ok()
        && require_review_id(&attempt.thema_id).is_ok()
}

fn checked_attempt_state(
    state: ReviewAttemptStateResponseV1,
    attempt_id: &str,
) -> Result<ReviewAttemptStateResponseV1, HttpClientError> {
    if state.version == 1
        && i64::try_from(state.attempt_revision).is_ok()
        && valid_attempt(&state.attempt)
        && state.attempt.attempt_id == attempt_id
    {
        Ok(state)
    } else {
        Err(invalid_response())
    }
}

fn checked_attempt_operation(
    result: ReviewAttemptOperationResponseV1,
    operation_id: &str,
    attempt_id: &str,
    expected_revision: u64,
) -> Result<ReviewAttemptOperationResponseV1, HttpClientError> {
    if result.version == 1
        && result.operation_id == operation_id
        && expected_revision.checked_add(1) == Some(result.attempt_revision)
        && i64::try_from(result.attempt_revision).is_ok()
        && valid_attempt(&result.attempt)
        && result.attempt.attempt_id == attempt_id
    {
        Ok(result)
    } else {
        Err(invalid_response())
    }
}

async fn execute_document_mutation(
    request: RequestBuilder,
    expected_status: StatusCode,
    operation_id: &str,
    expectation: DocumentMutationExpectation,
) -> Result<DocumentMutationResultV1, HttpClientError> {
    let result =
        execute_json_exact(request, MAX_OFFLINE_CONTENT_RESULT_BYTES, expected_status).await?;
    validate_document_mutation_result(&result, operation_id, &expectation)
        .then_some(result)
        .ok_or_else(invalid_response)
}

fn wire_request_sha256(mutation: &DeviceDesiredStateMutation) -> Result<String, HttpClientError> {
    desired_state_wire_request_sha256(1, mutation).map_err(|_| HttpClientError::InvalidMutation)
}

fn desired_thema_mutation(
    thema_id: Option<&str>,
    input: &ThemaWriteRequestV1,
    expected_revision: Option<u64>,
) -> Result<DeviceDesiredStateMutation, HttpClientError> {
    let content = thema_content(
        &input.kind,
        input.keimenon.clone(),
        input.hypothesis.clone(),
        input.answer.clone(),
        input.source_guid.clone(),
        input.parathema.clone(),
        input.review_image.clone(),
    )
    .map_err(|_| HttpClientError::InvalidMutation)?;
    let prepared = prepare_thema(
        thema_id.map(str::to_owned),
        content,
        input.tags.clone(),
        input
            .concepts
            .iter()
            .map(|concept| (concept.node_id.clone(), concept.relation.into()))
            .collect(),
        input.asset_paths.clone(),
        expected_revision,
    )
    .map_err(|_| HttpClientError::InvalidMutation)?;
    Ok(DeviceDesiredStateMutation::PutThema {
        prepared,
        initial_review_day: 0,
    })
}

enum DesiredMutationExpectation {
    Asset {
        logical_path: String,
        digest: Option<String>,
        media_type: Option<String>,
        active: bool,
        expected_revision: Option<u64>,
    },
    Thema {
        thema_id: Option<String>,
        input: Option<ThemaWriteRequestV1>,
        archived: Option<bool>,
        expected_revision: Option<u64>,
    },
    ReviewState {
        thema_id: String,
        suspended: bool,
        expected_state_revision: u64,
    },
    Settings {
        input: UpdateSettingsRequestV1,
        expected_revision: u64,
    },
}

async fn execute_desired_state_mutation(
    request: RequestBuilder,
    operation_id: &str,
    request_sha256: &str,
    expectation: DesiredMutationExpectation,
) -> Result<DesiredStateMutationResultV1, HttpClientError> {
    let result: DesiredStateMutationResultV1 =
        execute_json_exact(request, MAX_FRAME_SIZE, StatusCode::OK).await?;
    let valid = match &result {
        DesiredStateMutationResultV1::Applied {
            format_version,
            operation_id: actual,
            changed,
            authoritative,
            cursor,
        } => {
            *format_version == 1
                && actual == operation_id
                && valid_result_cursor(cursor)
                && valid_desired_projection(authoritative, *changed, &expectation)
        }
        DesiredStateMutationResultV1::Rejected {
            format_version,
            operation_id: actual,
            request_sha256: actual_request_sha256,
            rejection,
            cursor,
        } => {
            *format_version == 1
                && actual == operation_id
                && actual_request_sha256 == request_sha256
                && valid_desired_rejection(rejection, &expectation)
                && valid_result_cursor(cursor)
        }
    };
    valid.then_some(result).ok_or_else(invalid_response)
}

fn valid_desired_rejection(
    rejection: &DocumentMutationRejectionV1,
    expectation: &DesiredMutationExpectation,
) -> bool {
    let expected_revision = match expectation {
        DesiredMutationExpectation::Asset {
            expected_revision, ..
        }
        | DesiredMutationExpectation::Thema {
            expected_revision, ..
        } => *expected_revision,
        DesiredMutationExpectation::ReviewState {
            expected_state_revision,
            ..
        } => Some(*expected_state_revision),
        DesiredMutationExpectation::Settings {
            expected_revision, ..
        } => Some(*expected_revision),
    };
    match rejection {
        DocumentMutationRejectionV1::RevisionConflict {
            expected_revision: actual,
            ..
        } => *actual == expected_revision,
        _ => true,
    }
}

fn valid_desired_projection(
    projection: &DesiredStateMutationProjectionV1,
    changed: bool,
    expectation: &DesiredMutationExpectation,
) -> bool {
    match (projection, expectation) {
        (
            DesiredStateMutationProjectionV1::Asset(asset),
            DesiredMutationExpectation::Asset {
                logical_path,
                digest,
                media_type,
                active,
                expected_revision,
            },
        ) => {
            asset.logical_path == *logical_path
                && asset.content_sha256 == *digest
                && asset.media_type == *media_type
                && asset.active == *active
                && valid_mutation_revision(asset.revision, *expected_revision, changed)
        }
        (
            DesiredStateMutationProjectionV1::Thema(thema),
            DesiredMutationExpectation::Thema {
                thema_id,
                input,
                archived,
                expected_revision,
            },
        ) => {
            validate_thema(thema, &thema.thema_id)
                && thema_id
                    .as_ref()
                    .is_none_or(|expected| thema.thema_id == *expected)
                && input
                    .as_ref()
                    .is_none_or(|expected| thema_matches_input(thema, expected))
                && archived.is_none_or(|expected| thema.archived == expected)
                && valid_mutation_revision(thema.revision, *expected_revision, changed)
        }
        (
            DesiredStateMutationProjectionV1::Settings(settings),
            DesiredMutationExpectation::Settings {
                input,
                expected_revision,
            },
        ) => {
            settings_matches_input(settings, input)
                && valid_mutation_revision(settings.revision, Some(*expected_revision), changed)
        }
        (
            DesiredStateMutationProjectionV1::ReviewState { thema_id, state },
            DesiredMutationExpectation::ReviewState {
                thema_id: expected_thema_id,
                suspended,
                expected_state_revision,
            },
        ) => {
            thema_id == expected_thema_id
                && state.suspended == *suspended
                && valid_review_state(state)
                && valid_mutation_revision(
                    state.state_revision,
                    Some(*expected_state_revision),
                    changed,
                )
        }
        _ => false,
    }
}

fn settings_matches_input(settings: &SettingsV1, input: &UpdateSettingsRequestV1) -> bool {
    let expected_scheduler = input.clone().into_application().scheduler();
    settings.timezone == input.timezone
        && settings.day_boundary_hour == input.day_boundary_hour
        && settings.scheduler == FsrsConfigV1::from(&expected_scheduler)
}

fn valid_mutation_revision(revision: u64, expected_revision: Option<u64>, changed: bool) -> bool {
    match (expected_revision, changed) {
        (None, true) => revision == 1,
        (None, false) => false,
        (Some(expected), false) => revision == expected,
        (Some(expected), true) => expected.checked_add(1) == Some(revision),
    }
}

fn thema_matches_input(thema: &ThemaV1, input: &ThemaWriteRequestV1) -> bool {
    let kind = match thema.kind {
        ThemaKindV1::Basic => "basic",
        ThemaKindV1::Cloze => "cloze",
        ThemaKindV1::Mcq => "mcq",
        ThemaKindV1::McCloze => "mc-cloze",
    };
    let mut tags = input.tags.clone();
    tags.sort();
    tags.dedup();
    let mut concepts = input.concepts.clone();
    concepts.sort_by(|left, right| left.node_id.cmp(&right.node_id));
    concepts.dedup();
    let mut asset_paths = input.asset_paths.clone();
    asset_paths.sort();
    asset_paths.dedup();
    kind == input.kind
        && thema.keimenon == input.keimenon
        && thema.hypothesis == input.hypothesis
        && thema.answer == input.answer
        && thema.source_guid == input.source_guid
        && thema.parathema == input.parathema
        && thema.review_image == input.review_image
        && thema.tags == tags
        && thema.concepts == concepts
        && thema.asset_paths == asset_paths
        && is_canonical_sha256(&thema.content_sha256)
}

#[derive(Clone)]
struct DocumentMutationExpectation {
    document_id: Option<String>,
    active: bool,
    logical_path: Option<String>,
    content_type: Option<String>,
    content_sha256: Option<String>,
    prior_revision: Option<u64>,
}

fn validate_document_mutation_result(
    result: &DocumentMutationResultV1,
    expected_operation_id: &str,
    expectation: &DocumentMutationExpectation,
) -> bool {
    match result {
        DocumentMutationResultV1::Applied {
            format_version,
            operation_id,
            authoritative,
            cursor,
            changed,
        } => {
            *format_version == 1
                && operation_id == expected_operation_id
                && validate_document_head(authoritative, expectation, *changed)
                && valid_result_cursor(cursor)
        }
        DocumentMutationResultV1::Rejected {
            format_version,
            operation_id,
            cursor,
            ..
        } => {
            *format_version == 1
                && operation_id == expected_operation_id
                && valid_result_cursor(cursor)
        }
    }
}

fn validate_document_head(
    head: &crate::protocol::http_v1::DocumentHeadV1,
    expectation: &DocumentMutationExpectation,
    changed: bool,
) -> bool {
    let expected_revision = match (expectation.prior_revision, changed) {
        (None, true) => Some(1),
        (Some(revision), true) => revision.checked_add(1),
        (Some(revision), false) => Some(revision),
        (None, false) => None,
    };
    expectation
        .document_id
        .as_deref()
        .is_none_or(|expected| head.document_id == expected)
        && DocumentId::new(head.document_id.clone()).is_ok()
        && validate_document_path(&head.content_type, &head.logical_path).is_ok()
        && expectation
            .logical_path
            .as_deref()
            .is_none_or(|expected| head.logical_path == expected)
        && expectation
            .content_type
            .as_deref()
            .is_none_or(|expected| head.content_type == expected)
        && expected_revision == Some(head.revision)
        && head.active == expectation.active
        && match (
            head.content_sha256.as_deref(),
            expectation.content_sha256.as_deref(),
            expectation.active,
        ) {
            (Some(actual), Some(expected), true) => actual == expected,
            (Some(actual), None, true) => is_canonical_sha256(actual),
            (None, None, false) => true,
            _ => false,
        }
}

fn validate_thema(thema: &ThemaV1, expected_id: &str) -> bool {
    if thema.thema_id != expected_id
        || thema.revision == 0
        || !is_canonical_sha256(&thema.content_sha256)
    {
        return false;
    }
    let kind = match thema.kind {
        ThemaKindV1::Basic => "basic",
        ThemaKindV1::Cloze => "cloze",
        ThemaKindV1::Mcq => "mcq",
        ThemaKindV1::McCloze => "mc-cloze",
    };
    let Ok(content) = thema_content(
        kind,
        thema.keimenon.clone(),
        thema.hypothesis.clone(),
        thema.answer.clone(),
        thema.source_guid.clone(),
        thema.parathema.clone(),
        thema.review_image.clone(),
    ) else {
        return false;
    };
    prepare_thema(
        Some(thema.thema_id.clone()),
        content,
        thema.tags.clone(),
        thema
            .concepts
            .iter()
            .map(|concept| (concept.node_id.clone(), concept.relation.into()))
            .collect(),
        thema.asset_paths.clone(),
        None,
    )
    .is_ok_and(|prepared| {
        prepared.content_sha256 == thema.content_sha256
            && prepared.tags == thema.tags
            && prepared.concepts.len() == thema.concepts.len()
            && prepared
                .concepts
                .iter()
                .zip(&thema.concepts)
                .all(|(prepared, wire)| {
                    prepared.concept_id().as_str() == wire.node_id
                        && prepared.relation() == wire.relation.into()
                })
            && prepared.asset_paths == thema.asset_paths
    })
}

fn valid_result_cursor(cursor: &str) -> bool {
    !cursor.is_empty() && cursor.len() <= MAX_REPLICA_CURSOR_BYTES
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
    execute_json_response(request, max_bytes, Some(StatusCode::OK)).await
}

async fn execute_json_exact<T: DeserializeOwned>(
    request: RequestBuilder,
    max_bytes: usize,
    expected: StatusCode,
) -> Result<T, HttpClientError> {
    execute_json_response(request, max_bytes, Some(expected)).await
}

async fn execute_json_response<T: DeserializeOwned>(
    request: RequestBuilder,
    max_bytes: usize,
    expected: Option<StatusCode>,
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
    if expected.is_some_and(|expected| status != expected) && status.is_success() {
        Err(invalid_response())
    } else if status.is_success() {
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
    if response.status() == StatusCode::OK {
        return read_bounded(response, max_bytes).await;
    }
    if response.status().is_success() {
        return Err(invalid_response());
    }
    if !is_json(&response) {
        return Err(invalid_response());
    }
    let status = response.status();
    let bytes = read_bounded(response, MAX_ERROR_BODY_BYTES).await?;
    server_error(status, &bytes)
}

async fn execute_document_source(
    request: RequestBuilder,
    document_id: &DocumentId,
    expected_revision: u64,
) -> Result<DocumentSource, HttpClientError> {
    let response = request.send().await.map_err(transport_error)?;
    let status = response.status();
    if status != StatusCode::OK {
        if status.is_success() {
            return Err(invalid_response());
        }
        return execute_error_response(response, status).await;
    }
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .filter(|value| supported_content_type(value))
        .map(str::to_owned)
        .ok_or_else(invalid_response)?;
    let expected_etag = format!("\"{expected_revision}\"");
    if response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok())
        != Some(expected_etag.as_str())
    {
        return Err(invalid_response());
    }
    let bytes = read_bounded(response, MAX_DOCUMENT_SOURCE_BYTES).await?;
    let content_sha256 = format!("{:x}", Sha256::digest(&bytes));
    let source = String::from_utf8(bytes).map_err(|_| invalid_response())?;
    Ok(DocumentSource {
        document_id: document_id.to_string(),
        revision: expected_revision,
        content_type,
        content_sha256,
        source,
    })
}

async fn execute_managed_asset(
    request: RequestBuilder,
    max_bytes: usize,
    expected_media_type: &str,
    expected_revision: u64,
) -> Result<Vec<u8>, HttpClientError> {
    let response = request.send().await.map_err(transport_error)?;
    let status = response.status();
    if status != StatusCode::OK {
        if status.is_success() {
            return Err(invalid_response());
        }
        return execute_error_response(response, status).await;
    }
    let media_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    let etag = response
        .headers()
        .get(ETAG)
        .and_then(|value| value.to_str().ok());
    if !media_type.is_some_and(|value| value.eq_ignore_ascii_case(expected_media_type))
        || etag != Some(format!("\"{expected_revision}\"").as_str())
    {
        return Err(invalid_response());
    }
    read_bounded(response, max_bytes).await
}

async fn execute_pack_asset(
    request: RequestBuilder,
    max_bytes: usize,
    expected_media_type: &str,
) -> Result<Vec<u8>, HttpClientError> {
    let response = request.send().await.map_err(transport_error)?;
    let status = response.status();
    if status.is_success() && status != StatusCode::OK {
        return Err(invalid_response());
    }
    if !status.is_success() {
        return execute_error_response(response, status).await;
    }
    let media_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok());
    if !media_type.is_some_and(|value| value.eq_ignore_ascii_case(expected_media_type)) {
        return Err(invalid_response());
    }
    read_bounded(response, max_bytes).await
}

async fn execute_error_response<T>(
    response: Response,
    status: StatusCode,
) -> Result<T, HttpClientError> {
    if !is_json(&response) {
        return Err(invalid_response());
    }
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
    let body: ApiErrorResponseV1 = serde_json::from_slice(bytes).map_err(|_| invalid_response())?;
    if !error_status_matches(status, body.error.code()) {
        return Err(invalid_response());
    }
    let delivery = server_error_delivery(&body);
    Err(HttpClientError::Server(HttpServerError {
        status: status.as_u16(),
        body,
        delivery,
    }))
}

fn error_status_matches(status: StatusCode, code: ApiErrorCodeV1) -> bool {
    let expected = match code {
        ApiErrorCodeV1::Unauthorized => StatusCode::UNAUTHORIZED,
        ApiErrorCodeV1::Forbidden => StatusCode::FORBIDDEN,
        ApiErrorCodeV1::NotFound => StatusCode::NOT_FOUND,
        ApiErrorCodeV1::InvalidRequest
        | ApiErrorCodeV1::UnsupportedOfflineReviewVersion
        | ApiErrorCodeV1::UnsupportedOfflineContentVersion
        | ApiErrorCodeV1::UnsupportedOfflineAssetVersion => StatusCode::BAD_REQUEST,
        ApiErrorCodeV1::PreconditionRequired => StatusCode::PRECONDITION_REQUIRED,
        ApiErrorCodeV1::Internal | ApiErrorCodeV1::ReplicaProjectionTooLarge => {
            StatusCode::INTERNAL_SERVER_ERROR
        }
        ApiErrorCodeV1::InvalidContent
        | ApiErrorCodeV1::InvalidBody
        | ApiErrorCodeV1::InvalidOfflineReviewOperation
        | ApiErrorCodeV1::InvalidOfflineContentOperation
        | ApiErrorCodeV1::InvalidOfflineAssetOperation
        | ApiErrorCodeV1::RenameRequired
        | ApiErrorCodeV1::InvalidBlobMetadata => StatusCode::UNPROCESSABLE_ENTITY,
        ApiErrorCodeV1::RequestTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
        ApiErrorCodeV1::RequestTimeout => StatusCode::REQUEST_TIMEOUT,
        ApiErrorCodeV1::SnapshotRequired
        | ApiErrorCodeV1::ActiveSession
        | ApiErrorCodeV1::ReviewStateConflict
        | ApiErrorCodeV1::IdempotencyConflict
        | ApiErrorCodeV1::DeviceSequenceGap
        | ApiErrorCodeV1::BlobRequired
        | ApiErrorCodeV1::BlobMetadataConflict => StatusCode::CONFLICT,
        ApiErrorCodeV1::RevisionConflict | ApiErrorCodeV1::StaleAttempt => {
            StatusCode::PRECONDITION_FAILED
        }
        ApiErrorCodeV1::Busy => StatusCode::SERVICE_UNAVAILABLE,
    };
    status == expected
}

fn server_error_delivery(body: &ApiErrorResponseV1) -> DeliveryStatus {
    match body.error.code() {
        ApiErrorCodeV1::RequestTimeout | ApiErrorCodeV1::Internal => DeliveryStatus::Unknown,
        _ => DeliveryStatus::Confirmed,
    }
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
    Confirmed,
    NotSent,
    Unknown,
}

impl fmt::Display for DeliveryStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Confirmed => "confirmed",
            Self::NotSent => "not-sent",
            Self::Unknown => "unknown",
        })
    }
}

#[derive(Debug, Error)]
#[error("Gnosis server returned HTTP {status}")]
pub struct HttpServerError {
    status: u16,
    body: ApiErrorResponseV1,
    delivery: DeliveryStatus,
}

impl HttpServerError {
    pub const fn status(&self) -> u16 {
        self.status
    }

    pub const fn body(&self) -> &ApiErrorResponseV1 {
        &self.body
    }

    pub const fn delivery(&self) -> DeliveryStatus {
        self.delivery
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
    #[error("Gnosis operation ID is invalid")]
    InvalidOperationId,
    #[error("Gnosis document mutation is invalid")]
    InvalidMutation,
    #[error("Gnosis review request is invalid")]
    InvalidReview,
    #[error("Gnosis node has no exact source location")]
    NodeLocationUnavailable,
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

impl HttpClientError {
    pub const fn delivery(&self) -> DeliveryStatus {
        match self {
            Self::Timeout { delivery }
            | Self::Transport { delivery }
            | Self::ResponseTooLarge { delivery }
            | Self::InvalidResponse { delivery } => *delivery,
            Self::Server(error) => error.delivery(),
            _ => DeliveryStatus::NotSent,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use tempfile::TempDir;
    use tokio::{
        io::{AsyncReadExt as _, AsyncWriteExt as _},
        net::TcpListener,
        sync::oneshot,
        time::timeout,
    };

    use super::*;
    use crate::{
        application::{
            ReviewSource, ReviewSourceKind, generate_device, prepare_note, prepare_thema,
            thema_content,
        },
        client_replica::{LocalReplica, OfflineAnswer, OfflineResponse},
        domain::ThemaConceptRelation,
        protocol::{
            DaemonConfig, DaemonServer,
            http_v1::{ReviewSourceInputV1, ReviewSourceKindV1, ThemaConceptRelationV1},
            offline_asset_capabilities, offline_content_capabilities, replica_snapshot_page,
            replica_state_response, review_pack_response,
        },
        service::ServiceRuntime,
    };

    #[test]
    fn begin_result_rejects_noncontiguous_skipped_positions() {
        let result: ReviewAttemptBeginResponseV1 = serde_json::from_value(serde_json::json!({
            "start_position": 0,
            "session": {
                "session_id": "session",
                "owner_device_id": "device",
                "mode": "online",
                "source": {
                    "kind": "manual",
                    "tags": [],
                    "exclude_tags": [],
                    "topic_node_id": null,
                    "topic_forward_depth": null,
                    "topic_backward_depth": null,
                    "manual_ids": ["thema-a", "thema-b", "thema-c", "thema-d"]
                },
                "target_count": 4,
                "status": "completed",
                "current_position": 4,
                "queue_length": 4,
                "started_at_us": 1
            },
            "attempt": null,
            "skipped": [
                {"position": 0, "thema_id": "thema-a", "reason": "archived"},
                {"position": 2, "thema_id": "thema-c", "reason": "suspended"}
            ]
        }))
        .unwrap();

        let mut valid = result.clone();
        valid.session.target_count = Some(3);
        valid.session.current_position = 3;
        valid.session.queue_length = 3;
        valid.skipped = serde_json::from_value(serde_json::json!([
            {"position": 0, "thema_id": "thema-a", "reason": "archived"},
            {"position": 1, "thema_id": "thema-b", "reason": "suspended"},
            {"position": 2, "thema_id": "thema-c", "reason": "archived"}
        ]))
        .unwrap();
        assert!(checked_begin_result(valid.clone(), "session").is_ok());

        valid.start_position = 3;
        valid.skipped.clear();
        assert!(checked_begin_result(valid, "session").is_ok());
        assert!(checked_begin_result(result, "session").is_err());
    }

    #[test]
    fn node_themata_validator_enforces_order_cursor_and_page_size() {
        let page: NodeThemaAssociationPageV1 = serde_json::from_value(serde_json::json!({
            "items": [
                {"thema_id": "thema-a", "relation": "primary"},
                {"thema_id": "thema-b", "relation": "supporting"}
            ],
            "next": "thema-b"
        }))
        .unwrap();
        assert!(checked_node_themata_page(page.clone(), 2, None).is_ok());
        assert!(checked_node_themata_page(page.clone(), 1, None).is_err());
        assert!(checked_node_themata_page(page.clone(), 2, Some("thema-a")).is_err());

        let mut wrong_next = page.clone();
        wrong_next.next = Some("thema-a".into());
        assert!(checked_node_themata_page(wrong_next, 2, None).is_err());
        let mut unordered = page;
        unordered.items.reverse();
        assert!(checked_node_themata_page(unordered, 2, None).is_err());
    }

    #[test]
    fn node_response_validator_binds_identity_digest_span_and_page_cursor() {
        let node = NodeV1 {
            node_id: "node".into(),
            document_id: "document".into(),
            logical_path: "note.org".into(),
            document_revision: 1,
            content_sha256: "a".repeat(64),
            title: "Node".into(),
            title_path: vec!["Node".into()],
            tags: vec![],
            level: 1,
            start_byte: Some(0),
            end_byte: Some(7),
            active: true,
        };
        let node_id = NodeId::new("node").unwrap();
        assert!(checked_node(node.clone(), Some(&node_id)).is_ok());
        assert!(
            checked_node_page(
                NodePageV1 {
                    items: vec![node.clone()],
                    next: Some("node".into()),
                },
                1,
                None,
            )
            .is_ok()
        );

        let mut invalid = node.clone();
        invalid.content_sha256 = "A".repeat(64);
        assert!(checked_node(invalid, None).is_err());
        let mut invalid = node.clone();
        invalid.end_byte = None;
        assert!(checked_node(invalid, None).is_err());
        let mut invalid = node.clone();
        invalid.end_byte = Some(MAX_DOCUMENT_SOURCE_BYTES + 1);
        assert!(checked_node(invalid, None).is_err());
        assert!(
            checked_node_page(
                NodePageV1 {
                    items: vec![node],
                    next: Some("wrong".into()),
                },
                1,
                None,
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn node_source_rejects_mismatched_revision_and_digest() {
        let source = "* Node\n";
        let digest = format!("{:x}", Sha256::digest(source.as_bytes()));
        let revision_error = node_source_response_error(&digest, "\"2\"").await;
        assert!(matches!(
            revision_error,
            HttpClientError::InvalidResponse {
                delivery: DeliveryStatus::Unknown
            }
        ));

        let digest_error = node_source_response_error(&"a".repeat(64), "\"1\"").await;
        assert!(matches!(
            digest_error,
            HttpClientError::InvalidResponse {
                delivery: DeliveryStatus::Unknown
            }
        ));
    }

    async fn node_source_response_error(digest: &str, etag: &str) -> HttpClientError {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let node = serde_json::json!({
            "node_id": "node",
            "document_id": "document",
            "logical_path": "note.org",
            "document_revision": 1,
            "content_sha256": digest,
            "title": "Node",
            "title_path": ["Node"],
            "tags": [],
            "level": 1,
            "start_byte": 0,
            "end_byte": 7,
            "active": true
        })
        .to_string();
        let source = "* Node\n";
        let responses = [
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{node}",
                node.len()
            ),
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/org; charset=utf-8\r\nETag: {etag}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{source}",
                source.len()
            ),
        ];
        let server = tokio::spawn(async move {
            for response in responses {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let token = SecretToken::new(format!("gnosis_{}", "a".repeat(43))).unwrap();
        let client = HttpClient::new(&format!("http://{address}/"), token).unwrap();
        let error = client.node_source("node").await.unwrap_err();
        server.await.unwrap();
        error
    }

    #[tokio::test]
    async fn node_themata_http_pages_and_rejects_unauthorized_or_stale_cursors() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let socket = directory.path().join("gnosis.sock");
        let runtime = ServiceRuntime::start(database.clone(), database.with_extension("blobs"), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let reader = generate_device("reader".into(), vec!["read".into()]).unwrap();
        let reader_token = reader.token.clone();
        service.create_device(reader, 1).await.unwrap();
        let writer = generate_device("writer".into(), vec!["content:write".into()]).unwrap();
        let writer_token = writer.token.clone();
        service.create_device(writer, 2).await.unwrap();
        service
            .replace_note(
                prepare_note(
                    Some("document".into()),
                    "note.org".into(),
                    "#+title: Note\n\n* Heading\n:PROPERTIES:\n:ID: node\n:END:\nBody\n".into(),
                    None,
                )
                .unwrap(),
                3,
                None,
            )
            .await
            .unwrap();
        for index in 0..501 {
            let relation = match index % 3 {
                0 => ThemaConceptRelation::Primary,
                1 => ThemaConceptRelation::Supporting,
                _ => ThemaConceptRelation::Reference,
            };
            service
                .put_thema(
                    test_thema(
                        &format!("thema-{index:04}"),
                        vec![("node".into(), relation)],
                        None,
                    ),
                    20260719,
                    i64::from(index) + 4,
                    None,
                )
                .await
                .unwrap();
        }
        drop(service);
        runtime.finish().await.unwrap();

        let daemon = TestDaemon::start_paths(&socket, &database).await;
        let reader = HttpClient::new(
            &daemon.base_url,
            SecretToken::new(reader_token.clone()).unwrap(),
        )
        .unwrap();
        let node_id = "node";
        let node = reader.node(node_id).await.unwrap();
        assert_eq!(node.document_revision, 1);
        assert_eq!((node.start_byte, node.end_byte), (Some(15), Some(59)));
        let nodes = reader.nodes("", 100, None).await.unwrap();
        assert_eq!(nodes.items.as_slice(), std::slice::from_ref(&node));
        let located = reader.node_source(node_id).await.unwrap();
        assert_eq!(
            located.selected_source(),
            "* Heading\n:PROPERTIES:\n:ID: node\n:END:\nBody\n"
        );
        assert_eq!(located.document().revision(), 1);
        assert_eq!(located.document().content_sha256(), node.content_sha256);
        let first_themata = reader.themata("", 500, None).await.unwrap();
        assert_eq!(first_themata.items.len(), 500);
        assert_eq!(first_themata.items[0].thema_id, "thema-0000");
        let second_themata = reader
            .themata("", 500, first_themata.next.as_deref())
            .await
            .unwrap();
        assert_eq!(second_themata.items.len(), 1);
        assert_eq!(second_themata.items[0].thema_id, "thema-0500");
        let first = reader.node_themata(node_id, 500, None).await.unwrap();
        assert_eq!(first.items.len(), 500);
        assert_eq!(first.items[0].thema_id, "thema-0000");
        assert_eq!(first.items[0].relation, ThemaConceptRelationV1::Primary);
        assert_eq!(first.next.as_deref(), Some("thema-0499"));
        let second = reader
            .node_themata(node_id, 500, first.next.as_deref())
            .await
            .unwrap();
        assert_eq!(second.items.len(), 1);
        assert_eq!(second.items[0].thema_id, "thema-0500");
        assert!(matches!(
            reader.node_themata(node_id, 501, None).await,
            Err(HttpClientError::InvalidMutation)
        ));
        assert!(matches!(
            reader.node_themata(node_id, 500, Some("missing")).await,
            Err(HttpClientError::Server(error)) if error.status == 400
        ));
        let writer =
            HttpClient::new(&daemon.base_url, SecretToken::new(writer_token).unwrap()).unwrap();
        assert!(matches!(
            writer.node_themata(node_id, 500, None).await,
            Err(HttpClientError::Server(error)) if error.status == 403
        ));
        daemon.stop().await;

        let runtime = ServiceRuntime::start(database.clone(), database.with_extension("blobs"), 8)
            .await
            .unwrap();
        runtime
            .handle()
            .put_thema(
                test_thema("thema-0000", vec![], Some(1)),
                20260719,
                506,
                None,
            )
            .await
            .unwrap();
        runtime.finish().await.unwrap();
        let daemon = TestDaemon::start_paths(&socket, &database).await;
        let reader =
            HttpClient::new(&daemon.base_url, SecretToken::new(reader_token).unwrap()).unwrap();
        assert!(matches!(
            reader.node_themata(node_id, 500, Some("thema-0000")).await,
            Err(HttpClientError::Server(error)) if error.status == 400
        ));
        daemon.stop().await;
    }

    #[tokio::test]
    async fn node_source_uses_the_latest_document_that_retired_a_reused_node() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let socket = directory.path().join("gnosis.sock");
        let runtime = ServiceRuntime::start(database.clone(), database.with_extension("blobs"), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let reader = generate_device("reader".into(), vec!["read".into()]).unwrap();
        let token = reader.token.clone();
        service.create_device(reader, 1).await.unwrap();
        let original_a =
            "#+title: First\n\n* Historical\n:PROPERTIES:\n:ID: historical\n:END:\nFirst body\n";
        service
            .replace_note(
                prepare_note(
                    Some("document-a".into()),
                    "a.org".into(),
                    original_a.into(),
                    None,
                )
                .unwrap(),
                2,
                None,
            )
            .await
            .unwrap();
        service
            .replace_note(
                prepare_note(
                    Some("document-a".into()),
                    "a.org".into(),
                    "#+title: First current\n".into(),
                    Some(1),
                )
                .unwrap(),
                3,
                None,
            )
            .await
            .unwrap();
        let original_b =
            "#+title: Second\n\n* Historical\n:PROPERTIES:\n:ID: historical\n:END:\nSecond body\n";
        service
            .replace_note(
                prepare_note(
                    Some("document-b".into()),
                    "b.org".into(),
                    original_b.into(),
                    None,
                )
                .unwrap(),
                4,
                None,
            )
            .await
            .unwrap();
        service
            .replace_note(
                prepare_note(
                    Some("document-b".into()),
                    "b.org".into(),
                    "#+title: Second current\n".into(),
                    Some(1),
                )
                .unwrap(),
                5,
                None,
            )
            .await
            .unwrap();
        drop(service);
        runtime.finish().await.unwrap();

        let daemon = TestDaemon::start_paths(&socket, &database).await;
        let client = HttpClient::new(&daemon.base_url, SecretToken::new(token).unwrap()).unwrap();
        let node_id = "historical";
        let node = client.node(node_id).await.unwrap();
        assert!(!node.active);
        assert_eq!(node.document_id, "document-b");
        assert_eq!(node.document_revision, 1);
        let located = client.node_source(node_id).await.unwrap();
        assert_eq!(
            located.selected_source(),
            "* Historical\n:PROPERTIES:\n:ID: historical\n:END:\nSecond body\n"
        );
        assert_eq!(located.document().source(), original_b);
        daemon.stop().await;
    }

    fn test_thema(
        id: &str,
        concepts: Vec<(String, ThemaConceptRelation)>,
        expected_revision: Option<u64>,
    ) -> crate::application::PreparedThema {
        prepare_thema(
            Some(id.into()),
            thema_content(
                "basic",
                "Question".into(),
                vec![None],
                vec!["answer".into()],
                None,
                None,
                None,
            )
            .unwrap(),
            vec![],
            concepts,
            vec![],
            expected_revision,
        )
        .unwrap()
    }

    #[test]
    fn response_validators_reject_semantically_invalid_content() {
        let digest = "a".repeat(64);
        let expectation = DocumentMutationExpectation {
            document_id: Some("document".into()),
            active: true,
            logical_path: Some("note.org".into()),
            content_type: Some("text/org; charset=utf-8".into()),
            content_sha256: Some(digest.clone()),
            prior_revision: None,
        };
        let mut head = crate::protocol::http_v1::DocumentHeadV1 {
            document_id: "document".into(),
            logical_path: "note.org".into(),
            content_type: "text/org; charset=utf-8".into(),
            revision: 1,
            content_sha256: Some(digest),
            active: true,
        };
        assert!(validate_document_head(&head, &expectation, true));
        let mutation = DocumentMutationResultV1::Applied {
            format_version: 1,
            operation_id: "operation".into(),
            authoritative: head.clone(),
            cursor: "cursor".into(),
            changed: true,
        };
        assert!(validate_document_mutation_result(
            &mutation,
            "operation",
            &expectation
        ));
        assert!(!validate_document_head(&head, &expectation, false));
        let update_expectation = DocumentMutationExpectation {
            prior_revision: Some(1),
            ..expectation
        };
        assert!(validate_document_head(&head, &update_expectation, false));
        head.revision = 2;
        assert!(validate_document_head(&head, &update_expectation, true));
        head.logical_path = "../note.org".into();
        assert!(!validate_document_head(&head, &update_expectation, true));
        head.logical_path = "note.org".into();
        head.content_type = "text/markdown; charset=utf-8".into();
        assert!(!validate_document_head(&head, &update_expectation, true));
        let lifecycle_expectation = DocumentMutationExpectation {
            content_type: None,
            content_sha256: None,
            ..update_expectation.clone()
        };
        assert!(!validate_document_head(&head, &lifecycle_expectation, true));
        head.content_type = "text/org; charset=utf-8".into();
        head.revision = 1;
        head.content_sha256 = Some("b".repeat(64));
        assert!(!validate_document_head(&head, &update_expectation, false));
        head.content_sha256 = update_expectation.content_sha256.clone();
        head.active = false;
        assert!(!validate_document_head(&head, &update_expectation, false));

        let content = thema_content(
            "basic",
            "Question".into(),
            vec![],
            vec!["Answer".into()],
            None,
            None,
            None,
        )
        .unwrap();
        let prepared = prepare_thema(
            Some("thema".into()),
            content.clone(),
            vec![],
            vec![],
            vec![],
            None,
        )
        .unwrap();
        let mut thema = ThemaV1 {
            thema_id: "thema".into(),
            revision: 1,
            kind: ThemaKindV1::Basic,
            keimenon: "Question".into(),
            hypothesis: vec![],
            answer: vec!["Answer".into()],
            source_guid: None,
            parathema: None,
            review_image: None,
            tags: vec![],
            concepts: vec![],
            asset_paths: vec![],
            content_sha256: prepared.content_sha256,
            archived: false,
        };
        assert!(validate_thema(&thema, "thema"));
        thema.revision = 0;
        assert!(!validate_thema(&thema, "thema"));
        thema.revision = 1;
        thema.asset_paths.push("../asset".into());
        assert!(!validate_thema(&thema, "thema"));
        thema.asset_paths.clear();
        let duplicate_tags = prepare_thema(
            Some("thema".into()),
            content,
            vec!["tag".into(), "tag".into()],
            vec![],
            vec![],
            None,
        )
        .unwrap();
        thema.tags = vec!["tag".into(), "tag".into()];
        thema.content_sha256 = duplicate_tags.content_sha256;
        assert!(!validate_thema(&thema, "thema"));
    }

    #[test]
    fn server_errors_require_complete_shapes_and_matching_statuses() {
        let conflict =
            br#"{"error":{"code":"idempotency_conflict","message":"conflict","meta":null}}"#;
        assert!(matches!(
            server_error::<()>(StatusCode::CONFLICT, conflict).unwrap_err(),
            HttpClientError::Server(_)
        ));
        assert!(matches!(
            server_error::<()>(StatusCode::INTERNAL_SERVER_ERROR, conflict).unwrap_err(),
            HttpClientError::InvalidResponse { .. }
        ));

        for incomplete in [
            br#"{"error":{"code":"internal","message":"failure"}}"#.as_slice(),
            br#"{"error":{"code":"revision_conflict","message":"conflict","meta":{"expected_revision":1}}}"#.as_slice(),
            br#"{"error":{"code":"snapshot_required","message":"snapshot","meta":{"snapshot_url":"/wrong"}}}"#.as_slice(),
            br#"{"error":{"code":"blob_required","message":"blob","meta":{"content_sha256":"not-a-digest","upload_url":"/v1/sync/blobs/not-a-digest"}}}"#.as_slice(),
            br#"{"error":{"code":"blob_required","message":"blob","meta":{"content_sha256":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","upload_url":"/wrong"}}}"#.as_slice(),
        ] {
            assert!(matches!(
                server_error::<()>(StatusCode::INTERNAL_SERVER_ERROR, incomplete).unwrap_err(),
                HttpClientError::InvalidResponse { .. }
            ));
        }
    }

    #[test]
    fn desired_state_validators_bind_kind_identity_state_and_revision() {
        let digest = "a".repeat(64);
        let asset_head = AssetHeadV1 {
            logical_path: format!("assets/{digest}/a.png"),
            revision: 1,
            content_sha256: Some(digest.clone()),
            media_type: Some("image/png".into()),
            active: true,
        };
        let asset = DesiredStateMutationProjectionV1::Asset(asset_head.clone());
        let asset_expectation = DesiredMutationExpectation::Asset {
            logical_path: format!("assets/{digest}/a.png"),
            digest: Some(digest),
            media_type: Some("image/png".into()),
            active: true,
            expected_revision: None,
        };
        assert!(valid_desired_projection(&asset, true, &asset_expectation));
        assert!(!valid_desired_projection(&asset, false, &asset_expectation));
        for malformed in [
            AssetHeadV1 {
                logical_path: "assets/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/other.png".into(),
                ..asset_head.clone()
            },
            AssetHeadV1 {
                revision: 2,
                ..asset_head.clone()
            },
            AssetHeadV1 {
                content_sha256: Some("b".repeat(64)),
                ..asset_head.clone()
            },
            AssetHeadV1 {
                media_type: Some("image/jpeg".into()),
                ..asset_head.clone()
            },
            AssetHeadV1 {
                active: false,
                ..asset_head.clone()
            },
        ] {
            assert!(!valid_desired_projection(
                &DesiredStateMutationProjectionV1::Asset(malformed),
                true,
                &asset_expectation
            ));
        }

        let thema_input = ThemaWriteRequestV1 {
            kind: "basic".into(),
            keimenon: "Question".into(),
            hypothesis: vec![],
            answer: vec!["Answer".into()],
            source_guid: None,
            parathema: None,
            review_image: None,
            tags: vec!["zeta".into(), "alpha".into(), "zeta".into()],
            concepts: vec![],
            asset_paths: vec![],
        };
        let prepared = prepare_thema(
            Some("thema".into()),
            thema_content(
                "basic",
                "Question".into(),
                vec![],
                vec!["Answer".into()],
                None,
                None,
                None,
            )
            .unwrap(),
            thema_input.tags.clone(),
            vec![],
            vec![],
            None,
        )
        .unwrap();
        let mut thema: DesiredStateMutationProjectionV1 =
            serde_json::from_value(serde_json::json!({
                "kind": "thema",
                "value": {
                    "thema_id": "thema",
                    "revision": 2,
                    "kind": "basic",
                    "keimenon": "Question",
                    "hypothesis": [],
                    "answer": ["Answer"],
                    "source_guid": null,
                    "parathema": null,
                    "review_image": null,
                    "tags": ["alpha", "zeta"],
                    "concepts": [],
                    "asset_paths": [],
                    "content_sha256": prepared.content_sha256,
                    "archived": false
                }
            }))
            .unwrap();
        let thema_expectation = DesiredMutationExpectation::Thema {
            thema_id: Some("thema".into()),
            input: Some(thema_input.clone()),
            archived: None,
            expected_revision: Some(1),
        };
        assert!(valid_desired_projection(&thema, true, &thema_expectation));
        assert!(!valid_desired_projection(&asset, true, &thema_expectation));
        let valid_digest = match &mut thema {
            DesiredStateMutationProjectionV1::Thema(thema) => {
                std::mem::replace(&mut thema.content_sha256, "b".repeat(64))
            }
            _ => unreachable!(),
        };
        assert!(!valid_desired_projection(&thema, true, &thema_expectation));
        let archive_expectation = DesiredMutationExpectation::Thema {
            thema_id: Some("thema".into()),
            input: None,
            archived: Some(false),
            expected_revision: Some(1),
        };
        assert!(!valid_desired_projection(
            &thema,
            true,
            &archive_expectation
        ));
        let DesiredStateMutationProjectionV1::Thema(thema_value) = &mut thema else {
            unreachable!();
        };
        thema_value.content_sha256 = valid_digest;
        thema_value.thema_id = "x".repeat(129);
        thema_value.revision = 1;
        let create_expectation = DesiredMutationExpectation::Thema {
            thema_id: None,
            input: Some(thema_input),
            archived: None,
            expected_revision: None,
        };
        assert!(!valid_desired_projection(&thema, true, &create_expectation));

        let settings_input = UpdateSettingsRequestV1 {
            timezone: "UTC".into(),
            day_boundary_hour: 4,
            desired_retention: 0.9,
        };
        let scheduler = settings_input.clone().into_application().scheduler();
        let mut settings = DesiredStateMutationProjectionV1::Settings(SettingsV1 {
            revision: 2,
            timezone: "UTC".into(),
            day_boundary_hour: 4,
            scheduler: FsrsConfigV1::from(&scheduler),
        });
        let settings_expectation = DesiredMutationExpectation::Settings {
            input: settings_input,
            expected_revision: 1,
        };
        assert!(valid_desired_rejection(
            &DocumentMutationRejectionV1::RevisionConflict {
                expected_revision: Some(1),
                current_revision: Some(2),
            },
            &settings_expectation,
        ));
        assert!(!valid_desired_rejection(
            &DocumentMutationRejectionV1::RevisionConflict {
                expected_revision: Some(999),
                current_revision: Some(2),
            },
            &settings_expectation,
        ));
        assert!(valid_desired_projection(
            &settings,
            true,
            &settings_expectation
        ));
        let DesiredStateMutationProjectionV1::Settings(settings_value) = &mut settings else {
            unreachable!();
        };
        settings_value.scheduler.parameters[0] += 1.0;
        assert!(!valid_desired_projection(
            &settings,
            true,
            &settings_expectation
        ));
    }

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
    async fn oversized_document_import_is_not_sent() {
        let token = SecretToken::new(format!("gnosis_{}", "a".repeat(43))).unwrap();
        let client = HttpClient::new("http://127.0.0.1:1/", token).unwrap();
        let input = DocumentImportRequestV1 {
            content_type: "text/org; charset=utf-8".into(),
            source: "x".repeat(MAX_DOCUMENT_SOURCE_BYTES + 1),
            expected_revision: None,
        };

        let error = client
            .import_document("document", "operation", &input)
            .await
            .unwrap_err();
        assert!(matches!(error, HttpClientError::InvalidMutation));
        assert_eq!(error.delivery(), DeliveryStatus::NotSent);
    }

    #[tokio::test]
    async fn invalid_import_id_and_due_query_are_not_sent() {
        let token = SecretToken::new(format!("gnosis_{}", "a".repeat(43))).unwrap();
        let client = HttpClient::new("http://127.0.0.1:1/", token).unwrap();
        let input = DocumentImportRequestV1 {
            content_type: "text/org; charset=utf-8".into(),
            source: "* Note".into(),
            expected_revision: None,
        };

        let import_error = client
            .import_document(&"x".repeat(129), "operation", &input)
            .await
            .unwrap_err();
        assert!(matches!(import_error, HttpClientError::InvalidMutation));
        assert_eq!(import_error.delivery(), DeliveryStatus::NotSent);

        for result in [
            client.due_page(0, 1, None).await,
            client.due_page(101, 1, None).await,
            client.due_page(20_260_718, 0, None).await,
            client.due_page(20_260_718, 1, Some("")).await,
            client
                .due_page(20_260_718, 1, Some(&"c".repeat(MAX_DUE_CURSOR_BYTES + 1)))
                .await,
        ] {
            let error = result.unwrap_err();
            assert!(matches!(error, HttpClientError::InvalidMutation));
            assert_eq!(error.delivery(), DeliveryStatus::NotSent);
        }

        assert!(matches!(
            client
                .due_page(20_260_718, 1, Some(&"c".repeat(MAX_DUE_CURSOR_BYTES)),)
                .await,
            Err(HttpClientError::Transport { .. })
        ));
    }

    #[tokio::test]
    async fn invalid_document_mutations_are_not_sent() {
        let token = SecretToken::new(format!("gnosis_{}", "a".repeat(43))).unwrap();
        let client = HttpClient::new("http://127.0.0.1:1/", token).unwrap();
        let note = NoteWriteRequestV1 {
            logical_path: "note.org".into(),
            content_type: "text/org; charset=utf-8".into(),
            source: "* Note".into(),
        };
        let rename = NoteRenameRequestV1 {
            logical_path: "renamed.org".into(),
        };
        let invalid_id = "x".repeat(129);
        let unsupported = NoteWriteRequestV1 {
            content_type: "text/markdown; charset=utf-8".into(),
            ..note.clone()
        };
        for result in [
            client.create_note("operation", &unsupported).await,
            client
                .replace_note(&invalid_id, "operation", 1, &note)
                .await,
            client
                .rename_note(&invalid_id, "operation", 1, "text/org", &rename)
                .await,
            client
                .tombstone_note(&invalid_id, "operation", 1, "text/org")
                .await,
            client
                .replace_note("document", "operation", u64::MAX, &note)
                .await,
            client
                .rename_note("document", "operation", u64::MAX, "text/org", &rename)
                .await,
            client
                .tombstone_note("document", "operation", u64::MAX, "text/org")
                .await,
            client
                .import_document(
                    "document",
                    "operation",
                    &DocumentImportRequestV1 {
                        content_type: "text/org; charset=utf-8".into(),
                        source: "* Note".into(),
                        expected_revision: Some(u64::MAX),
                    },
                )
                .await,
        ] {
            let error = result.unwrap_err();
            assert!(matches!(error, HttpClientError::InvalidMutation));
            assert_eq!(error.delivery(), DeliveryStatus::NotSent);
        }
    }

    #[tokio::test]
    async fn archive_rejects_noncanonical_authoritative_thema_with_unknown_delivery() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let body = serde_json::json!({
            "status": "applied",
            "format_version": 1,
            "operation_id": "operation",
            "authoritative": {
                "kind": "thema",
                "value": {
                    "thema_id": "thema",
                    "revision": 2,
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
                    "content_sha256": "b".repeat(64),
                    "archived": true
                }
            },
            "cursor": "cursor",
            "changed": true
        })
        .to_string();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let token = SecretToken::new(format!("gnosis_{}", "a".repeat(43))).unwrap();
        let base_url = format!("http://{address}/");
        let client = HttpClient::new(&base_url, token).unwrap();

        let error = client
            .set_thema_archived("thema", "operation", 1, true)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            HttpClientError::InvalidResponse {
                delivery: DeliveryStatus::Unknown
            }
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn settings_rejects_noncanonical_scheduler_with_unknown_delivery() {
        let input = UpdateSettingsRequestV1 {
            timezone: "UTC".into(),
            day_boundary_hour: 4,
            desired_retention: 0.9,
        };
        let scheduler = input.clone().into_application().scheduler();
        let mut settings = SettingsV1 {
            revision: 2,
            timezone: "UTC".into(),
            day_boundary_hour: 4,
            scheduler: FsrsConfigV1::from(&scheduler),
        };
        settings.scheduler.parameters[0] += 1.0;
        let body = serde_json::json!({
            "status": "applied",
            "format_version": 1,
            "operation_id": "operation",
            "authoritative": DesiredStateMutationProjectionV1::Settings(settings),
            "cursor": "cursor",
            "changed": true
        })
        .to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let token = SecretToken::new(format!("gnosis_{}", "a".repeat(43))).unwrap();
        let base_url = format!("http://{address}/");
        let client = HttpClient::new(&base_url, token).unwrap();

        let error = client
            .update_settings("operation", 1, &input)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            HttpClientError::InvalidResponse {
                delivery: DeliveryStatus::Unknown
            }
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn rejected_settings_mutation_requires_matching_request_digest() {
        let input = UpdateSettingsRequestV1 {
            timezone: "UTC".into(),
            day_boundary_hour: 4,
            desired_retention: 0.9,
        };
        let body = serde_json::json!({
            "status": "rejected",
            "format_version": 1,
            "operation_id": "operation",
            "request_sha256": "b".repeat(64),
            "rejection": {
                "reason": "revision_conflict",
                "expected_revision": 1,
                "current_revision": 2
            },
            "cursor": "cursor"
        })
        .to_string();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 1024];
            assert!(stream.read(&mut request).await.unwrap() > 0);
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let token = SecretToken::new(format!("gnosis_{}", "a".repeat(43))).unwrap();
        let client = HttpClient::new(&format!("http://{address}/"), token).unwrap();

        let error = client
            .update_settings("operation", 1, &input)
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            HttpClientError::InvalidResponse {
                delivery: DeliveryStatus::Unknown
            }
        ));
        server.await.unwrap();
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

    #[tokio::test]
    async fn client_review_views_use_typed_contracts() {
        let fixture = prepare_review_fixture().await;
        let daemon = TestDaemon::start(&fixture).await;
        let token = SecretToken::new(fixture.token.clone()).unwrap();
        let client = HttpClient::new(&daemon.base_url, token).unwrap();

        let dashboard = client.dashboard(Some(7)).await.unwrap();
        assert_eq!(dashboard.window.days, 7);
        assert_eq!(dashboard.content.active_themata, 1);
        let sessions = client
            .review_sessions(&ReviewSessionListRequestV1 {
                mode: Some(ReviewSessionModeV1::Offline),
                ..ReviewSessionListRequestV1::default()
            })
            .await
            .unwrap();
        assert_eq!(sessions.items.len(), 1);
        let session_id = &sessions.items[0].session.session_id;
        assert_eq!(
            client
                .review_session(session_id)
                .await
                .unwrap()
                .review_count,
            0
        );
        assert!(
            client
                .review_history(&ReviewHistoryRequestV1::default())
                .await
                .unwrap()
                .items
                .is_empty()
        );
        let queue = client
            .review_queue_summary(&ReviewQueueSummaryRequestV1 {
                source: ReviewSourceInputV1 {
                    kind: ReviewSourceKindV1::Manual,
                    tags: vec![],
                    exclude_tags: vec![],
                    topic_node_id: None,
                    topic_forward_depth: None,
                    topic_backward_depth: None,
                    manual_ids: vec!["thema".into()],
                },
            })
            .await
            .unwrap();
        assert_eq!(queue.matched_count, 1);
        assert_eq!(
            client
                .thema_review_state("thema")
                .await
                .unwrap()
                .state
                .review_count,
            0
        );
        assert!(matches!(
            client.dashboard(Some(0)).await,
            Err(HttpClientError::InvalidReview)
        ));

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
        pack: ReviewPackEnvelopeV2,
    }

    async fn prepare_review_fixture() -> ReviewFixture {
        const ISSUED_AT_US: u64 = 1_784_106_000_000_000;
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let runtime = ServiceRuntime::start(database.clone(), database.with_extension("blobs"), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let device =
            generate_device("review-client".into(), vec!["read".into(), "review".into()]).unwrap();
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
            pack: review_pack_response(&pack).unwrap(),
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
        }
    }

    struct TestDaemon {
        base_url: String,
        shutdown: oneshot::Sender<()>,
        task: tokio::task::JoinHandle<Result<(), crate::protocol::ServerError>>,
    }

    impl TestDaemon {
        async fn start(fixture: &ReviewFixture) -> Self {
            Self::start_paths(&fixture.socket, &fixture.database).await
        }

        async fn start_paths(socket: &std::path::Path, database: &std::path::Path) -> Self {
            let mut config = DaemonConfig::new(socket, database);
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
