use std::{
    convert::Infallible,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    BoxError, Json, Router,
    body::Bytes,
    error_handling::HandleErrorLayer,
    extract::{
        DefaultBodyLimit, Extension, FromRequest, Path, Query, Request, State,
        rejection::{BytesRejection, JsonRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::{get, post},
};
use schemars::generate::SchemaSettings;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_stream::{StreamExt as _, wrappers::BroadcastStream};
use tower::{ServiceBuilder, limit::ConcurrencyLimitLayer, timeout::TimeoutLayer};

use crate::{
    application::{
        AnswerInput, ApplicationError, DevicePrincipal, DeviceScope, MAX_OFFLINE_ASSET_BYTES,
        ReviewAttemptMutation, ReviewAttemptOperation, ReviewAttemptOperationResult,
        ReviewAttemptOperationStatus, ReviewAttemptView, ReviewOperationAuthority,
        ReviewSessionView, ReviewSource, SettingsRecord, canonical_asset_path, prepare_asset,
        prepare_note, prepare_thema, review_day_at, thema_content,
    },
    domain::DocumentId,
    is_canonical_sha256,
    protocol::{
        MAX_OFFLINE_REVIEW_OPERATION_BYTES, OfflineAssetSubmission, OfflineAssetWireError,
        OfflineContentSubmission, OfflineContentWireError, OfflineReviewSubmission,
        OfflineReviewWireError, ReplicaWireError, active_review_pack_json, decode_cursor,
        decode_snapshot_cursor,
        http_v1::{
            ApiErrorBodyV1, ApiErrorCodeV1, ApiErrorMetaV1, ApiErrorResponseV1,
            AssetCapabilitiesV1, AssetHeadV1, ContentCapabilitiesV1, ContentChangePageV1,
            DocumentHeadV1, DocumentRevisionPageV1, DocumentRevisionSummaryV1, NodePageV1, NodeV1,
            NoteRenameRequestV1, NoteWriteRequestV1, ReplicaChangePageV1, ReplicaSnapshotPageV1,
            ReplicaStateV1, ReviewAttemptOperationResponseV1, ReviewAttemptStateResponseV1,
            SetAcceptedSuccessRequestV1, SetSuspensionRequestV1, SettingsV1, StagedBlobV1,
            ThemaPageV1, ThemaV1, ThemaWriteRequestV1, UpdateSettingsRequestV1, valid_bearer_token,
        },
        offline_asset_capabilities, offline_asset_result_json, offline_content_capabilities,
        offline_content_result_json, offline_result_json, replica_change_page,
        replica_snapshot_page, replica_state_response, review_pack_json,
    },
    service::{ServiceError, ServiceHandle},
    store::StoreError,
};

const MAX_BODY: usize = MAX_OFFLINE_ASSET_BYTES as usize;
const MAX_PAGE: u16 = 500;
const MAX_REPLICA_PAGE: u16 = 32;

#[derive(Clone)]
pub(crate) struct HttpState {
    service: ServiceHandle,
    cursors: broadcast::Sender<u64>,
}

pub(crate) fn router(service: ServiceHandle) -> Router {
    let (cursors, _) = broadcast::channel(64);
    let state = HttpState { service, cursors };
    let data =
        data_router().route_layer(middleware::from_fn_with_state(state.clone(), authenticate));
    Router::new()
        .route("/v1/openapi.json", get(openapi))
        .route("/health", get(health))
        .merge(data)
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(ConcurrencyLimitLayer::new(64))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(handle_service_error))
                .layer(TimeoutLayer::new(Duration::from_secs(30))),
        )
        .with_state(state)
        .layer(middleware::from_fn(normalize_rejection))
}

async fn handle_service_error(_error: BoxError) -> ApiError {
    ApiError::new(
        StatusCode::REQUEST_TIMEOUT,
        ApiErrorCodeV1::RequestTimeout,
        "request exceeded the server timeout",
    )
}

async fn normalize_rejection(request: Request, next: Next) -> Response {
    let response = next.run(request).await;
    if !response.status().is_client_error() && !response.status().is_server_error() {
        return response;
    }
    let json_response = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("application/json"));
    if json_response {
        return response;
    }
    let status = response.status();
    ApiError::new(
        status,
        ApiErrorCodeV1::InvalidRequest,
        "request could not be processed",
    )
    .into_response()
}

struct ApiJson<T>(T);

impl<S, T> FromRequest<S> for ApiJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(value)| Self(value))
            .map_err(ApiError::json_rejection)
    }
}

struct ApiBytes(Bytes);

impl<S> FromRequest<S> for ApiBytes
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(request: Request, state: &S) -> Result<Self, Self::Rejection> {
        Bytes::from_request(request, state)
            .await
            .map(Self)
            .map_err(ApiError::bytes_rejection)
    }
}

fn data_router() -> Router<HttpState> {
    Router::new()
        .route("/v1/changes", get(changes))
        .route("/v1/sync/state", get(replica_state))
        .route("/v1/sync/content-capabilities", get(content_capabilities))
        .route("/v1/sync/asset-capabilities", get(asset_capabilities))
        .route("/v1/sync/blobs/{digest}", axum::routing::put(stage_blob))
        .route("/v1/sync/changes", get(replica_changes))
        .route("/v1/sync/snapshot", get(replica_snapshot))
        .route("/v1/events", get(events))
        .route("/v1/settings", get(get_settings).put(update_settings))
        .route("/v1/notes", post(create_note))
        .route(
            "/v1/notes/{id}",
            get(get_note).put(update_note).delete(delete_note),
        )
        .route("/v1/notes/{id}/rename", post(rename_note))
        .route("/v1/notes/{id}/revisions", get(note_revisions))
        .route("/v1/notes/{id}/revisions/{revision}", get(note_revision))
        .route(
            "/v1/assets/{digest}/{name}",
            get(get_asset).put(put_asset).delete(delete_asset),
        )
        .route("/v1/blobs/{digest}", get(get_blob))
        .route("/v1/content/offline-operations", post(reconcile_content))
        .route(
            "/v1/content/offline-asset-operations",
            post(reconcile_asset),
        )
        .route("/v1/themata", post(create_thema).get(search_themata))
        .route("/v1/themata/{id}", get(get_thema).put(update_thema))
        .route("/v1/themata/{id}/archive", post(archive_thema))
        .route("/v1/themata/{id}/unarchive", post(unarchive_thema))
        .route("/v1/nodes", get(search_nodes))
        .route("/v1/nodes/{id}", get(get_node))
        .route("/v1/nodes/{id}/inbound", get(inbound_nodes))
        .route("/v1/nodes/{id}/outbound", get(outbound_nodes))
        .route("/v1/review/sessions", post(create_session))
        .route("/v1/review/sessions/active", get(active_session))
        .route("/v1/review/sessions/{id}/resume", post(resume_session))
        .route("/v1/review/sessions/{id}/quit", post(quit_session))
        .route("/v1/review/sessions/{id}/abort", post(abort_session))
        .route("/v1/review/sessions/{id}/attempts", post(begin_attempt))
        .route("/v1/review/packs", post(create_pack))
        .route("/v1/review/packs/active", get(active_pack))
        .route("/v1/review/packs/{id}", get(get_pack))
        .route(
            "/v1/review/packs/{id}/assets/{digest}/{name}",
            get(get_pack_asset),
        )
        .route("/v1/review/packs/{id}/abandon", post(abandon_pack))
        .route(
            "/v1/review/offline-operations",
            post(reconcile_offline)
                .layer(DefaultBodyLimit::max(MAX_OFFLINE_REVIEW_OPERATION_BYTES)),
        )
        .route("/v1/review/attempts/{id}/answer", post(answer_attempt))
        .route("/v1/review/attempts/{id}", get(get_attempt_state))
        .route(
            "/v1/review/attempts/{id}/accepted-success",
            axum::routing::put(set_accepted_success),
        )
        .route("/v1/review/attempts/{id}/override", post(override_attempt))
        .route(
            "/v1/review/attempts/{id}/suspension",
            post(toggle_suspension).put(set_suspension),
        )
        .route("/v1/review/attempts/{id}/commit", post(commit_attempt))
}

async fn authenticate(
    State(state): State<HttpState>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let token = bearer(request.headers())?;
    let principal = state
        .service
        .authenticate(token, now_us_i64()?)
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::unauthorized)?;
    request.extensions_mut().insert(principal);
    Ok(next.run(request).await)
}

fn bearer(headers: &HeaderMap) -> Result<String, ApiError> {
    let value = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(ApiError::unauthorized)?;
    let token = value
        .strip_prefix("Bearer ")
        .ok_or_else(ApiError::unauthorized)?;
    if !valid_bearer_token(token) {
        Err(ApiError::unauthorized())
    } else {
        Ok(token.to_owned())
    }
}

async fn health() -> Json<Value> {
    Json(json!({"status":"ok"}))
}

async fn openapi() -> Json<Value> {
    Json(openapi_document())
}

#[derive(Deserialize)]
struct PageQuery {
    after: Option<String>,
    limit: Option<u16>,
    q: Option<String>,
}

fn page_limit(value: Option<u16>) -> Result<u16, ApiError> {
    let value = value.unwrap_or(100);
    if (1..=MAX_PAGE).contains(&value) {
        Ok(value)
    } else {
        Err(ApiError::bad_request("invalid page limit"))
    }
}

fn replica_page_limit(value: Option<u16>) -> Result<u16, ApiError> {
    let value = value.unwrap_or(16);
    if (1..=MAX_REPLICA_PAGE).contains(&value) {
        Ok(value)
    } else {
        Err(ApiError::bad_request("invalid replica page limit"))
    }
}

async fn changes(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<PageQuery>,
) -> Result<Json<ContentChangePageV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let after = query
        .after
        .as_deref()
        .unwrap_or("0")
        .parse()
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let items = state
        .service
        .changes(after, page_limit(query.limit)?)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        ContentChangePageV1::from_application(&items).map_err(|_| ApiError::internal())?,
    ))
}

#[derive(Deserialize)]
struct ReplicaChangesQuery {
    cursor: String,
    limit: Option<u16>,
}

async fn replica_state(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
) -> Result<Json<ReplicaStateV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let replica = state
        .service
        .replica_state(principal.device_id)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        replica_state_response(&replica).map_err(ApiError::replica_wire)?,
    ))
}

async fn content_capabilities(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
) -> Result<Json<ContentCapabilitiesV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let replica = state
        .service
        .replica_state(principal.device_id)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        offline_content_capabilities(&replica.server_id).map_err(ApiError::offline_content_wire)?,
    ))
}

async fn asset_capabilities(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
) -> Result<Json<AssetCapabilitiesV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let replica = state
        .service
        .replica_state(principal.device_id)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        offline_asset_capabilities(&replica.server_id).map_err(ApiError::offline_asset_wire)?,
    ))
}

async fn replica_changes(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<ReplicaChangesQuery>,
) -> Result<Json<ReplicaChangePageV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let (server_id, after, after_token) =
        decode_cursor(&query.cursor).map_err(ApiError::replica_wire)?;
    let (changes, latest, _) = state
        .service
        .replica_changes(
            server_id.clone(),
            after,
            after_token.clone(),
            replica_page_limit(query.limit)?,
        )
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        replica_change_page(&server_id, after, &after_token, latest, &changes)
            .map_err(ApiError::replica_wire)?,
    ))
}

#[derive(Deserialize)]
struct SnapshotQuery {
    cursor: Option<String>,
    limit: Option<u16>,
}

async fn replica_snapshot(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<SnapshotQuery>,
) -> Result<Json<ReplicaSnapshotPageV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let limit = replica_page_limit(query.limit)?;
    let page = match query.cursor {
        Some(cursor) => {
            let position = decode_snapshot_cursor(&cursor).map_err(ApiError::replica_wire)?;
            state
                .service
                .replica_snapshot_page(principal.device_id, position, limit)
                .await
        }
        None => {
            state
                .service
                .create_replica_snapshot(principal.device_id, now_us()?, limit)
                .await
        }
    }
    .map_err(ApiError::service)?;
    Ok(Json(
        replica_snapshot_page(&page).map_err(ApiError::replica_wire)?,
    ))
}

async fn events(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
) -> Result<Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let stream = BroadcastStream::new(state.cursors.subscribe()).filter_map(|cursor| {
        cursor
            .ok()
            .map(|cursor| Ok(Event::default().event("cursor").data(cursor.to_string())))
    });
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

async fn create_note(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(input): ApiJson<NoteWriteRequestV1>,
) -> Result<(StatusCode, Json<DocumentHeadV1>), ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let prepared = prepare_note(None, input.logical_path, input.org, None)
        .map_err(ApiError::invalid_content)?;
    let head = state
        .service
        .replace_note(prepared, now_us_i64()?, Some(principal.device_id))
        .await
        .map_err(note_write_error)?;
    announce(&state).await;
    Ok((
        StatusCode::CREATED,
        Json(DocumentHeadV1::from_application(&head)),
    ))
}

async fn update_note(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<NoteWriteRequestV1>,
) -> Result<Json<DocumentHeadV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let revision = if_match(&headers)?;
    let prepared = prepare_note(
        Some(id.clone()),
        input.logical_path,
        input.org,
        Some(revision),
    )
    .map_err(ApiError::invalid_content)?;
    let head = state
        .service
        .replace_note(prepared, now_us_i64()?, Some(principal.device_id))
        .await
        .map_err(|error| conflict_with(error, revision))?;
    announce(&state).await;
    Ok(Json(DocumentHeadV1::from_application(&head)))
}

async fn get_note(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let id = DocumentId::new(id).map_err(|_| ApiError::bad_request("invalid document ID"))?;
    let (head, bytes) = state
        .service
        .note(id)
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/org; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", head.revision))
            .map_err(|_| ApiError::internal())?,
    );
    Ok(response)
}

async fn rename_note(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<NoteRenameRequestV1>,
) -> Result<Json<DocumentHeadV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let id = DocumentId::new(id).map_err(|_| ApiError::bad_request("invalid document ID"))?;
    let head = state
        .service
        .rename_note(
            id,
            input.logical_path,
            if_match(&headers)?,
            now_us_i64()?,
            Some(principal.device_id),
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(DocumentHeadV1::from_application(&head)))
}

async fn delete_note(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DocumentHeadV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let id = DocumentId::new(id).map_err(|_| ApiError::bad_request("invalid document ID"))?;
    let head = state
        .service
        .tombstone_note(
            id,
            if_match(&headers)?,
            now_us_i64()?,
            Some(principal.device_id),
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(DocumentHeadV1::from_application(&head)))
}

async fn note_revisions(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
) -> Result<Json<DocumentRevisionPageV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let id = DocumentId::new(id).map_err(|_| ApiError::bad_request("invalid document ID"))?;
    let after = query
        .after
        .as_deref()
        .unwrap_or("0")
        .parse()
        .map_err(|_| ApiError::bad_request("invalid revision cursor"))?;
    let items = state
        .service
        .note_revisions(id, after, page_limit(query.limit)?)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        DocumentRevisionPageV1::from_application(&items).map_err(|_| ApiError::internal())?,
    ))
}

async fn note_revision(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path((id, revision)): Path<(String, u64)>,
) -> Result<Response, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let id = DocumentId::new(id).map_err(|_| ApiError::bad_request("invalid document ID"))?;
    let (record, bytes) = state
        .service
        .note_revision(id, revision)
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    if record.blob_sha256.is_none() {
        let revision = DocumentRevisionSummaryV1::from_application(&record)
            .map_err(|_| ApiError::internal())?;
        return Ok((StatusCode::GONE, Json(revision)).into_response());
    }
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        "text/org; charset=utf-8"
            .parse()
            .map_err(|_| ApiError::internal())?,
    );
    response.headers_mut().insert(
        header::ETAG,
        format!("\"{}\"", record.revision)
            .parse()
            .map_err(|_| ApiError::internal())?,
    );
    Ok(response)
}

async fn put_asset(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path((digest, name)): Path<(String, String)>,
    headers: HeaderMap,
    ApiBytes(body): ApiBytes,
) -> Result<(StatusCode, Json<AssetHeadV1>), ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let path = canonical_asset_path(&digest, &name).map_err(ApiError::invalid_content)?;
    let expected = optional_if_match(&headers)?;
    let prepared = prepare_asset(path, name, expected).map_err(ApiError::invalid_content)?;
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let head = state
        .service
        .put_asset(
            prepared,
            body.to_vec(),
            media,
            now_us_i64()?,
            Some(principal.device_id),
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok((
        StatusCode::CREATED,
        Json(AssetHeadV1::from_application(&head)),
    ))
}

async fn stage_blob(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(digest): Path<String>,
    headers: HeaderMap,
    ApiBytes(body): ApiBytes,
) -> Result<(StatusCode, Json<StagedBlobV1>), ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let media_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let blob = state
        .service
        .stage_blob(&digest, body.to_vec(), media_type, now_us_i64()?)
        .await
        .map_err(ApiError::service)?;
    Ok((
        StatusCode::CREATED,
        Json(StagedBlobV1 {
            content_sha256: blob.sha256,
            size_bytes: blob.size_bytes,
        }),
    ))
}

async fn get_asset(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path((digest, name)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let path = canonical_asset_path(&digest, &name).map_err(ApiError::invalid_content)?;
    let (head, bytes) = state
        .service
        .asset(path)
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    let mut response = bytes.into_response();
    if let Some(media) = head.media_type.as_deref() {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            media.parse().map_err(|_| ApiError::internal())?,
        );
    }
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", head.revision))
            .map_err(|_| ApiError::internal())?,
    );
    Ok(response)
}

async fn get_blob(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(digest): Path<String>,
) -> Result<Response, ApiError> {
    require(&principal, DeviceScope::Read)?;
    validate_digest(&digest)?;
    let (meta, bytes) = state
        .service
        .blob(digest.clone())
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        meta.media_type.parse().map_err(|_| ApiError::internal())?,
    );
    response.headers_mut().insert(
        header::ETAG,
        format!("\"{digest}\"")
            .parse()
            .map_err(|_| ApiError::internal())?,
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    Ok(response)
}

fn validate_digest(digest: &str) -> Result<(), ApiError> {
    if is_canonical_sha256(digest) {
        Ok(())
    } else {
        Err(ApiError::bad_request("invalid content digest"))
    }
}

async fn delete_asset(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path((digest, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<AssetHeadV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let path = canonical_asset_path(&digest, &name).map_err(ApiError::invalid_content)?;
    let head = state
        .service
        .tombstone_asset(
            path,
            if_match(&headers)?,
            now_us_i64()?,
            Some(principal.device_id),
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(AssetHeadV1::from_application(&head)))
}

async fn create_thema(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(input): ApiJson<ThemaWriteRequestV1>,
) -> Result<(StatusCode, Json<ThemaV1>), ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let prepared = prepared_thema(None, input, None)?;
    let settings = state.service.settings().await.map_err(ApiError::service)?;
    let record = state
        .service
        .put_thema(
            prepared,
            settings_review_day(&settings)?,
            now_us_i64()?,
            Some(principal.device_id),
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok((
        StatusCode::CREATED,
        Json(ThemaV1::from_application(&record)),
    ))
}

async fn update_thema(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<ThemaWriteRequestV1>,
) -> Result<Json<ThemaV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let prepared = prepared_thema(Some(id), input, Some(if_match(&headers)?))?;
    let settings = state.service.settings().await.map_err(ApiError::service)?;
    let record = state
        .service
        .put_thema(
            prepared,
            settings_review_day(&settings)?,
            now_us_i64()?,
            Some(principal.device_id),
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(ThemaV1::from_application(&record)))
}

fn prepared_thema(
    id: Option<String>,
    input: ThemaWriteRequestV1,
    revision: Option<u64>,
) -> Result<crate::application::PreparedThema, ApiError> {
    let content = thema_content(
        &input.kind,
        input.keimenon,
        input.hypothesis,
        input.answer,
        input.source_guid,
        input.parathema,
        input.review_image,
    )
    .map_err(ApiError::invalid_content)?;
    prepare_thema(
        id,
        content,
        input.tags,
        input.node_ids,
        input.asset_paths,
        revision,
    )
    .map_err(ApiError::invalid_content)
}

async fn get_thema(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<ThemaV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let record = state
        .service
        .thema(id)
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(ThemaV1::from_application(&record)))
}

async fn search_themata(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<PageQuery>,
) -> Result<Json<ThemaPageV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let items = state
        .service
        .search_themata(
            query.q.unwrap_or_default(),
            page_limit(query.limit)?,
            query.after,
        )
        .await
        .map_err(ApiError::service)?;
    Ok(Json(ThemaPageV1::from_application(&items)))
}

async fn archive_thema(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
    headers: HeaderMap,
) -> Result<Json<ThemaV1>, ApiError> {
    set_archive(state, principal, path, headers, true).await
}
async fn unarchive_thema(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
    headers: HeaderMap,
) -> Result<Json<ThemaV1>, ApiError> {
    set_archive(state, principal, path, headers, false).await
}
async fn set_archive(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    archived: bool,
) -> Result<Json<ThemaV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let record = state
        .service
        .archive_thema(
            id,
            if_match(&headers)?,
            archived,
            now_us_i64()?,
            Some(principal.device_id),
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(ThemaV1::from_application(&record)))
}

async fn get_node(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<NodeV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let node = state
        .service
        .node(id)
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(NodeV1::from_application(&node)))
}
async fn search_nodes(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<PageQuery>,
) -> Result<Json<NodePageV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let items = state
        .service
        .search_nodes(
            query.q.unwrap_or_default(),
            page_limit(query.limit)?,
            query.after,
        )
        .await
        .map_err(ApiError::service)?;
    Ok(Json(NodePageV1::from_application(&items)))
}
async fn inbound_nodes(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
    query: Query<PageQuery>,
) -> Result<Json<NodePageV1>, ApiError> {
    graph(state, principal, path, query, true).await
}
async fn outbound_nodes(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
    query: Query<PageQuery>,
) -> Result<Json<NodePageV1>, ApiError> {
    graph(state, principal, path, query, false).await
}
async fn graph(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
    inbound: bool,
) -> Result<Json<NodePageV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let items = state
        .service
        .graph_nodes(id, inbound, page_limit(query.limit)?, query.after)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(NodePageV1::from_application(&items)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SessionInput {
    source: ReviewSource,
    target_count: Option<u32>,
}
async fn create_session(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(input): ApiJson<SessionInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require(&principal, DeviceScope::Review)?;
    let session = state
        .service
        .create_review_session(
            input.source,
            input.target_count,
            Some(principal.device_id),
            now_us()?,
        )
        .await
        .map_err(ApiError::service)?;
    Ok((StatusCode::CREATED, Json(session_json(&session))))
}
async fn active_session(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let session = state
        .service
        .active_review_session(review_actor(&principal))
        .await
        .map_err(ApiError::service)?;
    Ok(Json(json!({"session":session.as_ref().map(session_json)})))
}
async fn resume_session(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    Ok(Json(session_json(
        &state
            .service
            .resume_review_session(id, review_actor(&principal))
            .await
            .map_err(ApiError::service)?,
    )))
}
async fn quit_session(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
) -> Result<Json<Value>, ApiError> {
    stop_session(state, principal, path, false).await
}
async fn abort_session(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
) -> Result<Json<Value>, ApiError> {
    stop_session(state, principal, path, true).await
}
async fn stop_session(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    abort: bool,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    Ok(Json(session_json(
        &state
            .service
            .stop_review_session(id, review_actor(&principal), abort, now_us()?)
            .await
            .map_err(ApiError::service)?,
    )))
}
async fn begin_attempt(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require(&principal, DeviceScope::Review)?;
    let attempt = state
        .service
        .begin_review_attempt(id, review_actor(&principal), now_us()?)
        .await
        .map_err(ApiError::service)?;
    Ok((StatusCode::CREATED, Json(attempt_json(&attempt))))
}
async fn answer_attempt(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    ApiJson(answer): ApiJson<AnswerInput>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    Ok(Json(attempt_json(
        &state
            .service
            .grade_review_attempt(id, review_actor(&principal), answer, now_us()?)
            .await
            .map_err(ApiError::service)?,
    )))
}
async fn get_attempt_state(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<ReviewAttemptStateResponseV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let attempt = state
        .service
        .review_attempt_state(id, review_actor(&principal))
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        ReviewAttemptStateResponseV1::from_application(&attempt)
            .map_err(|_| ApiError::internal())?,
    ))
}
async fn set_accepted_success(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<SetAcceptedSuccessRequestV1>,
) -> Result<Json<ReviewAttemptOperationResponseV1>, ApiError> {
    apply_attempt_operation(
        &state,
        &principal,
        ReviewAttemptOperation::new(
            body.version,
            body.operation_id,
            id,
            body.expected_attempt_revision,
            ReviewAttemptMutation::AcceptedSuccess(body.accepted_success),
        )
        .map_err(|_| ApiError::bad_request("review attempt operation is invalid"))?,
    )
    .await
}
async fn override_attempt(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    Ok(Json(attempt_json(
        &state
            .service
            .override_review_attempt(id, review_actor(&principal))
            .await
            .map_err(ApiError::service)?,
    )))
}
async fn toggle_suspension(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    Ok(Json(attempt_json(
        &state
            .service
            .toggle_attempt_suspension(id, review_actor(&principal))
            .await
            .map_err(ApiError::service)?,
    )))
}
async fn set_suspension(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<SetSuspensionRequestV1>,
) -> Result<Json<ReviewAttemptOperationResponseV1>, ApiError> {
    apply_attempt_operation(
        &state,
        &principal,
        ReviewAttemptOperation::new(
            body.version,
            body.operation_id,
            id,
            body.expected_attempt_revision,
            ReviewAttemptMutation::Suspension(body.desired_suspended),
        )
        .map_err(|_| ApiError::bad_request("review attempt operation is invalid"))?,
    )
    .await
}
async fn apply_attempt_operation(
    state: &HttpState,
    principal: &DevicePrincipal,
    operation: ReviewAttemptOperation,
) -> Result<Json<ReviewAttemptOperationResponseV1>, ApiError> {
    require(principal, DeviceScope::Review)?;
    let result = state
        .service
        .apply_review_attempt_operation(
            operation,
            ReviewOperationAuthority::device(
                principal.device_id.clone(),
                principal.scopes.contains(&DeviceScope::Admin),
            ),
            now_us()?,
        )
        .await
        .map_err(ApiError::service)?;
    attempt_operation_response(result)
}

fn attempt_operation_response(
    result: ReviewAttemptOperationResult,
) -> Result<Json<ReviewAttemptOperationResponseV1>, ApiError> {
    match result.status {
        ReviewAttemptOperationStatus::Accepted => Ok(Json(
            ReviewAttemptOperationResponseV1::from_application(&result)
                .map_err(|_| ApiError::internal())?,
        )),
        ReviewAttemptOperationStatus::RevisionConflict => Err(ApiError::conflict(
            result.expected_attempt_revision,
            Some(result.attempt_revision),
        )),
        ReviewAttemptOperationStatus::StaleAttempt => Err(ApiError::new(
            StatusCode::PRECONDITION_FAILED,
            ApiErrorCodeV1::StaleAttempt,
            "thema content or review state changed",
        )),
        ReviewAttemptOperationStatus::StateConflict => Err(ApiError::new(
            StatusCode::CONFLICT,
            ApiErrorCodeV1::ReviewStateConflict,
            "review attempt is not mutable",
        )),
    }
}
async fn commit_attempt(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let event = state
        .service
        .commit_review_attempt(id, review_actor(&principal), now_us()?)
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(json!({"event_id":event})))
}

async fn create_pack(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(input): ApiJson<SessionInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require(&principal, DeviceScope::Review)?;
    let pack = state
        .service
        .create_review_pack(
            crate::protocol::OFFLINE_REVIEW_VERSION,
            input.source,
            input.target_count,
            principal.device_id,
            now_us()?,
        )
        .await
        .map_err(ApiError::service)?;
    Ok((
        StatusCode::CREATED,
        Json(review_pack_json(&pack).map_err(|_| ApiError::internal())?),
    ))
}

async fn get_pack(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let pack = state
        .service
        .review_pack(id, review_actor(&principal))
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(
        review_pack_json(&pack).map_err(|_| ApiError::internal())?,
    ))
}

async fn active_pack(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let pack = state
        .service
        .active_review_pack(review_actor(&principal))
        .await
        .map_err(ApiError::service)?;
    let version = pack
        .as_ref()
        .map_or(crate::protocol::OFFLINE_REVIEW_VERSION, |pack| {
            pack.format_version
        });
    Ok(Json(
        active_review_pack_json(version, pack.as_ref()).map_err(|_| ApiError::internal())?,
    ))
}

async fn get_pack_asset(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path((id, digest, name)): Path<(String, String, String)>,
) -> Result<Response, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let path = canonical_asset_path(&digest, &name).map_err(ApiError::invalid_content)?;
    let (asset, bytes) = state
        .service
        .review_pack_asset(id, review_actor(&principal), &path, &digest)
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    let mut response = bytes.into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        asset.media_type.parse().map_err(|_| ApiError::internal())?,
    );
    Ok(response)
}

async fn abandon_pack(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let pack = state
        .service
        .abandon_review_pack(id, review_actor(&principal), now_us()?)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        review_pack_json(&pack).map_err(|_| ApiError::internal())?,
    ))
}

async fn reconcile_offline(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(submission): ApiJson<OfflineReviewSubmission>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let decoded = submission.decode().map_err(ApiError::offline_review_wire)?;
    let result = state
        .service
        .reconcile_offline_operation(
            decoded.version,
            decoded.operation,
            decoded.canonical_payload,
            review_actor(&principal),
            now_us()?,
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(
        offline_result_json(&result).map_err(|_| ApiError::internal())?,
    ))
}

async fn reconcile_content(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(submission): ApiJson<OfflineContentSubmission>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let decoded = submission
        .decode()
        .map_err(ApiError::offline_content_wire)?;
    let result = state
        .service
        .reconcile_content_operation(
            decoded.version,
            decoded.operation,
            decoded.canonical_payload,
            Some(principal.device_id),
            now_us()?,
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(
        offline_content_result_json(&result).map_err(|_| ApiError::internal())?,
    ))
}

async fn reconcile_asset(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(submission): ApiJson<OfflineAssetSubmission>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let decoded = submission.decode().map_err(ApiError::offline_asset_wire)?;
    let result = state
        .service
        .reconcile_asset_operation(
            decoded.version,
            decoded.operation,
            decoded.canonical_payload,
            Some(principal.device_id),
            now_us()?,
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(
        offline_asset_result_json(&result).map_err(|_| ApiError::internal())?,
    ))
}

fn review_actor(principal: &DevicePrincipal) -> Option<String> {
    (!principal.scopes.contains(&DeviceScope::Admin)).then(|| principal.device_id.clone())
}

fn session_json(s: &ReviewSessionView) -> Value {
    json!({"session_id":s.session_id,"owner_device_id":s.owner_device_id,"mode":s.mode,
        "source":s.source,"target_count":s.target_count,"status":s.status,
        "current_position":s.current_position,"queue_length":s.queue_length,
        "started_at_us":s.started_at_us})
}
fn attempt_json(a: &ReviewAttemptView) -> Value {
    let presentation = &a.presentation;
    let reveal = a.response.is_some();
    json!({
        "attempt_id":a.attempt_id,
        "session_id":a.session_id,
        "thema_id":a.thema_id,
        "presentation":{
            "kind":presentation.kind,
            "keimenon":presentation.keimenon,
            "hypothesis":presentation.hypothesis,
            "source_guid":presentation.source_guid,
            "review_image":presentation.review_image,
            "tags":presentation.tags
        },
        "revealed_answer":reveal.then_some(&presentation.answer),
        "revealed_parathema":reveal.then_some(&presentation.parathema),
        "matcher_version":a.matcher_version,
        "response":a.response,
        "detected_success":a.detected_success,
        "accepted_success":a.accepted_success,
        "override_count":a.override_count,
        "proposed_transition":a.proposed_transition,
        "desired_suspended":a.desired_suspended,
        "status":a.status
    })
}

fn require(principal: &DevicePrincipal, scope: DeviceScope) -> Result<(), ApiError> {
    if principal.permits(scope) {
        Ok(())
    } else {
        Err(ApiError::forbidden())
    }
}
fn if_match(headers: &HeaderMap) -> Result<u64, ApiError> {
    optional_if_match(headers)?.ok_or_else(|| ApiError::precondition("If-Match is required"))
}
fn optional_if_match(headers: &HeaderMap) -> Result<Option<u64>, ApiError> {
    headers
        .get(header::IF_MATCH)
        .map(|v| {
            v.to_str()
                .ok()
                .and_then(parse_if_match)
                .ok_or_else(|| ApiError::bad_request("invalid If-Match"))
        })
        .transpose()
}

fn parse_if_match(value: &str) -> Option<u64> {
    let revision = if let Some(value) = value.strip_prefix('"') {
        value.strip_suffix('"')?
    } else if value.ends_with('"') {
        return None;
    } else {
        value
    };
    if revision.is_empty() || !revision.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    revision.parse().ok()
}

async fn get_settings(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
) -> Result<Json<SettingsV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let settings = state.service.settings().await.map_err(ApiError::service)?;
    Ok(Json(SettingsV1::from_application(&settings)))
}

async fn update_settings(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<UpdateSettingsRequestV1>,
) -> Result<Json<SettingsV1>, ApiError> {
    require(&principal, DeviceScope::SettingsWrite)?;
    let settings = state
        .service
        .update_settings(
            input.into_application(),
            if_match(&headers)?,
            now_us_i64()?,
            Some(principal.device_id),
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(SettingsV1::from_application(&settings)))
}

fn now_us() -> Result<u64, ApiError> {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| ApiError::internal())?
            .as_micros(),
    )
    .map_err(|_| ApiError::internal())
}
fn now_us_i64() -> Result<i64, ApiError> {
    i64::try_from(now_us()?).map_err(|_| ApiError::internal())
}
fn settings_review_day(settings: &SettingsRecord) -> Result<u32, ApiError> {
    review_day_at(now_us()?, &settings.timezone, settings.day_boundary_hour)
        .map_err(|_| ApiError::internal())
}
async fn announce(state: &HttpState) {
    if let Ok(cursor) = state.service.latest_content_cursor().await {
        let _ = state.cursors.send(cursor);
    }
}
fn conflict_with(error: ServiceError, expected: u64) -> ApiError {
    match error {
        ServiceError::Store(StoreError::Conflict { actual, .. }) => {
            ApiError::conflict(expected, actual)
        }
        other => note_write_error(other),
    }
}

fn note_write_error(error: ServiceError) -> ApiError {
    match error {
        ServiceError::Store(StoreError::NotFound) => {
            ApiError::invalid_content("inactive document asset")
        }
        other => ApiError::service(other),
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: ApiErrorCodeV1,
    message: &'static str,
    meta: Option<ApiErrorMetaV1>,
}
impl ApiError {
    fn new(status: StatusCode, code: ApiErrorCodeV1, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            meta: None,
        }
    }
    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            ApiErrorCodeV1::Unauthorized,
            "bearer token is missing or invalid",
        )
    }
    fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            ApiErrorCodeV1::Forbidden,
            "token scope does not permit this operation",
        )
    }
    fn not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            ApiErrorCodeV1::NotFound,
            "resource not found",
        )
    }
    fn bad_request(message: &'static str) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            ApiErrorCodeV1::InvalidRequest,
            message,
        )
    }
    fn precondition(message: &'static str) -> Self {
        Self::new(
            StatusCode::PRECONDITION_REQUIRED,
            ApiErrorCodeV1::PreconditionRequired,
            message,
        )
    }
    fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiErrorCodeV1::Internal,
            "internal server failure",
        )
    }
    fn invalid_content(_error: impl std::fmt::Debug) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            ApiErrorCodeV1::InvalidContent,
            "content validation failed",
        )
    }
    fn json_rejection(error: JsonRejection) -> Self {
        Self::body_rejection(error.status(), "request body is not valid JSON")
    }
    fn bytes_rejection(error: BytesRejection) -> Self {
        Self::body_rejection(error.status(), "request body is invalid")
    }
    fn body_rejection(status: StatusCode, message: &'static str) -> Self {
        if status == StatusCode::PAYLOAD_TOO_LARGE {
            Self::new(
                status,
                ApiErrorCodeV1::RequestTooLarge,
                "request body exceeds the 1 MiB limit",
            )
        } else {
            Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCodeV1::InvalidBody,
                message,
            )
        }
    }
    fn offline_review_wire(error: OfflineReviewWireError) -> Self {
        match error {
            OfflineReviewWireError::Version => Self::new(
                StatusCode::BAD_REQUEST,
                ApiErrorCodeV1::UnsupportedOfflineReviewVersion,
                "offline review protocol version is unsupported",
            ),
            OfflineReviewWireError::Operation => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCodeV1::InvalidOfflineReviewOperation,
                "offline review operation is invalid",
            ),
        }
    }
    fn offline_content_wire(error: OfflineContentWireError) -> Self {
        match error {
            OfflineContentWireError::Version => Self::new(
                StatusCode::BAD_REQUEST,
                ApiErrorCodeV1::UnsupportedOfflineContentVersion,
                "offline content protocol version is unsupported",
            ),
            OfflineContentWireError::Operation => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCodeV1::InvalidOfflineContentOperation,
                "offline content operation is invalid",
            ),
        }
    }
    fn offline_asset_wire(error: OfflineAssetWireError) -> Self {
        match error {
            OfflineAssetWireError::Version => Self::new(
                StatusCode::BAD_REQUEST,
                ApiErrorCodeV1::UnsupportedOfflineAssetVersion,
                "offline asset protocol version is unsupported",
            ),
            OfflineAssetWireError::Operation => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCodeV1::InvalidOfflineAssetOperation,
                "offline asset operation is invalid",
            ),
        }
    }
    fn replica_wire(error: ReplicaWireError) -> Self {
        match error {
            ReplicaWireError::Cursor | ReplicaWireError::Snapshot => {
                Self::bad_request("replica cursor is invalid")
            }
            ReplicaWireError::PageTooLarge => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                ApiErrorCodeV1::ReplicaProjectionTooLarge,
                "stored projection exceeds the replica response budget",
            ),
            ReplicaWireError::Encode | ReplicaWireError::Response => Self::internal(),
        }
    }
    fn snapshot_required() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: ApiErrorCodeV1::SnapshotRequired,
            message: "replica cursor cannot be resumed from this server state",
            meta: Some(ApiErrorMetaV1::Snapshot {
                snapshot_url: "/v1/sync/snapshot".into(),
            }),
        }
    }
    fn conflict(expected: u64, actual: Option<u64>) -> Self {
        Self {
            status: StatusCode::PRECONDITION_FAILED,
            code: ApiErrorCodeV1::RevisionConflict,
            message: "resource revision changed",
            meta: Some(ApiErrorMetaV1::Revision {
                expected_revision: expected,
                current_revision: actual,
            }),
        }
    }
    fn service(error: ServiceError) -> Self {
        match error {
            ServiceError::Busy => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                ApiErrorCodeV1::Busy,
                "server is busy",
            ),
            ServiceError::AssetDigest => Self::bad_request("asset digest does not match content"),
            ServiceError::InvalidReviewReplay => {
                Self::bad_request("review replay identity is invalid")
            }
            ServiceError::InvalidBlobMetadata => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCodeV1::InvalidBlobMetadata,
                "blob size or media type is invalid",
            ),
            ServiceError::Store(error) => Self::store(error),
            ServiceError::Application(error) => Self::application(error),
            ServiceError::Review(_) => Self::bad_request("review operation is invalid"),
            ServiceError::Settings(_) => Self::invalid_content("settings"),
            ServiceError::Stopped
            | ServiceError::Corrupt
            | ServiceError::Blob(_)
            | ServiceError::Backup(_) => Self::internal(),
        }
    }

    fn store(error: StoreError) -> Self {
        match error {
            StoreError::NotFound => Self::not_found(),
            StoreError::Conflict { expected, actual } => Self::conflict(expected, actual),
            StoreError::ActiveReviewSession => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCodeV1::ActiveSession,
                "a review session is already active",
            ),
            StoreError::StaleAttempt => Self::new(
                StatusCode::PRECONDITION_FAILED,
                ApiErrorCodeV1::StaleAttempt,
                "thema content or review state changed",
            ),
            StoreError::InvalidReviewState => Self::bad_request("review operation is invalid"),
            StoreError::InvalidReviewResponse => Self::bad_request("review response is invalid"),
            StoreError::ReviewOwnership => Self::forbidden(),
            StoreError::IdempotencyConflict => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCodeV1::IdempotencyConflict,
                "operation identity was reused with different content",
            ),
            StoreError::DeviceSequence { expected } => Self {
                status: StatusCode::CONFLICT,
                code: ApiErrorCodeV1::DeviceSequenceGap,
                message: "device operation sequence is not contiguous",
                meta: Some(ApiErrorMetaV1::DeviceSequence {
                    expected_sequence: expected,
                }),
            },
            StoreError::ReplicaCursor | StoreError::ReplicaServer => Self::snapshot_required(),
            StoreError::BlobRequired { digest } => Self {
                status: StatusCode::CONFLICT,
                code: ApiErrorCodeV1::BlobRequired,
                message: "operation requires a staged blob",
                meta: Some(ApiErrorMetaV1::Blob {
                    upload_url: format!("/v1/sync/blobs/{digest}"),
                    content_sha256: digest,
                }),
            },
            StoreError::BlobMetadata => Self::new(
                StatusCode::CONFLICT,
                ApiErrorCodeV1::BlobMetadataConflict,
                "blob metadata does not match stored content",
            ),
            StoreError::PathChange => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                ApiErrorCodeV1::RenameRequired,
                "document path changes require the rename endpoint",
            ),
            StoreError::InvalidProjection { .. } => Self::invalid_content("stored projection"),
            StoreError::CorruptData { .. }
            | StoreError::UnsupportedSchemaVersion { .. }
            | StoreError::Storage(_) => Self::internal(),
        }
    }

    fn application(error: ApplicationError) -> Self {
        match error {
            ApplicationError::InvalidDocumentId(error) => Self::invalid_content(error),
            ApplicationError::InvalidOrg(error) => Self::invalid_content(error),
            ApplicationError::InvalidContent(error) => Self::invalid_content(error),
            ApplicationError::Conflict { expected, actual } => Self::conflict(expected, actual),
            ApplicationError::Storage => Self::internal(),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ApiErrorResponseV1 {
            error: ApiErrorBodyV1::new(self.code, self.message.into(), self.meta),
        };
        (self.status, Json(body)).into_response()
    }
}

fn openapi_document() -> Value {
    let mut paths = serde_json::Map::new();
    for (path, method) in OPENAPI_OPERATIONS {
        let entry = paths.entry((*path).to_owned()).or_insert_with(|| json!({}));
        entry
            .as_object_mut()
            .expect("OpenAPI path item is an object")
            .insert((*method).to_owned(), openapi_operation(path, method));
    }
    json!({
        "openapi":"3.1.0",
        "jsonSchemaDialect":"https://json-schema.org/draft/2020-12/schema",
        "info":{"title":"Gnosis API","version":"1.0.0"},
        "paths":paths,
        "components":{
            "securitySchemes":{"bearerAuth":{"type":"http","scheme":"bearer"}},
            "schemas":http_v1_schemas()
        }
    })
}

fn openapi_operation(path: &str, method: &str) -> Value {
    if let Some(operation) = openapi_sync_operation(path, method) {
        return operation;
    }
    if let Some(operation) = openapi_content_operation(path, method) {
        return operation;
    }
    if let Some(operation) = openapi_review_attempt_operation(path, method) {
        return operation;
    }
    let mut operation = json!({
        "operationId":format!("{}_{}", method, path.replace("/v1/", "").replace(['/', '{', '}'], "_")),
        "security":[{"bearerAuth":[]}],
        "parameters":openapi_path_parameters(path),
        "responses":openapi_responses()
    });
    if let Some(body) = openapi_request_body(path, method) {
        operation["requestBody"] = body;
    }
    operation
}

fn openapi_content_operation(path: &str, method: &str) -> Option<Value> {
    if let Some(responses) = content_raw_responses(path, method) {
        return Some(content_operation(path, method, "read", responses));
    }
    let contract = content_json_contract(path, method)?;
    let mut operation = content_operation(
        path,
        method,
        contract.scope,
        content_json_responses(
            contract.success_status,
            contract.response,
            contract.error_statuses,
        ),
    );
    if let Some(body) = content_request_body(path, method, contract.request) {
        operation["requestBody"] = body;
    }
    Some(operation)
}

fn content_operation(path: &str, method: &str, scope: &str, responses: Value) -> Value {
    json!({
        "operationId":format!("{}_{}", method, path.replace("/v1/", "").replace(['/', '{', '}'], "_")),
        "security":[{"bearerAuth":[]}],
        "x-gnosis-required-scopes":[scope],
        "parameters":content_parameters(path, method),
        "responses":responses
    })
}

struct ContentJsonContract {
    scope: &'static str,
    request: Option<&'static str>,
    success_status: &'static str,
    response: &'static str,
    error_statuses: &'static [&'static str],
}

fn json_contract(
    scope: &'static str,
    request: Option<&'static str>,
    success_status: &'static str,
    response: &'static str,
    error_statuses: &'static [&'static str],
) -> ContentJsonContract {
    ContentJsonContract {
        scope,
        request,
        success_status,
        response,
        error_statuses,
    }
}

fn content_json_contract(path: &str, method: &str) -> Option<ContentJsonContract> {
    match (path, method) {
        ("/v1/changes", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "ContentChangePageV1",
            &["400", "401", "403", "408", "500", "503"],
        )),
        ("/v1/settings", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "SettingsV1",
            &["401", "403", "408", "500", "503"],
        )),
        ("/v1/settings", "put") => Some(json_contract(
            "settings:write",
            Some("UpdateSettingsRequestV1"),
            "200",
            "SettingsV1",
            &[
                "400", "401", "403", "408", "412", "413", "422", "428", "500", "503",
            ],
        )),
        ("/v1/notes", "post") => Some(json_contract(
            "content:write",
            Some("NoteWriteRequestV1"),
            "201",
            "DocumentHeadV1",
            &["401", "403", "408", "412", "413", "422", "500", "503"],
        )),
        ("/v1/notes/{id}", "put") => Some(json_contract(
            "content:write",
            Some("NoteWriteRequestV1"),
            "200",
            "DocumentHeadV1",
            &[
                "400", "401", "403", "408", "412", "413", "422", "428", "500", "503",
            ],
        )),
        ("/v1/notes/{id}", "delete") => Some(json_contract(
            "content:write",
            None,
            "200",
            "DocumentHeadV1",
            &[
                "400", "401", "403", "404", "408", "412", "422", "428", "500", "503",
            ],
        )),
        ("/v1/notes/{id}/rename", "post") => Some(json_contract(
            "content:write",
            Some("NoteRenameRequestV1"),
            "200",
            "DocumentHeadV1",
            &[
                "400", "401", "403", "404", "408", "412", "413", "422", "428", "500", "503",
            ],
        )),
        ("/v1/notes/{id}/revisions", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "DocumentRevisionPageV1",
            &["400", "401", "403", "408", "500", "503"],
        )),
        ("/v1/sync/blobs/{digest}", "put") => Some(json_contract(
            "content:write",
            None,
            "201",
            "StagedBlobV1",
            &[
                "400", "401", "403", "408", "409", "413", "422", "500", "503",
            ],
        )),
        ("/v1/assets/{digest}/{name}", "put") => Some(json_contract(
            "content:write",
            None,
            "201",
            "AssetHeadV1",
            &[
                "400", "401", "403", "404", "408", "409", "412", "413", "422", "500", "503",
            ],
        )),
        ("/v1/assets/{digest}/{name}", "delete") => Some(json_contract(
            "content:write",
            None,
            "200",
            "AssetHeadV1",
            &[
                "400", "401", "403", "404", "408", "412", "422", "428", "500", "503",
            ],
        )),
        ("/v1/themata", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "ThemaPageV1",
            &["400", "401", "403", "408", "500", "503"],
        )),
        ("/v1/themata", "post") => Some(json_contract(
            "content:write",
            Some("ThemaWriteRequestV1"),
            "201",
            "ThemaV1",
            &["401", "403", "404", "408", "413", "422", "500", "503"],
        )),
        ("/v1/themata/{id}", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "ThemaV1",
            &["401", "403", "404", "408", "500", "503"],
        )),
        ("/v1/themata/{id}", "put") => Some(json_contract(
            "content:write",
            Some("ThemaWriteRequestV1"),
            "200",
            "ThemaV1",
            &[
                "400", "401", "403", "404", "408", "412", "413", "422", "428", "500", "503",
            ],
        )),
        ("/v1/themata/{id}/archive", "post") | ("/v1/themata/{id}/unarchive", "post") => {
            Some(json_contract(
                "content:write",
                None,
                "200",
                "ThemaV1",
                &[
                    "400", "401", "403", "404", "408", "412", "422", "428", "500", "503",
                ],
            ))
        }
        ("/v1/nodes", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "NodePageV1",
            &["400", "401", "403", "408", "500", "503"],
        )),
        ("/v1/nodes/{id}", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "NodeV1",
            &["401", "403", "404", "408", "500", "503"],
        )),
        ("/v1/nodes/{id}/inbound", "get") | ("/v1/nodes/{id}/outbound", "get") => {
            Some(json_contract(
                "read",
                None,
                "200",
                "NodePageV1",
                &["400", "401", "403", "404", "408", "500", "503"],
            ))
        }
        _ => None,
    }
}

fn content_request_body(path: &str, method: &str, schema: Option<&str>) -> Option<Value> {
    if method == "put"
        && matches!(
            path,
            "/v1/sync/blobs/{digest}" | "/v1/assets/{digest}/{name}"
        )
    {
        return Some(json!({
            "required":true,
            "content":{"*/*":{"schema":{"type":"string","format":"binary"}}}
        }));
    }
    schema.map(openapi_json_request)
}

fn content_json_responses(status: &str, schema: &str, error_statuses: &[&str]) -> Value {
    let mut responses = serde_json::Map::new();
    responses.insert(status.into(), openapi_json_response("Success", schema));
    add_content_errors(&mut responses, error_statuses);
    Value::Object(responses)
}

fn content_raw_responses(path: &str, method: &str) -> Option<Value> {
    if method != "get" {
        return None;
    }
    let mut responses = serde_json::Map::new();
    let error_statuses: &[&str] = match path {
        "/v1/events" => {
            responses.insert(
                "200".into(),
                media_response("Event stream", "text/event-stream", false, &[]),
            );
            &["401", "403", "408", "500", "503"]
        }
        "/v1/notes/{id}" => {
            responses.insert(
                "200".into(),
                media_response("Org document", "text/org", false, &["ETag"]),
            );
            &["400", "401", "403", "404", "408", "500", "503"]
        }
        "/v1/notes/{id}/revisions/{revision}" => {
            responses.insert(
                "200".into(),
                media_response("Org revision", "text/org", false, &["ETag"]),
            );
            responses.insert(
                "410".into(),
                openapi_json_response("Tombstone", "DocumentRevisionSummaryV1"),
            );
            &["400", "401", "403", "404", "408", "500", "503"]
        }
        "/v1/assets/{digest}/{name}" => {
            responses.insert(
                "200".into(),
                media_response("Asset bytes", "*/*", true, &["ETag"]),
            );
            &["401", "403", "404", "408", "422", "500", "503"]
        }
        "/v1/blobs/{digest}" => {
            responses.insert(
                "200".into(),
                media_response("Immutable blob", "*/*", true, &["ETag", "Cache-Control"]),
            );
            &["400", "401", "403", "404", "408", "500", "503"]
        }
        _ => return None,
    };
    add_content_errors(&mut responses, error_statuses);
    Some(Value::Object(responses))
}

fn media_response(description: &str, media_type: &str, binary: bool, headers: &[&str]) -> Value {
    let schema = if binary {
        json!({"type":"string","format":"binary"})
    } else {
        json!({"type":"string"})
    };
    let headers = headers
        .iter()
        .map(|name| ((*name).to_owned(), json!({"schema":{"type":"string"}})))
        .collect::<serde_json::Map<_, _>>();
    json!({"description":description,"headers":headers,"content":{media_type:{"schema":schema}}})
}

fn add_content_errors(responses: &mut serde_json::Map<String, Value>, statuses: &[&str]) {
    for status in statuses {
        responses.insert((*status).into(), openapi_error_response());
    }
}

fn content_parameters(path: &str, method: &str) -> Vec<Value> {
    let mut parameters = openapi_path_parameters(path);
    parameters.extend(content_query_parameters(path));
    if content_requires_if_match(path, method) {
        parameters.push(if_match_parameter(true));
    } else if path == "/v1/assets/{digest}/{name}" && method == "put" {
        parameters.push(if_match_parameter(false));
    }
    parameters
}

fn content_query_parameters(path: &str) -> Vec<Value> {
    let paged = matches!(
        path,
        "/v1/changes"
            | "/v1/notes/{id}/revisions"
            | "/v1/themata"
            | "/v1/nodes"
            | "/v1/nodes/{id}/inbound"
            | "/v1/nodes/{id}/outbound"
    );
    if !paged {
        return Vec::new();
    }
    let numeric = matches!(path, "/v1/changes" | "/v1/notes/{id}/revisions");
    let mut parameters = vec![
        content_after_parameter(numeric),
        json!({"name":"limit","in":"query","required":false,"schema":{"type":"integer","minimum":1,"maximum":MAX_PAGE,"default":100}}),
    ];
    if matches!(path, "/v1/themata" | "/v1/nodes") {
        parameters
            .push(json!({"name":"q","in":"query","required":false,"schema":{"type":"string"}}));
    }
    parameters
}

fn content_after_parameter(numeric: bool) -> Value {
    let schema = if numeric {
        json!({"type":"string","pattern":"^[0-9]+$","default":"0"})
    } else {
        json!({"type":"string"})
    };
    json!({"name":"after","in":"query","required":false,"schema":schema})
}

fn content_requires_if_match(path: &str, method: &str) -> bool {
    matches!(
        (path, method),
        ("/v1/settings", "put")
            | ("/v1/notes/{id}", "put" | "delete")
            | ("/v1/notes/{id}/rename", "post")
            | ("/v1/assets/{digest}/{name}", "delete")
            | ("/v1/themata/{id}", "put")
            | ("/v1/themata/{id}/archive", "post")
            | ("/v1/themata/{id}/unarchive", "post")
    )
}

fn if_match_parameter(required: bool) -> Value {
    json!({"name":"If-Match","in":"header","required":required,"schema":{"type":"string","pattern":"^(?:[0-9]+|\"[0-9]+\")$"}})
}

fn openapi_review_attempt_operation(path: &str, method: &str) -> Option<Value> {
    let (operation_id, response_schema, request_schema) = match (path, method) {
        ("/v1/review/attempts/{id}", "get") => (
            "get_review_attempt_state",
            "ReviewAttemptStateResponseV1",
            None,
        ),
        ("/v1/review/attempts/{id}/accepted-success", "put") => (
            "put_review_attempt_accepted_success",
            "ReviewAttemptOperationResponseV1",
            Some("SetAcceptedSuccessRequestV1"),
        ),
        ("/v1/review/attempts/{id}/suspension", "put") => (
            "put_review_attempt_suspension",
            "ReviewAttemptOperationResponseV1",
            Some("SetSuspensionRequestV1"),
        ),
        _ => return None,
    };
    let mut operation = json!({
        "operationId":operation_id,
        "security":[{"bearerAuth":[]}],
        "x-gnosis-required-scopes":["review"],
        "parameters":openapi_path_parameters(path),
        "responses":review_attempt_responses(method, response_schema)
    });
    if let Some(schema) = request_schema {
        operation["requestBody"] = openapi_json_request(schema);
    }
    Some(operation)
}

fn review_attempt_responses(method: &str, schema: &str) -> Value {
    let mut responses = serde_json::Map::new();
    responses.insert("200".into(), openapi_json_response("Success", schema));
    let statuses: &[&str] = if method == "get" {
        &["401", "403", "404", "408", "500", "503"]
    } else {
        &[
            "400", "401", "403", "404", "408", "409", "412", "413", "422", "500", "503",
        ]
    };
    for status in statuses {
        responses.insert((*status).into(), openapi_error_response());
    }
    Value::Object(responses)
}

fn openapi_json_request(schema: &str) -> Value {
    json!({
        "required":true,
        "content":{"application/json":{"schema":{
            "$ref":format!("#/components/schemas/{schema}")
        }}}
    })
}

fn openapi_sync_operation(path: &str, method: &str) -> Option<Value> {
    let schema = sync_success_schema(path, method)?;
    Some(json!({
        "operationId":format!("{}_{}", method, path.replace("/v1/", "").replace('/', "_")),
        "security":[{"bearerAuth":[]}],
        "x-gnosis-required-scopes":["read"],
        "parameters":sync_parameters(path),
        "responses":sync_responses(path, schema)
    }))
}

fn sync_success_schema(path: &str, method: &str) -> Option<&'static str> {
    match (path, method) {
        ("/v1/sync/state", "get") => Some("ReplicaStateV1"),
        ("/v1/sync/content-capabilities", "get") => Some("ContentCapabilitiesV1"),
        ("/v1/sync/asset-capabilities", "get") => Some("AssetCapabilitiesV1"),
        ("/v1/sync/changes", "get") => Some("ReplicaChangePageV1"),
        ("/v1/sync/snapshot", "get") => Some("ReplicaSnapshotPageV1"),
        _ => None,
    }
}

fn sync_parameters(path: &str) -> Vec<Value> {
    let mut parameters = Vec::new();
    if path == "/v1/sync/changes" {
        parameters.push(replica_cursor_parameter(true));
    } else if path == "/v1/sync/snapshot" {
        parameters.push(replica_cursor_parameter(false));
    }
    if matches!(path, "/v1/sync/changes" | "/v1/sync/snapshot") {
        parameters.push(json!({
            "name":"limit","in":"query","required":false,
            "schema":{"type":"integer","minimum":1,"maximum":MAX_REPLICA_PAGE,"default":16}
        }));
    }
    parameters
}

fn replica_cursor_parameter(required: bool) -> Value {
    json!({
        "name":"cursor","in":"query","required":required,
        "schema":{"type":"string","minLength":1,"maxLength":crate::protocol::MAX_REPLICA_CURSOR_BYTES}
    })
}

fn sync_responses(path: &str, schema: &str) -> Value {
    let mut responses = serde_json::Map::new();
    responses.insert("200".into(), openapi_json_response("Success", schema));
    let statuses = if matches!(path, "/v1/sync/changes" | "/v1/sync/snapshot") {
        &["400", "401", "403", "408", "409", "500", "503"][..]
    } else {
        &["401", "403", "408", "500", "503"][..]
    };
    for status in statuses {
        responses.insert((*status).into(), openapi_error_response());
    }
    Value::Object(responses)
}

fn openapi_json_response(description: &str, schema: &str) -> Value {
    json!({
        "description":description,
        "content":{"application/json":{"schema":{
            "$ref":format!("#/components/schemas/{schema}")
        }}}
    })
}

fn http_v1_schemas() -> serde_json::Map<String, Value> {
    let mut generator = openapi_schema_settings().for_serialize().into_generator();
    generator.subschema_for::<ReplicaStateV1>();
    generator.subschema_for::<ContentCapabilitiesV1>();
    generator.subschema_for::<AssetCapabilitiesV1>();
    generator.subschema_for::<ReplicaChangePageV1>();
    generator.subschema_for::<ReplicaSnapshotPageV1>();
    generator.subschema_for::<ContentChangePageV1>();
    generator.subschema_for::<DocumentHeadV1>();
    generator.subschema_for::<DocumentRevisionPageV1>();
    generator.subschema_for::<DocumentRevisionSummaryV1>();
    generator.subschema_for::<AssetHeadV1>();
    generator.subschema_for::<StagedBlobV1>();
    generator.subschema_for::<ThemaPageV1>();
    generator.subschema_for::<ThemaV1>();
    generator.subschema_for::<NodePageV1>();
    generator.subschema_for::<NodeV1>();
    generator.subschema_for::<SettingsV1>();
    generator.subschema_for::<ReviewAttemptStateResponseV1>();
    generator.subschema_for::<ReviewAttemptOperationResponseV1>();
    generator.subschema_for::<ApiErrorCodeV1>();
    generator.subschema_for::<ApiErrorResponseV1>();
    let mut definitions = generator.take_definitions(true);

    let mut requests = openapi_schema_settings().for_deserialize().into_generator();
    requests.subschema_for::<NoteWriteRequestV1>();
    requests.subschema_for::<NoteRenameRequestV1>();
    requests.subschema_for::<ThemaWriteRequestV1>();
    requests.subschema_for::<UpdateSettingsRequestV1>();
    requests.subschema_for::<SetAcceptedSuccessRequestV1>();
    requests.subschema_for::<SetSuspensionRequestV1>();
    definitions.extend(requests.take_definitions(true));
    definitions
}

fn openapi_schema_settings() -> SchemaSettings {
    SchemaSettings::draft2020_12().with(|settings| {
        settings.definitions_path = "/components/schemas".into();
        settings.meta_schema = None;
    })
}

fn openapi_path_parameters(path: &str) -> Vec<Value> {
    path.split('{')
        .skip(1)
        .filter_map(|value| value.split('}').next())
        .map(|name| {
            let schema = if name == "revision" {
                json!({"type":"integer","minimum":1})
            } else {
                json!({"type":"string","minLength":1})
            };
            json!({"name":name,"in":"path","required":true,"schema":schema})
        })
        .collect()
}

fn openapi_request_body(path: &str, method: &str) -> Option<Value> {
    let has_json = matches!(method, "post" | "put")
        && !path.ends_with("/archive")
        && !path.ends_with("/unarchive")
        && !path.ends_with("/resume")
        && !path.ends_with("/quit")
        && !path.ends_with("/abort")
        && !path.ends_with("/attempts")
        && !path.ends_with("/override")
        && !path.ends_with("/suspension")
        && !path.ends_with("/commit");
    if (path.starts_with("/v1/assets/") || path.starts_with("/v1/sync/blobs/")) && method == "put" {
        Some(
            json!({"required":true,"content":{"application/octet-stream":{"schema":{"type":"string","contentEncoding":"binary"}}}}),
        )
    } else {
        has_json.then(
            || json!({"required":true,"content":{"application/json":{"schema":{"type":"object"}}}}),
        )
    }
}

fn openapi_responses() -> Value {
    let mut responses = serde_json::Map::from_iter([
        ("200".into(), json!({"description":"Success"})),
        ("201".into(), json!({"description":"Created"})),
    ]);
    for status in [
        "400", "401", "403", "404", "408", "409", "410", "412", "413", "422", "428", "500", "503",
    ] {
        responses.insert(status.into(), openapi_error_response());
    }
    Value::Object(responses)
}

fn openapi_error_response() -> Value {
    json!({"description":"Stable JSON error","content":{"application/json":{
        "schema":{"$ref":"#/components/schemas/ApiErrorResponseV1"}
    }}})
}

const OPENAPI_OPERATIONS: &[(&str, &str)] = &[
    ("/v1/changes", "get"),
    ("/v1/sync/state", "get"),
    ("/v1/sync/content-capabilities", "get"),
    ("/v1/sync/asset-capabilities", "get"),
    ("/v1/sync/blobs/{digest}", "put"),
    ("/v1/sync/changes", "get"),
    ("/v1/sync/snapshot", "get"),
    ("/v1/events", "get"),
    ("/v1/settings", "get"),
    ("/v1/settings", "put"),
    ("/v1/notes", "post"),
    ("/v1/notes/{id}", "get"),
    ("/v1/notes/{id}", "put"),
    ("/v1/notes/{id}", "delete"),
    ("/v1/notes/{id}/rename", "post"),
    ("/v1/notes/{id}/revisions", "get"),
    ("/v1/notes/{id}/revisions/{revision}", "get"),
    ("/v1/assets/{digest}/{name}", "get"),
    ("/v1/assets/{digest}/{name}", "put"),
    ("/v1/assets/{digest}/{name}", "delete"),
    ("/v1/blobs/{digest}", "get"),
    ("/v1/content/offline-operations", "post"),
    ("/v1/content/offline-asset-operations", "post"),
    ("/v1/themata", "get"),
    ("/v1/themata", "post"),
    ("/v1/themata/{id}", "get"),
    ("/v1/themata/{id}", "put"),
    ("/v1/themata/{id}/archive", "post"),
    ("/v1/themata/{id}/unarchive", "post"),
    ("/v1/nodes", "get"),
    ("/v1/nodes/{id}", "get"),
    ("/v1/nodes/{id}/inbound", "get"),
    ("/v1/nodes/{id}/outbound", "get"),
    ("/v1/review/sessions", "post"),
    ("/v1/review/sessions/active", "get"),
    ("/v1/review/sessions/{id}/resume", "post"),
    ("/v1/review/sessions/{id}/quit", "post"),
    ("/v1/review/sessions/{id}/abort", "post"),
    ("/v1/review/sessions/{id}/attempts", "post"),
    ("/v1/review/packs", "post"),
    ("/v1/review/packs/active", "get"),
    ("/v1/review/packs/{id}", "get"),
    ("/v1/review/packs/{id}/assets/{digest}/{name}", "get"),
    ("/v1/review/packs/{id}/abandon", "post"),
    ("/v1/review/offline-operations", "post"),
    ("/v1/review/attempts/{id}", "get"),
    ("/v1/review/attempts/{id}/answer", "post"),
    ("/v1/review/attempts/{id}/accepted-success", "put"),
    ("/v1/review/attempts/{id}/override", "post"),
    ("/v1/review/attempts/{id}/suspension", "post"),
    ("/v1/review/attempts/{id}/suspension", "put"),
    ("/v1/review/attempts/{id}/commit", "post"),
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use axum::{
        body::{Body, to_bytes},
        http::{HeaderMap, Request, StatusCode},
    };
    use serde_json::json;
    use sha2::{Digest as _, Sha256};
    use tower::ServiceExt;

    use super::{
        ApiError, OPENAPI_OPERATIONS, attempt_json, openapi_document, optional_if_match, router,
    };
    use crate::application::{
        AnswerInput, ApplicationError, OfflineAssetOperation, OfflineReviewOperation,
        PresentationSnapshot, ReviewAttemptView, ReviewPackView, ReviewSource, ReviewSourceKind,
        SettingsInput, generate_device, prepare_asset, prepare_thema, presentation_content,
        review_day_at, thema_content,
    };
    use crate::domain::{
        DocumentId, DocumentProjection, DocumentProjectionInput, compute_transition, fuzz_delta,
        grade,
    };
    use crate::protocol::{
        MAX_REPLICA_PAGE_BYTES, canonical_asset_operation, decode_cursor, encode_cursor,
        http_v1::{
            ApiErrorCodeV1, ApiErrorMetaV1, ApiErrorResponseV1, ContentCapabilitiesV1,
            ReplicaStateV1, ReviewAttemptV1,
        },
    };
    use crate::service::{ServiceError, ServiceRuntime};
    use crate::store::{SqliteStore, StoreError};

    #[test]
    fn service_errors_have_stable_http_contracts() {
        assert_api_error(
            ServiceError::Store(StoreError::InvalidReviewResponse),
            StatusCode::BAD_REQUEST,
            ApiErrorCodeV1::InvalidRequest,
            "review response is invalid",
            None,
        );
        assert_api_error(
            ServiceError::InvalidReviewReplay,
            StatusCode::BAD_REQUEST,
            ApiErrorCodeV1::InvalidRequest,
            "review replay identity is invalid",
            None,
        );
        assert_api_error(
            ServiceError::Application(ApplicationError::Conflict {
                expected: 7,
                actual: Some(9),
            }),
            StatusCode::PRECONDITION_FAILED,
            ApiErrorCodeV1::RevisionConflict,
            "resource revision changed",
            Some(ApiErrorMetaV1::Revision {
                expected_revision: 7,
                current_revision: Some(9),
            }),
        );
    }

    #[test]
    fn if_match_parser_accepts_only_bare_or_balanced_revisions() {
        for (value, revision) in [("7", 7), ("\"7\"", 7)] {
            assert_eq!(
                optional_if_match(&if_match_headers(value)).unwrap(),
                Some(revision)
            );
        }
        for value in ["+7", "\"+7\"", "\"7", "7\"", "\"\"7\"\""] {
            assert!(
                optional_if_match(&if_match_headers(value)).is_err(),
                "{value}"
            );
        }
    }

    fn if_match_headers(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("if-match", value.parse().unwrap());
        headers
    }

    #[test]
    fn store_recovery_errors_preserve_metadata() {
        assert_api_error(
            ServiceError::Store(StoreError::DeviceSequence { expected: 11 }),
            StatusCode::CONFLICT,
            ApiErrorCodeV1::DeviceSequenceGap,
            "device operation sequence is not contiguous",
            Some(ApiErrorMetaV1::DeviceSequence {
                expected_sequence: 11,
            }),
        );
        assert_api_error(
            ServiceError::Store(StoreError::ReplicaCursor),
            StatusCode::CONFLICT,
            ApiErrorCodeV1::SnapshotRequired,
            "replica cursor cannot be resumed from this server state",
            Some(ApiErrorMetaV1::Snapshot {
                snapshot_url: "/v1/sync/snapshot".into(),
            }),
        );
        let digest = "a".repeat(64);
        assert_api_error(
            ServiceError::Store(StoreError::BlobRequired {
                digest: digest.clone(),
            }),
            StatusCode::CONFLICT,
            ApiErrorCodeV1::BlobRequired,
            "operation requires a staged blob",
            Some(ApiErrorMetaV1::Blob {
                content_sha256: digest.clone(),
                upload_url: format!("/v1/sync/blobs/{digest}"),
            }),
        );
    }

    #[test]
    fn internal_store_errors_do_not_leak_details() {
        assert_api_error(
            ServiceError::Store(StoreError::CorruptData {
                field: "private field",
            }),
            StatusCode::INTERNAL_SERVER_ERROR,
            ApiErrorCodeV1::Internal,
            "internal server failure",
            None,
        );
    }

    fn assert_api_error(
        error: ServiceError,
        status: StatusCode,
        code: ApiErrorCodeV1,
        message: &'static str,
        meta: Option<ApiErrorMetaV1>,
    ) {
        let error = ApiError::service(error);
        assert_eq!(error.status, status);
        assert_eq!(error.code, code);
        assert_eq!(error.message, message);
        assert_eq!(error.meta, meta);
    }

    #[test]
    fn openapi_covers_versioned_routes_and_bearer_security() {
        let api = openapi_document();
        assert_eq!(api["openapi"], "3.1.0");
        assert_eq!(
            api["components"]["securitySchemes"]["bearerAuth"]["scheme"],
            "bearer"
        );
        for (path, method) in OPENAPI_OPERATIONS {
            let operation = &api["paths"][path][method];
            assert!(operation["responses"].is_object(), "{method} {path}");
            assert_eq!(operation["security"][0]["bearerAuth"], json!([]));
        }
    }

    #[test]
    fn openapi_types_replica_bootstrap_exactly() {
        let api = openapi_document();
        let state = &api["paths"]["/v1/sync/state"]["get"];
        assert_eq!(
            response_statuses(state),
            set(&["200", "401", "403", "408", "500", "503"])
        );
        assert_eq!(
            state["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ReplicaStateV1"
        );
        assert_eq!(state["x-gnosis-required-scopes"], json!(["read"]));

        let changes = &api["paths"]["/v1/sync/changes"]["get"];
        assert_eq!(changes["parameters"][0]["name"], "cursor");
        assert_eq!(changes["parameters"][0]["required"], true);
        assert_eq!(changes["parameters"][1]["schema"]["maximum"], 32);
        assert_eq!(
            response_statuses(changes),
            set(&["200", "400", "401", "403", "408", "409", "500", "503"])
        );

        let schemas = &api["components"]["schemas"];
        assert!(
            schemas["ReplicaStateV1"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("next_review_sequence"))
        );
        assert!(
            schemas["ReplicaStateV1"]["properties"]
                .get("next_device_sequence")
                .is_none()
        );
        assert!(
            schemas["ApiErrorCodeV1"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("snapshot_required"))
        );
        assert_eq!(
            schemas["DocumentOperationV1"]["enum"],
            json!(["create", "update", "rename", "snapshot", "tombstone"])
        );
        assert_eq!(
            schemas["ThemaKindV1"]["enum"],
            json!(["basic", "cloze", "mcq", "mc-cloze"])
        );
        assert_eq!(
            schemas["SettingsRevisionV1"]["properties"]["day_boundary_hour"]["maximum"],
            23
        );
        assert_eq!(
            schemas["ApiErrorBodyV1"]["anyOf"].as_array().unwrap().len(),
            5
        );
        assert_eq!(
            schemas["SnapshotApiErrorSchemaV1"]["properties"]["code"]["const"],
            "snapshot_required"
        );
        assert_eq!(
            schemas["ReplicaSnapshotPageV1"]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            schemas["ReplicaSnapshotPageV1"]["properties"]["version"],
            json!({"type":"integer","format":"uint32","minimum":0,"const":1})
        );
        assert_eq!(
            schemas["ReplicaSnapshotPageV1"]["oneOf"][0]["properties"]["cursor"]["maxLength"],
            512
        );
        assert_eq!(
            schemas["PlainApiErrorCodeSchemaV1"]["allOf"][0]["$ref"],
            "#/components/schemas/ApiErrorCodeV1"
        );
        assert_eq!(
            schemas["PlainApiErrorCodeSchemaV1"]["allOf"][1]["not"]["enum"],
            json!([
                "snapshot_required",
                "revision_conflict",
                "device_sequence_gap",
                "blob_required"
            ])
        );
        assert_local_refs_resolve(&api, &api);
    }

    #[test]
    fn openapi_types_review_attempt_operations_exactly() {
        let api = openapi_document();
        let state = &api["paths"]["/v1/review/attempts/{id}"]["get"];
        assert_eq!(state["x-gnosis-required-scopes"], json!(["review"]));
        assert_eq!(
            state["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ReviewAttemptStateResponseV1"
        );

        let acceptance = &api["paths"]["/v1/review/attempts/{id}/accepted-success"]["put"];
        assert_eq!(
            acceptance["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/SetAcceptedSuccessRequestV1"
        );
        assert_eq!(
            acceptance["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ReviewAttemptOperationResponseV1"
        );
        assert_eq!(
            response_statuses(acceptance),
            set(&[
                "200", "400", "401", "403", "404", "408", "409", "412", "413", "422", "500", "503"
            ])
        );

        let suspension = &api["paths"]["/v1/review/attempts/{id}/suspension"];
        assert!(suspension["post"].is_object());
        assert_eq!(
            suspension["put"]["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/SetSuspensionRequestV1"
        );
        let schemas = &api["components"]["schemas"];
        assert_eq!(
            schemas["SetAcceptedSuccessRequestV1"]["properties"]["version"]["const"],
            1
        );
        assert_eq!(
            schemas["SetAcceptedSuccessRequestV1"]["properties"]["operation_id"]["maxLength"],
            128
        );
        assert_eq!(
            schemas["SetAcceptedSuccessRequestV1"]["properties"]["operation_id"]["pattern"],
            "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$"
        );
        assert_eq!(
            schemas["SetAcceptedSuccessRequestV1"]["properties"]["expected_attempt_revision"]["maximum"],
            i64::MAX
        );
        assert!(
            schemas["ApiErrorCodeV1"]["enum"]
                .as_array()
                .unwrap()
                .contains(&json!("review_state_conflict"))
        );
        assert_eq!(
            schemas["ReviewAttemptStatusV1"]["enum"],
            json!(["pending", "graded", "committed"])
        );
        assert_eq!(
            schemas["ThemaKindV1"]["enum"],
            json!(["basic", "cloze", "mcq", "mc-cloze"])
        );
        assert_local_refs_resolve(&api, &api);
    }

    #[test]
    fn openapi_types_content_resources_exactly() {
        let api = openapi_document();
        let contracts = [
            expected_json_contract(
                "/v1/changes",
                "get",
                "read",
                None,
                "200",
                "ContentChangePageV1",
                &["200", "400", "401", "403", "408", "500", "503"],
            ),
            expected_json_contract(
                "/v1/settings",
                "get",
                "read",
                None,
                "200",
                "SettingsV1",
                &["200", "401", "403", "408", "500", "503"],
            ),
            expected_json_contract(
                "/v1/settings",
                "put",
                "settings:write",
                Some("UpdateSettingsRequestV1"),
                "200",
                "SettingsV1",
                &[
                    "200", "400", "401", "403", "408", "412", "413", "422", "428", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/notes",
                "post",
                "content:write",
                Some("NoteWriteRequestV1"),
                "201",
                "DocumentHeadV1",
                &[
                    "201", "401", "403", "408", "412", "413", "422", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/notes/{id}",
                "put",
                "content:write",
                Some("NoteWriteRequestV1"),
                "200",
                "DocumentHeadV1",
                &[
                    "200", "400", "401", "403", "408", "412", "413", "422", "428", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/notes/{id}",
                "delete",
                "content:write",
                None,
                "200",
                "DocumentHeadV1",
                &[
                    "200", "400", "401", "403", "404", "408", "412", "422", "428", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/notes/{id}/rename",
                "post",
                "content:write",
                Some("NoteRenameRequestV1"),
                "200",
                "DocumentHeadV1",
                &[
                    "200", "400", "401", "403", "404", "408", "412", "413", "422", "428", "500",
                    "503",
                ],
            ),
            expected_json_contract(
                "/v1/notes/{id}/revisions",
                "get",
                "read",
                None,
                "200",
                "DocumentRevisionPageV1",
                &["200", "400", "401", "403", "408", "500", "503"],
            ),
            expected_json_contract(
                "/v1/sync/blobs/{digest}",
                "put",
                "content:write",
                None,
                "201",
                "StagedBlobV1",
                &[
                    "201", "400", "401", "403", "408", "409", "413", "422", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/assets/{digest}/{name}",
                "put",
                "content:write",
                None,
                "201",
                "AssetHeadV1",
                &[
                    "201", "400", "401", "403", "404", "408", "409", "412", "413", "422", "500",
                    "503",
                ],
            ),
            expected_json_contract(
                "/v1/assets/{digest}/{name}",
                "delete",
                "content:write",
                None,
                "200",
                "AssetHeadV1",
                &[
                    "200", "400", "401", "403", "404", "408", "412", "422", "428", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/themata",
                "get",
                "read",
                None,
                "200",
                "ThemaPageV1",
                &["200", "400", "401", "403", "408", "500", "503"],
            ),
            expected_json_contract(
                "/v1/themata",
                "post",
                "content:write",
                Some("ThemaWriteRequestV1"),
                "201",
                "ThemaV1",
                &[
                    "201", "401", "403", "404", "408", "413", "422", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/themata/{id}",
                "get",
                "read",
                None,
                "200",
                "ThemaV1",
                &["200", "401", "403", "404", "408", "500", "503"],
            ),
            expected_json_contract(
                "/v1/themata/{id}",
                "put",
                "content:write",
                Some("ThemaWriteRequestV1"),
                "200",
                "ThemaV1",
                &[
                    "200", "400", "401", "403", "404", "408", "412", "413", "422", "428", "500",
                    "503",
                ],
            ),
            expected_json_contract(
                "/v1/themata/{id}/archive",
                "post",
                "content:write",
                None,
                "200",
                "ThemaV1",
                &[
                    "200", "400", "401", "403", "404", "408", "412", "422", "428", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/themata/{id}/unarchive",
                "post",
                "content:write",
                None,
                "200",
                "ThemaV1",
                &[
                    "200", "400", "401", "403", "404", "408", "412", "422", "428", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/nodes",
                "get",
                "read",
                None,
                "200",
                "NodePageV1",
                &["200", "400", "401", "403", "408", "500", "503"],
            ),
            expected_json_contract(
                "/v1/nodes/{id}",
                "get",
                "read",
                None,
                "200",
                "NodeV1",
                &["200", "401", "403", "404", "408", "500", "503"],
            ),
            expected_json_contract(
                "/v1/nodes/{id}/inbound",
                "get",
                "read",
                None,
                "200",
                "NodePageV1",
                &["200", "400", "401", "403", "404", "408", "500", "503"],
            ),
            expected_json_contract(
                "/v1/nodes/{id}/outbound",
                "get",
                "read",
                None,
                "200",
                "NodePageV1",
                &["200", "400", "401", "403", "404", "408", "500", "503"],
            ),
        ];
        for contract in contracts {
            assert_json_operation(&api, &contract);
        }

        assert_required_header(&api, "/v1/settings", "put", "If-Match");
        assert_required_header(&api, "/v1/notes/{id}", "put", "If-Match");
        assert_required_header(&api, "/v1/notes/{id}", "delete", "If-Match");
        assert_required_header(&api, "/v1/notes/{id}/rename", "post", "If-Match");
        assert_optional_header(&api, "/v1/assets/{digest}/{name}", "put", "If-Match");
        assert_required_header(&api, "/v1/assets/{digest}/{name}", "delete", "If-Match");
        assert_required_header(&api, "/v1/themata/{id}", "put", "If-Match");
        assert_required_header(&api, "/v1/themata/{id}/archive", "post", "If-Match");
        assert_required_header(&api, "/v1/themata/{id}/unarchive", "post", "If-Match");
        assert_eq!(
            operation_parameter(&api["paths"]["/v1/settings"]["put"], "If-Match")["schema"]["pattern"],
            "^(?:[0-9]+|\"[0-9]+\")$"
        );

        let changes = &api["paths"]["/v1/changes"]["get"];
        let after = operation_parameter(changes, "after");
        assert_eq!(after["schema"]["pattern"], "^[0-9]+$");
        assert_eq!(after["schema"]["default"], "0");
        let limit = operation_parameter(changes, "limit");
        assert_eq!(limit["schema"]["maximum"], 500);
        assert_eq!(limit["schema"]["default"], 100);

        let revision = &api["paths"]["/v1/notes/{id}/revisions/{revision}"]["get"];
        assert_eq!(
            revision["responses"]["200"]["content"]["text/org"]["schema"]["type"],
            "string"
        );
        assert_eq!(
            revision["responses"]["200"]["headers"]["ETag"]["schema"]["type"],
            "string"
        );
        assert_eq!(
            revision["responses"]["410"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/DocumentRevisionSummaryV1"
        );
        let note = &api["paths"]["/v1/notes/{id}"]["get"]["responses"]["200"];
        assert_eq!(note["content"]["text/org"]["schema"]["type"], "string");
        assert_eq!(note["headers"]["ETag"]["schema"]["type"], "string");
        assert_binary_request(&api, "/v1/sync/blobs/{digest}");
        assert_binary_request(&api, "/v1/assets/{digest}/{name}");
        assert_binary_response(&api, "/v1/assets/{digest}/{name}");
        assert_binary_response(&api, "/v1/blobs/{digest}");
        assert_eq!(
            api["paths"]["/v1/events"]["get"]["responses"]["200"]["content"]["text/event-stream"]["schema"]
                ["type"],
            "string"
        );
        let schemas = &api["components"]["schemas"];
        assert_eq!(
            schemas["ThemaWriteRequestV1"]["properties"]["kind"]["pattern"],
            "^(basic|cloze|mcq|mc-cloze)$"
        );
        assert_eq!(
            schemas["SchedulerParametersInputV1"]["properties"]["scheduler_version"]["default"],
            0
        );
        assert_eq!(
            schemas["ContentChangePageV1"]["properties"]["items"]["maxItems"],
            500
        );
        let thema_required = schemas["ThemaWriteRequestV1"]["required"]
            .as_array()
            .unwrap();
        for optional in [
            "hypothesis",
            "source_guid",
            "parathema",
            "review_image",
            "tags",
            "node_ids",
            "asset_paths",
        ] {
            assert!(!thema_required.contains(&json!(optional)), "{optional}");
        }
        assert!(
            schemas["TagRuleInputV1"]
                .get("required")
                .is_none_or(|required| required.as_array().is_some_and(Vec::is_empty))
        );
        assert_raw_content_response_statuses(&api);
        assert_local_refs_resolve(&api, &api);
    }

    struct ExpectedJsonContract<'a> {
        path: &'a str,
        method: &'a str,
        scope: &'a str,
        request: Option<&'a str>,
        success_status: &'a str,
        response: &'a str,
        statuses: &'a [&'a str],
    }

    const fn expected_json_contract<'a>(
        path: &'a str,
        method: &'a str,
        scope: &'a str,
        request: Option<&'a str>,
        success_status: &'a str,
        response: &'a str,
        statuses: &'a [&'a str],
    ) -> ExpectedJsonContract<'a> {
        ExpectedJsonContract {
            path,
            method,
            scope,
            request,
            success_status,
            response,
            statuses,
        }
    }

    fn assert_json_operation(api: &serde_json::Value, contract: &ExpectedJsonContract<'_>) {
        let operation = &api["paths"][contract.path][contract.method];
        assert_eq!(
            operation["x-gnosis-required-scopes"],
            json!([contract.scope]),
            "{} {}",
            contract.method,
            contract.path
        );
        if let Some(request) = contract.request {
            assert_eq!(
                operation["requestBody"]["content"]["application/json"]["schema"]["$ref"],
                format!("#/components/schemas/{request}"),
                "{} {}",
                contract.method,
                contract.path
            );
        }
        assert_eq!(
            operation["responses"][contract.success_status]["content"]["application/json"]["schema"]
                ["$ref"],
            format!("#/components/schemas/{}", contract.response),
            "{} {}",
            contract.method,
            contract.path
        );
        assert_eq!(
            response_statuses(operation),
            set(contract.statuses),
            "{} {}",
            contract.method,
            contract.path
        );
    }

    fn assert_required_header(api: &serde_json::Value, path: &str, method: &str, name: &str) {
        let header = operation_parameter(&api["paths"][path][method], name);
        assert_eq!(header["in"], "header");
        assert_eq!(header["required"], true);
    }

    fn assert_optional_header(api: &serde_json::Value, path: &str, method: &str, name: &str) {
        let header = operation_parameter(&api["paths"][path][method], name);
        assert_eq!(header["in"], "header");
        assert_eq!(header["required"], false);
    }

    fn operation_parameter<'a>(
        operation: &'a serde_json::Value,
        name: &str,
    ) -> &'a serde_json::Value {
        operation["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|parameter| parameter["name"] == name)
            .unwrap()
    }

    fn assert_binary_request(api: &serde_json::Value, path: &str) {
        let schema = &api["paths"][path]["put"]["requestBody"]["content"]["*/*"]["schema"];
        assert_eq!(schema, &json!({"type":"string","format":"binary"}));
    }

    fn assert_binary_response(api: &serde_json::Value, path: &str) {
        let schema = &api["paths"][path]["get"]["responses"]["200"]["content"]["*/*"]["schema"];
        assert_eq!(schema, &json!({"type":"string","format":"binary"}));
    }

    fn assert_raw_content_response_statuses(api: &serde_json::Value) {
        let contracts: &[(&str, &str, &[&str])] = &[
            (
                "/v1/notes/{id}",
                "get",
                &["200", "400", "401", "403", "404", "408", "500", "503"],
            ),
            (
                "/v1/notes/{id}/revisions/{revision}",
                "get",
                &[
                    "200", "400", "401", "403", "404", "408", "410", "500", "503",
                ],
            ),
            (
                "/v1/assets/{digest}/{name}",
                "get",
                &["200", "401", "403", "404", "408", "422", "500", "503"],
            ),
            (
                "/v1/blobs/{digest}",
                "get",
                &["200", "400", "401", "403", "404", "408", "500", "503"],
            ),
            (
                "/v1/events",
                "get",
                &["200", "401", "403", "408", "500", "503"],
            ),
        ];
        for (path, method, statuses) in contracts {
            assert_eq!(
                response_statuses(&api["paths"][path][method]),
                set(statuses),
                "{method} {path}"
            );
        }
    }

    fn response_statuses(operation: &serde_json::Value) -> BTreeSet<&str> {
        operation["responses"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect()
    }

    fn set<'a>(values: &[&'a str]) -> BTreeSet<&'a str> {
        values.iter().copied().collect()
    }

    fn assert_local_refs_resolve(root: &serde_json::Value, value: &serde_json::Value) {
        match value {
            serde_json::Value::Array(values) => {
                for value in values {
                    assert_local_refs_resolve(root, value);
                }
            }
            serde_json::Value::Object(values) => {
                if let Some(reference) = values.get("$ref").and_then(|value| value.as_str())
                    && let Some(pointer) = reference.strip_prefix('#')
                {
                    assert!(root.pointer(pointer).is_some(), "{reference}");
                }
                for value in values.values() {
                    assert_local_refs_resolve(root, value);
                }
            }
            _ => {}
        }
    }

    #[tokio::test]
    async fn data_routes_require_valid_scoped_tokens() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device("test".into(), vec!["read".into()]).unwrap();
        let token = device.token.clone();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let app = router(service.clone());

        let missing = app
            .clone()
            .oneshot(Request::get("/v1/settings").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::UNAUTHORIZED);

        let allowed = app
            .clone()
            .oneshot(authorized("GET", "/v1/settings", &token, Body::empty()))
            .await
            .unwrap();
        assert_eq!(allowed.status(), StatusCode::OK);

        let missing_revisions = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/v1/notes/missing/revisions",
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(missing_revisions.status(), StatusCode::OK);
        assert_eq!(
            response_json(missing_revisions).await,
            json!({"items":[], "next":null})
        );

        let missing_graph = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/v1/nodes/missing/inbound",
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(missing_graph.status(), StatusCode::NOT_FOUND);

        let mut settings = serde_json::to_value(service.settings().await.unwrap()).unwrap();
        settings.as_object_mut().unwrap().remove("revision");
        let settings = serde_json::to_vec(&settings).unwrap();
        let forbidden = app
            .clone()
            .oneshot(authorized(
                "PUT",
                "/v1/settings",
                &token,
                Body::from(settings),
            ))
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let oversized = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/notes",
                &token,
                Body::from(vec![b'x'; super::MAX_BODY + 1]),
            ))
            .await
            .unwrap();
        assert_eq!(oversized.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(oversized.headers()["content-type"], "application/json");

        let malformed_query = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/v1/changes?limit=invalid",
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(malformed_query.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            malformed_query.headers()["content-type"],
            "application/json"
        );

        service.revoke_device(device_id, 2).await.unwrap();
        let revoked = app
            .clone()
            .oneshot(authorized("GET", "/v1/settings", &token, Body::empty()))
            .await
            .unwrap();
        assert_eq!(revoked.status(), StatusCode::UNAUTHORIZED);

        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn asset_http_preserves_media_type_and_rejects_noncanonical_paths() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device =
            generate_device("asset".into(), vec!["read".into(), "content:write".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let app = router(service.clone());
        let bytes = b"png bytes";
        let digest = format!("{:x}", Sha256::digest(bytes));
        let path = format!("/v1/assets/{digest}/image.png");

        let created = app
            .clone()
            .oneshot(authorized_with_type(
                "PUT",
                &path,
                &token,
                "image/png",
                Body::from(bytes.to_vec()),
            ))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);

        let fetched = app
            .clone()
            .oneshot(authorized("GET", &path, &token, Body::empty()))
            .await
            .unwrap();
        assert_eq!(fetched.status(), StatusCode::OK);
        assert_eq!(fetched.headers()["content-type"], "image/png");
        assert_eq!(
            to_bytes(fetched.into_body(), usize::MAX).await.unwrap(),
            bytes.as_slice()
        );

        let invalid = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/v1/assets/not-a-digest/image.png",
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            response_json(invalid).await["error"]["code"],
            "invalid_content"
        );

        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn desired_attempt_operations_retry_and_recover_by_revision() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let (token, device_id, attempt_id) = online_attempt(&service).await;
        let app = router(service.clone());
        let uri = format!("/v1/review/attempts/{attempt_id}/accepted-success");
        let body = json!({
            "version": 1,
            "operation_id": "http-operation",
            "expected_attempt_revision": 1,
            "accepted_success": false
        });

        let first = app
            .clone()
            .oneshot(authorized(
                "PUT",
                &uri,
                &token,
                Body::from(serde_json::to_vec(&body).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first = response_json(first).await;
        assert_eq!(first["version"], 1);
        assert_eq!(first["operation_id"], "http-operation");
        assert_eq!(first["attempt_revision"], 2);
        assert_eq!(first["attempt"]["accepted_success"], false);
        assert_eq!(first["attempt"]["override_count"], 1);

        let replay = app
            .clone()
            .oneshot(authorized(
                "PUT",
                &uri,
                &token,
                Body::from(serde_json::to_vec(&body).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response_json(replay).await, first);

        let stale = app
            .clone()
            .oneshot(authorized(
                "PUT",
                &format!("/v1/review/attempts/{attempt_id}/suspension"),
                &token,
                Body::from(
                    serde_json::to_vec(&json!({
                        "version": 1,
                        "operation_id": "stale-operation",
                        "expected_attempt_revision": 1,
                        "desired_suspended": true
                    }))
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::PRECONDITION_FAILED);
        let stale = response_json(stale).await;
        assert_eq!(stale["error"]["code"], "revision_conflict");
        assert_eq!(stale["error"]["meta"]["expected_revision"], 1);
        assert_eq!(stale["error"]["meta"]["current_revision"], 2);

        let unrepresentable = app
            .clone()
            .oneshot(authorized(
                "PUT",
                &format!("/v1/review/attempts/{attempt_id}/suspension"),
                &token,
                Body::from(
                    serde_json::to_vec(&json!({
                        "version": 1,
                        "operation_id": "unrepresentable",
                        "expected_attempt_revision": u64::MAX,
                        "desired_suspended": true
                    }))
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(unrepresentable.status(), StatusCode::BAD_REQUEST);

        let invalid_id = app
            .clone()
            .oneshot(authorized(
                "PUT",
                &format!("/v1/review/attempts/{attempt_id}/suspension"),
                &token,
                Body::from(
                    serde_json::to_vec(&json!({
                        "version": 1,
                        "operation_id": "\0",
                        "expected_attempt_revision": 2,
                        "desired_suspended": true
                    }))
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(invalid_id.status(), StatusCode::BAD_REQUEST);

        let state = app
            .clone()
            .oneshot(authorized(
                "GET",
                &format!("/v1/review/attempts/{attempt_id}"),
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(state.status(), StatusCode::OK);
        assert_eq!(response_json(state).await["attempt_revision"], 2);

        let other = generate_device("other".into(), vec!["review".into()]).unwrap();
        let other_token = other.token.clone();
        service.create_device(other, 4).await.unwrap();
        let wrong_owner_state = app
            .clone()
            .oneshot(authorized(
                "GET",
                &format!("/v1/review/attempts/{attempt_id}"),
                &other_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(wrong_owner_state.status(), StatusCode::FORBIDDEN);
        let reused = app
            .clone()
            .oneshot(authorized(
                "PUT",
                &uri,
                &other_token,
                Body::from(serde_json::to_vec(&body).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(reused.status(), StatusCode::FORBIDDEN);

        let admin = generate_device("admin".into(), vec!["admin".into()]).unwrap();
        let admin_token = admin.token.clone();
        service.create_device(admin, 5).await.unwrap();
        let independent = app
            .clone()
            .oneshot(authorized(
                "PUT",
                &uri,
                &admin_token,
                Body::from(
                    serde_json::to_vec(&json!({
                        "version": 1,
                        "operation_id": "http-operation",
                        "expected_attempt_revision": 2,
                        "accepted_success": false
                    }))
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(independent.status(), StatusCode::OK);
        assert_eq!(response_json(independent).await["attempt_revision"], 3);

        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
        assert!(!device_id.is_empty());
    }

    async fn online_attempt(service: &crate::service::ServiceHandle) -> (String, String, String) {
        let device = generate_device("reviewer".into(), vec!["review".into()]).unwrap();
        let token = device.token.clone();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        service
            .put_thema(
                prepare_thema(
                    Some("thema".into()),
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
                    vec![],
                    vec![],
                    None,
                )
                .unwrap(),
                20260716,
                1,
                None,
            )
            .await
            .unwrap();
        let session = service
            .create_review_session(
                ReviewSource {
                    kind: ReviewSourceKind::Manual,
                    tags: vec![],
                    exclude_tags: vec![],
                    topic_node_id: None,
                    topic_forward_depth: None,
                    topic_backward_depth: None,
                    manual_ids: vec!["thema".into()],
                },
                None,
                Some(device_id.clone()),
                2,
            )
            .await
            .unwrap();
        let attempt = service
            .begin_review_attempt(session.session_id, Some(device_id.clone()), 3)
            .await
            .unwrap();
        service
            .grade_review_attempt(
                attempt.attempt_id.clone(),
                Some(device_id.clone()),
                AnswerInput::Text("answer".into()),
                4,
            )
            .await
            .unwrap();
        (token, device_id, attempt.attempt_id)
    }

    #[tokio::test]
    async fn missing_note_assets_are_invalid_content() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device("writer".into(), vec!["content:write".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let asset = format!("assets/{}/missing.png", "a".repeat(64));
        let body = serde_json::to_vec(&json!({
            "logical_path": "note.org",
            "org": format!("#+title: Missing\n[[file:{asset}]]\n")
        }))
        .unwrap();

        let response = router(service.clone())
            .oneshot(authorized("POST", "/v1/notes", &token, Body::from(body)))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "invalid_content"
        );
        assert_eq!(service.latest_content_cursor().await.unwrap(), 0);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn offline_content_http_is_scoped_idempotent_and_preserves_invalid_org() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device(
            "content".into(),
            vec!["read".into(), "content:write".into()],
        )
        .unwrap();
        let device_id = device.device_id.clone();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let app = router(service.clone());
        let state: ReplicaStateV1 = response_typed(
            app.clone()
                .oneshot(authorized("GET", "/v1/sync/state", &token, Body::empty()))
                .await
                .unwrap(),
        )
        .await;
        let accepted_body = content_operation_json(
            "content-1",
            &device_id,
            1,
            &state.cursor,
            "document",
            "#+title: Offline\n* Heading\n",
        );
        let accepted = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/content/offline-operations",
                &token,
                Body::from(serde_json::to_vec(&accepted_body).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        let accepted = response_json(accepted).await;
        assert_eq!(accepted["result"]["status"], "accepted");
        assert_eq!(
            accepted["result"]["authoritative"]["document_id"],
            "document"
        );
        assert_eq!(accepted["result"]["authoritative"]["revision"], 1);

        let replay = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/content/offline-operations",
                &token,
                Body::from(serde_json::to_vec(&accepted_body).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response_json(replay).await, accepted);

        let state: ReplicaStateV1 = response_typed(
            app.clone()
                .oneshot(authorized("GET", "/v1/sync/state", &token, Body::empty()))
                .await
                .unwrap(),
        )
        .await;
        let invalid_body = content_operation_json(
            "content-2",
            &device_id,
            2,
            &state.cursor,
            "invalid-document",
            "* [[id:broken\n",
        );
        let invalid = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/content/offline-operations",
                &token,
                Body::from(serde_json::to_vec(&invalid_body).unwrap()),
            ))
            .await
            .unwrap();
        let invalid = response_json(invalid).await;
        assert_eq!(invalid["result"]["status"], "conflict");
        assert_eq!(invalid["result"]["conflict_reason"], "invalid_content");
        assert!(invalid["result"]["authoritative"].is_null());

        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn offline_asset_http_stages_before_sequenced_reconciliation() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device =
            generate_device("asset".into(), vec!["read".into(), "content:write".into()]).unwrap();
        let device_id = device.device_id.clone();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let state = service.replica_state(device_id.clone()).await.unwrap();
        let bytes = b"offline image";
        let digest = format!("{:x}", Sha256::digest(bytes));
        let operation = OfflineAssetOperation {
            operation_id: "asset-1".into(),
            device_id,
            device_sequence: 1,
            cursor_server_id: state.server_id,
            cursor: state.cursor,
            cursor_token: state.cursor_token,
            authored_at_us: 10,
            logical_path: format!("assets/{digest}/image.png"),
            original_name: "image.png".into(),
            content_sha256: digest.clone(),
            size_bytes: bytes.len() as u64,
            media_type: "image/png".into(),
        };
        let body = canonical_asset_operation(&operation).unwrap();
        let app = router(service.clone());

        let missing = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/content/offline-asset-operations",
                &token,
                Body::from(body.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::CONFLICT);
        assert_eq!(
            response_json(missing).await["error"]["code"],
            "blob_required"
        );

        let upload = authorized_with_type(
            "PUT",
            &format!("/v1/sync/blobs/{digest}"),
            &token,
            "image/png",
            Body::from(bytes.to_vec()),
        );
        assert_eq!(
            app.clone().oneshot(upload).await.unwrap().status(),
            StatusCode::CREATED
        );
        let exact_retry = authorized_with_type(
            "PUT",
            &format!("/v1/sync/blobs/{digest}"),
            &token,
            "image/png",
            Body::from(bytes.to_vec()),
        );
        assert_eq!(
            app.clone().oneshot(exact_retry).await.unwrap().status(),
            StatusCode::CREATED
        );
        let alternate_media_retry = authorized_with_type(
            "PUT",
            &format!("/v1/sync/blobs/{digest}"),
            &token,
            "application/octet-stream",
            Body::from(bytes.to_vec()),
        );
        assert_eq!(
            app.clone()
                .oneshot(alternate_media_retry)
                .await
                .unwrap()
                .status(),
            StatusCode::CREATED
        );
        let mismatch = authorized_with_type(
            "PUT",
            &format!("/v1/sync/blobs/{}", "a".repeat(64)),
            &token,
            "image/png",
            Body::from(bytes.to_vec()),
        );
        assert_eq!(
            app.clone().oneshot(mismatch).await.unwrap().status(),
            StatusCode::BAD_REQUEST
        );
        let oversized = authorized_with_type(
            "PUT",
            &format!("/v1/sync/blobs/{digest}"),
            &token,
            "image/png",
            Body::from(vec![0; super::MAX_BODY + 1]),
        );
        assert_eq!(
            app.clone().oneshot(oversized).await.unwrap().status(),
            StatusCode::PAYLOAD_TOO_LARGE
        );

        let accepted = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/content/offline-asset-operations",
                &token,
                Body::from(body.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        let accepted = response_json(accepted).await;
        assert_eq!(accepted["result"]["status"], "accepted");
        assert_eq!(
            accepted["result"]["authoritative"]["content_sha256"],
            digest
        );
        let replay = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/content/offline-asset-operations",
                &token,
                Body::from(body),
            ))
            .await
            .unwrap();
        assert_eq!(response_json(replay).await, accepted);

        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn replica_http_snapshot_delta_and_blob_contract() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let owner = generate_device(
            "owner".into(),
            vec![
                "read".into(),
                "content:write".into(),
                "settings:write".into(),
            ],
        )
        .unwrap();
        let other = generate_device("other".into(), vec!["read".into()]).unwrap();
        let owner_id = owner.device_id.clone();
        let owner_token = owner.token.clone();
        let other_token = other.token.clone();
        service.create_device(owner, 1).await.unwrap();
        service.create_device(other, 2).await.unwrap();
        let app = router(service.clone());

        let note = json!({
            "logical_path": "note.org",
            "org": "#+title: Note\n\n* Heading\n:PROPERTIES:\n:ID: node\n:END:\nBody\n"
        });
        let created = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/notes",
                &owner_token,
                Body::from(serde_json::to_vec(&note).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);

        let state: ReplicaStateV1 = response_typed(
            app.clone()
                .oneshot(authorized(
                    "GET",
                    "/v1/sync/state",
                    &owner_token,
                    Body::empty(),
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(state.version, 1);
        assert!(state.snapshot_required);
        let server_id = state.server_id.clone();
        let capabilities: ContentCapabilitiesV1 = response_typed(
            app.clone()
                .oneshot(authorized(
                    "GET",
                    "/v1/sync/content-capabilities",
                    &owner_token,
                    Body::empty(),
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(capabilities.version, 1);
        assert_eq!(capabilities.server_id, server_id);
        assert_eq!(capabilities.document_lifecycle, 1);

        let first_request = app.clone().oneshot(authorized(
            "GET",
            "/v1/sync/snapshot?limit=1",
            &owner_token,
            Body::empty(),
        ));
        let retry_request = app.clone().oneshot(authorized(
            "GET",
            "/v1/sync/snapshot?limit=1",
            &owner_token,
            Body::empty(),
        ));
        let (first, retry) = tokio::join!(first_request, retry_request);
        let first = response_json(first.unwrap()).await;
        let retry = response_json(retry.unwrap()).await;
        assert_eq!(retry["snapshot_id"], first["snapshot_id"]);
        assert_eq!(first["items"][0]["entity_kind"], "document");
        assert!(first["cursor"].is_null());
        let mut next = first["next"].as_str().unwrap().to_owned();

        let denied = app
            .clone()
            .oneshot(authorized(
                "GET",
                &format!("/v1/sync/snapshot?cursor={next}&limit=1"),
                &other_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        let settings = service.settings().await.unwrap();
        service
            .update_settings(
                SettingsInput {
                    timezone: "UTC".into(),
                    day_boundary_hour: settings.day_boundary_hour,
                    scheduler: settings.scheduler,
                    tag_rules: settings.tag_rules,
                },
                settings.revision,
                3,
                Some(owner_id),
            )
            .await
            .unwrap();

        let mut snapshot_settings = None;
        let mut snapshot_cursor = None;
        loop {
            let page = response_json(
                app.clone()
                    .oneshot(authorized(
                        "GET",
                        &format!("/v1/sync/snapshot?cursor={next}&limit=1"),
                        &owner_token,
                        Body::empty(),
                    ))
                    .await
                    .unwrap(),
            )
            .await;
            if page["items"][0]["entity_kind"] == "settings" {
                snapshot_settings = Some(page["items"][0]["projection"].clone());
            }
            if let Some(cursor) = page["cursor"].as_str() {
                snapshot_cursor = Some(cursor.to_owned());
            }
            let Some(value) = page["next"].as_str() else {
                break;
            };
            next = value.to_owned();
        }
        let snapshot_settings = snapshot_settings.unwrap();
        let snapshot_cursor = snapshot_cursor.unwrap();
        assert_eq!(snapshot_settings["revision"], 1);
        assert_eq!(snapshot_settings["timezone"], "UTC");

        let delta = response_json(
            app.clone()
                .oneshot(authorized(
                    "GET",
                    &format!("/v1/sync/changes?cursor={snapshot_cursor}&limit=10"),
                    &owner_token,
                    Body::empty(),
                ))
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(delta["items"][0]["entity_kind"], "settings");
        assert_eq!(delta["items"][0]["projection"]["revision"], 2);
        assert_eq!(delta["items"][0]["projection"]["timezone"], "UTC");

        let (_, position, token) = decode_cursor(&state.cursor).unwrap();
        let ahead = encode_cursor(&server_id, position + 100, &token).unwrap();
        let response = app
            .clone()
            .oneshot(authorized(
                "GET",
                &format!("/v1/sync/changes?cursor={ahead}"),
                &owner_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let error: ApiErrorResponseV1 = response_typed(response).await;
        assert_eq!(error.error.code(), ApiErrorCodeV1::SnapshotRequired);

        let bytes = b"shared asset".to_vec();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let path = super::canonical_asset_path(&digest, "asset.png").unwrap();
        let head = service
            .put_asset(
                prepare_asset(path.clone(), "asset.png".into(), None).unwrap(),
                bytes.clone(),
                "image/png".into(),
                4,
                None,
            )
            .await
            .unwrap();
        service
            .tombstone_asset(path, head.revision, 5, None)
            .await
            .unwrap();
        let blob = app
            .clone()
            .oneshot(authorized(
                "GET",
                &format!("/v1/blobs/{digest}"),
                &owner_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(blob.status(), StatusCode::OK);
        assert_eq!(
            blob.headers()["cache-control"],
            "private, max-age=31536000, immutable"
        );
        assert_eq!(to_bytes(blob.into_body(), usize::MAX).await.unwrap(), bytes);

        let before = service.latest_content_cursor().await.unwrap();
        let invalid = json!({"logical_path":"invalid.org","org":"not org"});
        let response = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/notes",
                &owner_token,
                Body::from(serde_json::to_vec(&invalid).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(service.latest_content_cursor().await.unwrap(), before);

        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn replica_http_reports_an_unsyncable_projection() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let mut store = SqliteStore::open(&database).unwrap();
        let projection = DocumentProjection::new(DocumentProjectionInput {
            title: "x".repeat(MAX_REPLICA_PAGE_BYTES),
            file_node_id: None,
            file_tags: vec![],
            nodes: vec![],
            link_occurrences: vec![],
            compatibility_sha1: "a".repeat(40),
            content_sha256: "b".repeat(64),
        })
        .unwrap();
        store
            .replace_document(&DocumentId::new("oversized").unwrap(), &projection, None)
            .unwrap();
        drop(store);

        let runtime = ServiceRuntime::start(database, directory.path().join("blobs"), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let device = generate_device("replica".into(), vec!["read".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let response = router(service.clone())
            .oneshot(authorized(
                "GET",
                "/v1/sync/snapshot?limit=1",
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "replica_projection_too_large"
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn offline_review_http_flow_is_versioned_owned_and_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let owner = generate_device("owner".into(), vec!["read".into(), "review".into()]).unwrap();
        let other = generate_device("other".into(), vec!["review".into()]).unwrap();
        let admin = generate_device("admin".into(), vec!["admin".into(), "review".into()]).unwrap();
        let owner_id = owner.device_id.clone();
        let owner_token = owner.token.clone();
        let other_token = other.token.clone();
        let admin_token = admin.token.clone();
        service.create_device(owner, 1).await.unwrap();
        service.create_device(other, 2).await.unwrap();
        service.create_device(admin, 3).await.unwrap();
        let image = b"offline image".to_vec();
        let digest = format!("{:x}", Sha256::digest(&image));
        let asset_path = super::canonical_asset_path(&digest, "image.png").unwrap();
        let asset_head = service
            .put_asset(
                prepare_asset(asset_path.clone(), "image.png".into(), None).unwrap(),
                image.clone(),
                "image/png".into(),
                4,
                None,
            )
            .await
            .unwrap();
        let content = thema_content(
            "basic",
            "Question".into(),
            vec![None],
            vec!["answer".into()],
            None,
            None,
            Some(asset_path.clone()),
        )
        .unwrap();
        let thema = service
            .put_thema(
                prepare_thema(
                    Some("thema".into()),
                    content,
                    vec![],
                    vec![],
                    vec![asset_path.clone()],
                    None,
                )
                .unwrap(),
                20260715,
                5,
                None,
            )
            .await
            .unwrap();
        let app = router(service.clone());
        let pack_body = json!({
            "source": {
                "kind": "manual",
                "tags": [],
                "exclude_tags": [],
                "topic_node_id": null,
                "topic_forward_depth": null,
                "topic_backward_depth": null,
                "manual_ids": ["thema"]
            },
            "target_count": 1
        });
        let created = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/packs",
                &owner_token,
                Body::from(serde_json::to_vec(&pack_body).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let created = response_json(created).await;
        assert_eq!(created["version"], 2);
        assert!(created["pack"]["cursor"].is_string());
        let pack = service
            .review_pack(
                created["pack"]["pack_id"].as_str().unwrap().to_owned(),
                Some(owner_id.clone()),
            )
            .await
            .unwrap()
            .unwrap();
        let active = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/v1/review/packs/active",
                &owner_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response_json(active).await["pack"]["pack_id"], pack.pack_id);
        let denied = app
            .clone()
            .oneshot(authorized(
                "GET",
                &format!("/v1/review/packs/{}", pack.pack_id),
                &other_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::FORBIDDEN);

        let asset = &pack.assets[0];
        let response = app
            .clone()
            .oneshot(authorized(
                "GET",
                &format!(
                    "/v1/review/packs/{}/assets/{}/image.png",
                    pack.pack_id, asset.content_sha256
                ),
                &owner_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX).await.unwrap(),
            image
        );
        let denied_asset = app
            .clone()
            .oneshot(authorized(
                "GET",
                &format!(
                    "/v1/review/packs/{}/assets/{}/image.png",
                    pack.pack_id, asset.content_sha256
                ),
                &other_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(denied_asset.status(), StatusCode::FORBIDDEN);

        let operation = offline_operation(&pack, "attempt", 1);
        let submission = json!({"version": 1, "operation": operation});
        let body = serde_json::to_vec(&submission).unwrap();
        let accepted = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/offline-operations",
                &owner_token,
                Body::from(body.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::OK);
        let accepted = response_json(accepted).await;
        assert_eq!(accepted["version"], 2);
        assert_eq!(accepted["result"]["status"], "accepted");
        assert!(accepted["result"]["cursor"].is_string());
        let changes = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/v1/changes",
                &owner_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(changes.status(), StatusCode::OK);
        assert!(
            response_json(changes).await["items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|change| change["operation"] == "commit")
        );
        let replay = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/offline-operations",
                &owner_token,
                Body::from(body),
            ))
            .await
            .unwrap();
        assert_eq!(response_json(replay).await, accepted);
        let mut changed = submission.clone();
        changed["operation"]["desired_suspended"] = json!(true);
        let conflict = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/offline-operations",
                &owner_token,
                Body::from(serde_json::to_vec(&changed).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);
        let mut unknown = submission;
        unknown["operation"]["presentation"]["future"] = json!(true);
        let invalid = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/offline-operations",
                &owner_token,
                Body::from(serde_json::to_vec(&unknown).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);

        let large_invalid = json!({
            "version": 1,
            "operation": {"padding": "x".repeat(super::MAX_BODY + 1)}
        });
        let response = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/offline-operations",
                &owner_token,
                Body::from(serde_json::to_vec(&large_invalid).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);

        service
            .archive_thema("thema".into(), thema.revision, true, 6, None)
            .await
            .unwrap();
        let tombstone = service
            .tombstone_asset(asset_path.clone(), asset_head.revision, 7, None)
            .await
            .unwrap();
        let retained = app
            .clone()
            .oneshot(authorized(
                "GET",
                &format!(
                    "/v1/review/packs/{}/assets/{}/image.png",
                    pack.pack_id, asset.content_sha256
                ),
                &owner_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(retained.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(retained.into_body(), usize::MAX).await.unwrap(),
            image
        );

        service
            .put_asset(
                prepare_asset(
                    asset_path.clone(),
                    "image.png".into(),
                    Some(tombstone.revision),
                )
                .unwrap(),
                image,
                "image/png".into(),
                8,
                None,
            )
            .await
            .unwrap();
        service
            .archive_thema("thema".into(), thema.revision + 1, false, 9, None)
            .await
            .unwrap();
        let stranded = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/packs",
                &owner_token,
                Body::from(serde_json::to_vec(&pack_body).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(stranded.status(), StatusCode::CREATED);
        let stranded = response_json(stranded).await;
        let stranded_id = stranded["pack"]["pack_id"].as_str().unwrap();
        service.revoke_device(owner_id, 10).await.unwrap();

        let discovered = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/v1/review/packs/active",
                &admin_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(
            response_json(discovered).await["pack"]["pack_id"],
            stranded_id
        );
        let abandoned = app
            .clone()
            .oneshot(authorized(
                "POST",
                &format!("/v1/review/packs/{stranded_id}/abandon"),
                &admin_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(
            response_json(abandoned).await["pack"]["status"],
            "abandoned"
        );
        let replacement = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/packs",
                &admin_token,
                Body::from(serde_json::to_vec(&pack_body).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(replacement.status(), StatusCode::CREATED);

        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[test]
    fn pending_attempts_do_not_reveal_captured_answers() {
        let mut attempt = ReviewAttemptView {
            attempt_id: "attempt".into(),
            session_id: "session".into(),
            thema_id: "thema".into(),
            presentation: PresentationSnapshot {
                kind: "basic".into(),
                keimenon: "Question".into(),
                hypothesis: vec![],
                answer: vec!["secret".into()],
                source_guid: None,
                parathema: None,
                review_image: None,
                tags: vec![],
            },
            matcher_version: 1,
            response: None,
            detected_success: None,
            accepted_success: None,
            override_count: 0,
            proposed_transition: None,
            desired_suspended: false,
            status: "pending".into(),
        };
        let pending = attempt_json(&attempt);
        assert!(pending["revealed_answer"].is_null());
        assert!(pending["presentation"].get("answer").is_none());
        assert_eq!(
            serde_json::to_value(ReviewAttemptV1::from_application(&attempt).unwrap()).unwrap(),
            pending
        );
        attempt.response = Some(AnswerInput::Text("answer".into()));
        assert_eq!(attempt_json(&attempt)["revealed_answer"], json!(["secret"]));
        assert_eq!(
            serde_json::to_value(ReviewAttemptV1::from_application(&attempt).unwrap()).unwrap(),
            attempt_json(&attempt)
        );
    }

    fn authorized(method: &str, uri: &str, token: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(body)
            .unwrap()
    }

    fn authorized_with_type(
        method: &str,
        uri: &str,
        token: &str,
        media_type: &str,
        body: Body,
    ) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", media_type)
            .body(body)
            .unwrap()
    }

    fn content_operation_json(
        operation_id: &str,
        device_id: &str,
        device_sequence: u64,
        cursor: &str,
        document_id: &str,
        org: &str,
    ) -> serde_json::Value {
        json!({
            "version": 1,
            "operation": {
                "operation_id": operation_id,
                "device_id": device_id,
                "device_sequence": device_sequence,
                "cursor": cursor,
                "authored_at_us": device_sequence + 10,
                "mutation": {
                    "kind": "put_document",
                    "document_id": document_id,
                    "logical_path": format!("{document_id}.org"),
                    "content_sha256": format!("{:x}", Sha256::digest(org.as_bytes())),
                    "org": org,
                    "base": {"kind": "absent"}
                }
            }
        })
    }

    async fn response_json(response: axum::response::Response) -> serde_json::Value {
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    async fn response_typed<T: serde::de::DeserializeOwned>(
        response: axum::response::Response,
    ) -> T {
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
    }

    fn offline_operation(
        pack: &ReviewPackView,
        attempt_id: &str,
        device_sequence: u64,
    ) -> OfflineReviewOperation {
        let item = &pack.items[0];
        let response = AnswerInput::Text("answer".into());
        let detected_success = grade(
            item.matcher_version,
            &presentation_content(&item.presentation).unwrap(),
            &response.clone().into(),
        )
        .unwrap()
        .success;
        let answered_at_us = pack.issued_at_us + 1;
        let review_day =
            review_day_at(answered_at_us, &pack.timezone, pack.day_boundary_hour).unwrap();
        let state = item.review_state.clone().into();
        let parameters = item.parameters.clone().into();
        let probe = compute_transition(
            &state,
            &parameters,
            detected_success,
            review_day,
            answered_at_us,
            0.0,
        )
        .unwrap();
        let delta =
            fuzz_delta(probe.algorithm_interval_days, parameters.interval_fuzz, 0.0).unwrap();
        let transition = compute_transition(
            &state,
            &parameters,
            detected_success,
            review_day,
            answered_at_us,
            delta,
        )
        .unwrap();
        OfflineReviewOperation {
            attempt_id: attempt_id.into(),
            pack_id: pack.pack_id.clone(),
            session_id: pack.session_id.clone(),
            lease_id: pack.lease_id.clone(),
            device_id: pack.device_id.clone(),
            device_sequence,
            session_position: item.position,
            thema_id: item.thema_id.clone(),
            thema_revision: item.thema_revision,
            base_state_revision: item.review_state.state_revision,
            content_sha256: item.content_sha256.clone(),
            presentation: item.presentation.clone(),
            parameters: item.parameters.clone(),
            matcher_version: item.matcher_version,
            response,
            detected_success,
            accepted_success: detected_success,
            override_count: 0,
            proposed_transition: (&transition).into(),
            desired_suspended: false,
            answered_at_us,
            fuzz_sample: 0.0,
        }
    }
}
