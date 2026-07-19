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
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue},
    redirect::Policy,
};
use serde::{Serialize, de::DeserializeOwned};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    application::{
        DEFAULT_ONLINE_REVIEW_TARGET_COUNT, MAX_DUE_CURSOR_BYTES, MAX_OFFLINE_ASSET_BYTES,
        MAX_ONLINE_REVIEW_TARGET_COUNT, canonical_asset_path, prepare_thema, thema_content,
        valid_operation_id, validate_org_path,
    },
    client_replica::{
        DeviceUploadKind, DeviceUploadResult, PendingAssetUpload, PendingDeviceUpload,
    },
    domain::{
        DocumentId, DocumentProjection, DocumentProjectionInput, LinkOccurrence, Node, NodeId,
        SourceSpan, TitlePath,
    },
    is_canonical_sha256,
    protocol::{
        DocumentView, DuePageView, MAX_FRAME_SIZE, MAX_OFFLINE_ASSET_OPERATION_BYTES,
        MAX_OFFLINE_ASSET_RESULT_BYTES, MAX_OFFLINE_CONTENT_OPERATION_BYTES,
        MAX_OFFLINE_CONTENT_RESULT_BYTES, MAX_OFFLINE_REVIEW_OPERATION_BYTES,
        MAX_OFFLINE_REVIEW_RESULT_BYTES, MAX_REPLICA_CURSOR_BYTES, MAX_REPLICA_PAGE_BYTES,
        decode_review_pack,
        http_v1::{
            ActiveReviewPackEnvelopeV2, ActiveReviewSessionV1, ApiErrorCodeV1, ApiErrorResponseV1,
            AssetCapabilitiesV1, ContentCapabilitiesV1, DashboardV1,
            DesiredStateMutationProjectionV1, DesiredStateMutationResultV1, DeviceIdentityV1,
            DocumentImportRequestV1, DocumentMutationResultV1, NoteRenameRequestV1,
            NoteWriteRequestV1, ReplicaChangePageV1, ReplicaSnapshotPageV1, ReplicaStateV1,
            ReviewAnswerV1, ReviewAttemptOperationResponseV1, ReviewAttemptStateResponseV1,
            ReviewAttemptV1, ReviewClozeCheckRequestV1, ReviewClozeCheckV1, ReviewCommitResponseV1,
            ReviewHistoryPageV1, ReviewHistoryRequestV1, ReviewPackAssetV2, ReviewPackEnvelopeV2,
            ReviewPackStatusV2, ReviewQueueSummaryRequestV1, ReviewQueueSummaryV1,
            ReviewSessionListRequestV1, ReviewSessionModeV1, ReviewSessionPageV1,
            ReviewSessionRequestV1, ReviewSessionStatusV1, ReviewSessionSummaryV1, ReviewSessionV1,
            SetAcceptedSuccessRequestV1, SetSuspensionRequestV1, StagedBlobV1, ThemaKindV1,
            ThemaReviewStateV1, ThemaV1, ThemaWriteRequestV1, UpdateSettingsRequestV1,
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
                content_sha256: Some(format!("{:x}", Sha256::digest(input.org.as_bytes()))),
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
                content_sha256: Some(format!("{:x}", Sha256::digest(input.org.as_bytes()))),
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
        if format!("{:x}", Sha256::digest(bytes)) != digest {
            return Err(HttpClientError::InvalidUpload);
        }
        let media_type =
            HeaderValue::from_str(media_type).map_err(|_| HttpClientError::InvalidUpload)?;
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
            DesiredMutationExpectation::Asset {
                logical_path: format!("assets/{digest}/{name}"),
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
        let request = self
            .client
            .delete(self.asset_endpoint(digest, name)?)
            .header("Idempotency-Key", operation_id)
            .header("If-Match", expected_revision);
        execute_desired_state_mutation(
            request,
            operation_id,
            DesiredMutationExpectation::Asset {
                logical_path: format!("assets/{digest}/{name}"),
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
        self.send_desired_state_json(
            self.endpoint("v1/themata")?,
            reqwest::Method::POST,
            operation_id,
            None,
            input,
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
        self.send_desired_state_json(
            self.thema_endpoint(thema_id, None)?,
            reqwest::Method::PUT,
            operation_id,
            Some(expected_revision),
            input,
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
        let suffix = if archived { "archive" } else { "unarchive" };
        let request = self
            .client
            .post(self.thema_endpoint(thema_id, Some(suffix))?)
            .header("Idempotency-Key", operation_id)
            .header("If-Match", expected_revision);
        execute_desired_state_mutation(
            request,
            operation_id,
            DesiredMutationExpectation::Thema {
                thema_id: Some(thema_id.to_owned()),
                input: None,
                archived: Some(archived),
                expected_revision: Some(expected_revision),
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
        self.send_desired_state_json(
            self.endpoint("v1/settings")?,
            reqwest::Method::PUT,
            operation_id,
            Some(expected_revision),
            input,
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

    pub async fn import_document(
        &self,
        document_id: &str,
        operation_id: &str,
        input: &DocumentImportRequestV1,
    ) -> Result<DocumentMutationResultV1, HttpClientError> {
        validate_document_mutation_request(document_id, input.expected_revision)?;
        require_operation_id(operation_id)?;
        let url = self.resource_endpoint("v1/documents", document_id)?;
        let body = serde_json::to_vec(input).map_err(|_| HttpClientError::InvalidMutation)?;
        if body.len() > MAX_FRAME_SIZE {
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
                content_sha256: Some(format!("{:x}", Sha256::digest(input.org.as_bytes()))),
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
    ) -> Result<ReviewAttemptV1, HttpClientError> {
        require_review_id(session_id)?;
        let url = self.review_resource_endpoint("sessions", session_id, Some("attempts"))?;
        let attempt = execute_json_exact(
            self.client.post(url),
            MAX_OFFLINE_REVIEW_RESULT_BYTES,
            StatusCode::CREATED,
        )
        .await?;
        checked_attempt(attempt, None, Some(session_id))
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
        url: Url,
        method: reqwest::Method,
        operation_id: &str,
        expected_revision: Option<u64>,
        input: &impl Serialize,
        expectation: DesiredMutationExpectation,
    ) -> Result<DesiredStateMutationResultV1, HttpClientError> {
        require_operation_id(operation_id)?;
        let mut request = self
            .client
            .request(method, url)
            .header("Idempotency-Key", operation_id)
            .header(CONTENT_TYPE, HeaderValue::from_static("application/json"))
            .body(encode_document_mutation(input)?);
        if let Some(revision) = expected_revision {
            if revision == 0 {
                return Err(HttpClientError::InvalidMutation);
            }
            request = request.header("If-Match", revision);
        }
        execute_desired_state_mutation(request, operation_id, expectation).await
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

fn validate_document_view(
    document: &DocumentView,
    document_id: &str,
) -> Result<(), HttpClientError> {
    if document.document_id != document_id
        || document.revision == 0
        || DocumentId::new(document.document_id.clone()).is_err()
        || !canonical_hex(&document.content_sha256, 64)
        || !canonical_hex(&document.compatibility_sha1, 40)
        || !all_unique(&document.file_tags)
        || document.nodes.iter().any(|node| !all_unique(&node.tags))
    {
        return Err(invalid_response());
    }
    let nodes = document
        .nodes
        .iter()
        .map(|node| {
            Node::new(
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
            )
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
        compatibility_sha1: document.compatibility_sha1.clone(),
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
    require_review_id(&attempt.attempt_id).is_ok()
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
    Settings {
        input: UpdateSettingsRequestV1,
        expected_revision: u64,
    },
}

async fn execute_desired_state_mutation(
    request: RequestBuilder,
    operation_id: &str,
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
            cursor,
            ..
        } => *format_version == 1 && actual == operation_id && valid_result_cursor(cursor),
    };
    valid.then_some(result).ok_or_else(invalid_response)
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
            thema_id
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
            settings.timezone == input.timezone
                && settings.day_boundary_hour == input.day_boundary_hour
                && settings.scheduler.desired_retention == input.desired_retention
                && valid_mutation_revision(settings.revision, Some(*expected_revision), changed)
        }
        _ => false,
    }
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
    if !validate_thema(thema, &thema.thema_id) {
        return false;
    }
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

struct DocumentMutationExpectation {
    document_id: Option<String>,
    active: bool,
    logical_path: Option<String>,
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
        && validate_org_path(&head.logical_path).is_ok()
        && expectation
            .logical_path
            .as_deref()
            .is_none_or(|expected| head.logical_path == expected)
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
    use tokio::{io::AsyncReadExt as _, net::TcpListener, sync::oneshot, time::timeout};

    use super::*;
    use crate::{
        application::{
            ReviewSource, ReviewSourceKind, generate_device, prepare_thema, thema_content,
        },
        client_replica::{LocalReplica, OfflineAnswer, OfflineResponse},
        protocol::{
            DaemonConfig, DaemonServer,
            http_v1::{ReviewSourceInputV1, ReviewSourceKindV1},
            offline_asset_capabilities, offline_content_capabilities, replica_snapshot_page,
            replica_state_response, review_pack_response,
        },
        service::ServiceRuntime,
    };

    #[test]
    fn response_validators_reject_semantically_invalid_content() {
        let digest = "a".repeat(64);
        let expectation = DocumentMutationExpectation {
            document_id: Some("document".into()),
            active: true,
            logical_path: Some("note.org".into()),
            content_sha256: Some(digest.clone()),
            prior_revision: None,
        };
        let mut head = crate::protocol::http_v1::DocumentHeadV1 {
            document_id: "document".into(),
            logical_path: "note.org".into(),
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
        let asset =
            DesiredStateMutationProjectionV1::Asset(crate::protocol::http_v1::AssetHeadV1 {
                logical_path: format!("assets/{digest}/a.png"),
                revision: 1,
                content_sha256: Some(digest.clone()),
                media_type: Some("image/png".into()),
                active: true,
            });
        let asset_expectation = DesiredMutationExpectation::Asset {
            logical_path: format!("assets/{digest}/a.png"),
            digest: Some(digest),
            media_type: Some("image/png".into()),
            active: true,
            expected_revision: None,
        };
        assert!(valid_desired_projection(&asset, true, &asset_expectation));
        assert!(!valid_desired_projection(&asset, false, &asset_expectation));

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
        let DesiredStateMutationProjectionV1::Thema(thema_value) = &mut thema else {
            unreachable!();
        };
        thema_value.content_sha256 = valid_digest;
        thema_value.thema_id = "../thema".into();
        let create_expectation = DesiredMutationExpectation::Thema {
            thema_id: None,
            input: Some(thema_input),
            archived: None,
            expected_revision: None,
        };
        assert!(!valid_desired_projection(&thema, true, &create_expectation));

        let settings: DesiredStateMutationProjectionV1 =
            serde_json::from_value(serde_json::json!({
                "kind": "settings",
                "value": {
                    "revision": 2,
                    "timezone": "UTC",
                    "day_boundary_hour": 4,
                    "scheduler": {
                        "algorithm": "fsrs",
                        "model": "fsrs6",
                        "implementation": "fsrs-6.6.1",
                        "desired_retention": 0.9,
                        "parameters": vec![0.0; 21]
                    }
                }
            }))
            .unwrap();
        let settings_expectation = DesiredMutationExpectation::Settings {
            input: UpdateSettingsRequestV1 {
                timezone: "UTC".into(),
                day_boundary_hour: 4,
                desired_retention: 0.9,
            },
            expected_revision: 1,
        };
        assert!(valid_desired_projection(
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
            org: "\\".repeat(MAX_FRAME_SIZE / 2 + 1),
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
            org: "* Note".into(),
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
            org: "* Note".into(),
        };
        let rename = NoteRenameRequestV1 {
            logical_path: "renamed.org".into(),
        };
        let invalid_id = "x".repeat(129);
        for result in [
            client
                .replace_note(&invalid_id, "operation", 1, &note)
                .await,
            client
                .rename_note(&invalid_id, "operation", 1, &rename)
                .await,
            client.tombstone_note(&invalid_id, "operation", 1).await,
            client
                .replace_note("document", "operation", u64::MAX, &note)
                .await,
            client
                .rename_note("document", "operation", u64::MAX, &rename)
                .await,
            client
                .tombstone_note("document", "operation", u64::MAX)
                .await,
            client
                .import_document(
                    "document",
                    "operation",
                    &DocumentImportRequestV1 {
                        org: "* Note".into(),
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
