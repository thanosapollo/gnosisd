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
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_stream::{StreamExt as _, wrappers::BroadcastStream};
use tower::{ServiceBuilder, limit::ConcurrencyLimitLayer, timeout::TimeoutLayer};

use crate::{
    application::{
        AnswerInput, AssetHead, ContentChange, DevicePrincipal, DeviceScope, DocumentHead,
        DocumentRevision, MAX_REVIEW_PACK_BYTES, NodeRecord, ReviewAttemptView, ReviewSessionView,
        ReviewSource, ServiceError, ServiceHandle, SettingsInput, SettingsRecord, ThemaRecord,
        canonical_asset_path, prepare_asset, prepare_note, prepare_thema, review_day_at,
        thema_content,
    },
    domain::DocumentId,
    protocol::{
        OfflineContentSubmission, OfflineContentWireError, OfflineReviewSubmission,
        OfflineReviewWireError, ReplicaWireError, active_review_pack_json, decode_cursor,
        decode_snapshot_cursor, offline_content_capabilities_json, offline_content_result_json,
        offline_result_json, replica_changes_json, replica_snapshot_json, replica_state_json,
        review_pack_json,
    },
    store::StoreError,
};

const MAX_BODY: usize = 1024 * 1024;
const MAX_OFFLINE_OPERATION_BODY: usize = MAX_REVIEW_PACK_BYTES + 2 * MAX_BODY;
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
        "request_timeout",
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
    ApiError::new(status, "invalid_request", "request could not be processed").into_response()
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
            post(reconcile_offline).layer(DefaultBodyLimit::max(MAX_OFFLINE_OPERATION_BODY)),
        )
        .route("/v1/review/attempts/{id}/answer", post(answer_attempt))
        .route("/v1/review/attempts/{id}/override", post(override_attempt))
        .route(
            "/v1/review/attempts/{id}/suspension",
            post(toggle_suspension),
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
    if token.len() < 40 || token.len() > 128 {
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
) -> Result<Json<Value>, ApiError> {
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
    let next = items.last().map(|item| item.cursor);
    Ok(Json(
        json!({"items": items.iter().map(change_json).collect::<Vec<_>>(), "next": next}),
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
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let replica = state
        .service
        .replica_state(principal.device_id)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        replica_state_json(&replica).map_err(ApiError::replica_wire)?,
    ))
}

async fn content_capabilities(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let replica = state
        .service
        .replica_state(principal.device_id)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        offline_content_capabilities_json(&replica.server_id)
            .map_err(ApiError::offline_content_wire)?,
    ))
}

async fn replica_changes(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<ReplicaChangesQuery>,
) -> Result<Json<Value>, ApiError> {
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
        replica_changes_json(&server_id, after, &after_token, latest, &changes)
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
) -> Result<Json<Value>, ApiError> {
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
        replica_snapshot_json(&page).map_err(ApiError::replica_wire)?,
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

fn change_json(change: &ContentChange) -> Value {
    json!({"cursor":change.cursor,"entity_kind":change.entity_kind,"entity_id":change.entity_id,
        "revision":change.revision,"operation":change.operation,"created_at_us":change.created_at_us})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoteInput {
    logical_path: String,
    org: String,
}

async fn create_note(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(input): ApiJson<NoteInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let prepared = prepare_note(None, input.logical_path, input.org, None)
        .map_err(ApiError::invalid_content)?;
    let head = state
        .service
        .replace_note(prepared, now_us_i64()?, Some(principal.device_id))
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok((StatusCode::CREATED, Json(head_json(&head))))
}

async fn update_note(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<NoteInput>,
) -> Result<Json<Value>, ApiError> {
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
        .map_err(|error| conflict_with(error, id, revision))?;
    announce(&state).await;
    Ok(Json(head_json(&head)))
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RenameInput {
    logical_path: String,
}

async fn rename_note(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<RenameInput>,
) -> Result<Json<Value>, ApiError> {
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
    Ok(Json(head_json(&head)))
}

async fn delete_note(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
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
    Ok(Json(head_json(&head)))
}

fn head_json(head: &DocumentHead) -> Value {
    json!({"document_id":head.document_id,"logical_path":head.logical_path,"revision":head.revision,
        "content_sha256":head.blob_sha256,"active":head.active})
}

async fn note_revisions(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
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
    Ok(Json(json!({
        "items": items.iter().map(revision_json).collect::<Vec<_>>(),
        "next": items.last().map(|item| item.revision)
    })))
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
        return Ok((StatusCode::GONE, Json(revision_json(&record))).into_response());
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

fn revision_json(record: &DocumentRevision) -> Value {
    json!({
        "document_id": record.document_id,
        "revision": record.revision,
        "logical_path": record.logical_path,
        "content_sha256": record.blob_sha256,
        "operation": record.operation,
        "created_at_us": record.created_at_us,
        "author_device_id": record.author_device_id
    })
}

async fn put_asset(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path((digest, name)): Path<(String, String)>,
    headers: HeaderMap,
    ApiBytes(body): ApiBytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
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
    Ok((StatusCode::CREATED, Json(asset_json(&head))))
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
    if digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
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
) -> Result<Json<Value>, ApiError> {
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
    Ok(Json(asset_json(&head)))
}

fn asset_json(head: &AssetHead) -> Value {
    json!({"logical_path":head.logical_path,"revision":head.revision,"content_sha256":head.blob_sha256,"media_type":head.media_type,"active":head.active})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemaInput {
    kind: String,
    keimenon: String,
    #[serde(default)]
    hypothesis: Vec<Option<String>>,
    answer: Vec<String>,
    source_guid: Option<String>,
    parathema: Option<String>,
    review_image: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    node_ids: Vec<String>,
    #[serde(default)]
    asset_paths: Vec<String>,
}

async fn create_thema(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(input): ApiJson<ThemaInput>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
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
    Ok((StatusCode::CREATED, Json(thema_json(&record))))
}

async fn update_thema(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<ThemaInput>,
) -> Result<Json<Value>, ApiError> {
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
    Ok(Json(thema_json(&record)))
}

fn prepared_thema(
    id: Option<String>,
    input: ThemaInput,
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
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let record = state
        .service
        .thema(id)
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(thema_json(&record)))
}

async fn search_themata(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
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
    Ok(Json(
        json!({"items":items.iter().map(thema_json).collect::<Vec<_>>(),"next":items.last().map(|item| &item.thema_id)}),
    ))
}

async fn archive_thema(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    set_archive(state, principal, path, headers, true).await
}
async fn unarchive_thema(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    set_archive(state, principal, path, headers, false).await
}
async fn set_archive(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    archived: bool,
) -> Result<Json<Value>, ApiError> {
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
    Ok(Json(thema_json(&record)))
}

fn thema_json(record: &ThemaRecord) -> Value {
    json!({"thema_id":record.thema_id,"revision":record.revision,"kind":record.content.kind.as_str(),"keimenon":record.content.keimenon,"hypothesis":record.content.hypothesis,"answer":record.content.answer,"source_guid":record.content.source_guid,"parathema":record.content.parathema,"review_image":record.content.review_image,"tags":record.tags,"node_ids":record.node_ids,"asset_paths":record.asset_paths,"content_sha256":record.content_sha256,"archived":record.archived})
}

async fn get_node(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let node = state
        .service
        .node(id)
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(node_json(&node)))
}
async fn search_nodes(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
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
    Ok(Json(
        json!({"items":items.iter().map(node_json).collect::<Vec<_>>(),"next":items.last().map(|n|&n.node_id)}),
    ))
}
async fn inbound_nodes(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
    query: Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    graph(state, principal, path, query, true).await
}
async fn outbound_nodes(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
    query: Query<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    graph(state, principal, path, query, false).await
}
async fn graph(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
    inbound: bool,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let items = state
        .service
        .graph_nodes(id, inbound, page_limit(query.limit)?, query.after)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        json!({"items":items.iter().map(node_json).collect::<Vec<_>>(),"next":items.last().map(|n|&n.node_id)}),
    ))
}
fn node_json(node: &NodeRecord) -> Value {
    json!({"node_id":node.node_id,"document_id":node.document_id,"logical_path":node.logical_path,"title":node.title,"title_path":node.title_path,"tags":node.tags,"level":node.level,"active":node.active,"thema_ids":node.thema_ids})
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
                .and_then(|v| v.trim_matches('"').parse().ok())
                .ok_or_else(|| ApiError::bad_request("invalid If-Match"))
        })
        .transpose()
}

async fn get_settings(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let settings = state.service.settings().await.map_err(ApiError::service)?;
    Ok(Json(
        serde_json::to_value(settings).map_err(|_| ApiError::internal())?,
    ))
}

async fn update_settings(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<SettingsInput>,
) -> Result<Json<Value>, ApiError> {
    require(&principal, DeviceScope::SettingsWrite)?;
    let settings = state
        .service
        .update_settings(
            input,
            if_match(&headers)?,
            now_us_i64()?,
            Some(principal.device_id),
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(
        serde_json::to_value(settings).map_err(|_| ApiError::internal())?,
    ))
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
fn conflict_with(error: ServiceError, _id: String, expected: u64) -> ApiError {
    match error {
        ServiceError::Store(StoreError::Conflict { actual, .. }) => {
            ApiError::conflict(expected, actual)
        }
        other => ApiError::service(other),
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    meta: Option<Value>,
}
impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
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
            "unauthorized",
            "bearer token is missing or invalid",
        )
    }
    fn forbidden() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "forbidden",
            "token scope does not permit this operation",
        )
    }
    fn not_found() -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", "resource not found")
    }
    fn bad_request(message: &'static str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }
    fn precondition(message: &'static str) -> Self {
        Self::new(
            StatusCode::PRECONDITION_REQUIRED,
            "precondition_required",
            message,
        )
    }
    fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "internal server failure",
        )
    }
    fn invalid_content(_error: impl std::fmt::Debug) -> Self {
        Self::new(
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid_content",
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
                "request_too_large",
                "request body exceeds the 1 MiB limit",
            )
        } else {
            Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_body", message)
        }
    }
    fn offline_review_wire(error: OfflineReviewWireError) -> Self {
        match error {
            OfflineReviewWireError::Version => Self::new(
                StatusCode::BAD_REQUEST,
                "unsupported_offline_review_version",
                "offline review protocol version is unsupported",
            ),
            OfflineReviewWireError::Operation => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_offline_review_operation",
                "offline review operation is invalid",
            ),
        }
    }
    fn offline_content_wire(error: OfflineContentWireError) -> Self {
        match error {
            OfflineContentWireError::Version => Self::new(
                StatusCode::BAD_REQUEST,
                "unsupported_offline_content_version",
                "offline content protocol version is unsupported",
            ),
            OfflineContentWireError::Operation => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_offline_content_operation",
                "offline content operation is invalid",
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
                "replica_projection_too_large",
                "stored projection exceeds the replica response budget",
            ),
            ReplicaWireError::Encode | ReplicaWireError::Response => Self::internal(),
        }
    }
    fn snapshot_required() -> Self {
        Self {
            status: StatusCode::CONFLICT,
            code: "snapshot_required",
            message: "replica cursor cannot be resumed from this server state",
            meta: Some(json!({"snapshot_url":"/v1/sync/snapshot"})),
        }
    }
    fn conflict(expected: u64, actual: Option<u64>) -> Self {
        Self {
            status: StatusCode::PRECONDITION_FAILED,
            code: "revision_conflict",
            message: "resource revision changed",
            meta: Some(json!({"expected_revision":expected,"current_revision":actual})),
        }
    }
    fn service(error: ServiceError) -> Self {
        match error {
            ServiceError::Busy => {
                Self::new(StatusCode::SERVICE_UNAVAILABLE, "busy", "server is busy")
            }
            ServiceError::Store(StoreError::NotFound) => Self::not_found(),
            ServiceError::Store(StoreError::Conflict { expected, actual }) => {
                Self::conflict(expected, actual)
            }
            ServiceError::Store(StoreError::ActiveReviewSession) => Self::new(
                StatusCode::CONFLICT,
                "active_session",
                "a review session is already active",
            ),
            ServiceError::Store(StoreError::StaleAttempt) => Self::new(
                StatusCode::PRECONDITION_FAILED,
                "stale_attempt",
                "thema content or review state changed",
            ),
            ServiceError::Store(StoreError::ReviewOwnership) => Self::forbidden(),
            ServiceError::Store(StoreError::IdempotencyConflict) => Self::new(
                StatusCode::CONFLICT,
                "idempotency_conflict",
                "operation identity was reused with different content",
            ),
            ServiceError::Store(StoreError::DeviceSequence { expected }) => Self {
                status: StatusCode::CONFLICT,
                code: "device_sequence_gap",
                message: "device operation sequence is not contiguous",
                meta: Some(json!({"expected_sequence":expected})),
            },
            ServiceError::Store(StoreError::ReplicaCursor | StoreError::ReplicaServer) => {
                Self::snapshot_required()
            }
            ServiceError::Store(StoreError::InvalidReviewState) | ServiceError::Review(_) => {
                Self::bad_request("review operation is invalid")
            }
            ServiceError::Store(StoreError::PathChange) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "rename_required",
                "document path changes require the rename endpoint",
            ),
            ServiceError::Store(StoreError::InvalidProjection { .. }) => {
                Self::invalid_content("stored projection")
            }
            ServiceError::AssetDigest => Self::bad_request("asset digest does not match content"),
            ServiceError::Settings(_) => Self::invalid_content("settings"),
            _ => Self::internal(),
        }
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error":{"code":self.code,"message":self.message,"meta":self.meta}})),
        )
            .into_response()
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
        "info":{"title":"Gnosis API","version":"1.0.0"},
        "paths":paths,
        "components":{
            "securitySchemes":{"bearerAuth":{"type":"http","scheme":"bearer"}},
            "schemas":{"Error":{"type":"object","required":["error"],"properties":{
                "error":{"type":"object","required":["code","message"],"properties":{
                    "code":{"type":"string"},"message":{"type":"string"},"meta":{}
                }}
            }}}
        }
    })
}

fn openapi_operation(path: &str, method: &str) -> Value {
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
    if path.starts_with("/v1/assets/") && method == "put" {
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
        "schema":{"$ref":"#/components/schemas/Error"}
    }}})
}

const OPENAPI_OPERATIONS: &[(&str, &str)] = &[
    ("/v1/changes", "get"),
    ("/v1/sync/state", "get"),
    ("/v1/sync/content-capabilities", "get"),
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
    ("/v1/review/attempts/{id}/answer", "post"),
    ("/v1/review/attempts/{id}/override", "post"),
    ("/v1/review/attempts/{id}/suspension", "post"),
    ("/v1/review/attempts/{id}/commit", "post"),
];

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode},
    };
    use serde_json::json;
    use sha2::{Digest as _, Sha256};
    use tower::ServiceExt;

    use super::{OPENAPI_OPERATIONS, attempt_json, openapi_document, router};
    use crate::application::{
        AnswerInput, OfflineReviewOperation, PresentationSnapshot, ReviewAttemptView,
        ReviewPackView, ServiceRuntime, SettingsInput, generate_device, prepare_asset,
        prepare_thema, presentation_content, review_day_at, thema_content,
    };
    use crate::domain::{
        DocumentId, DocumentProjection, DocumentProjectionInput, compute_transition, fuzz_delta,
        grade,
    };
    use crate::protocol::{MAX_REPLICA_PAGE_BYTES, decode_cursor, encode_cursor};
    use crate::store::SqliteStore;

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
        let state = response_json(
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
            state["cursor"].as_str().unwrap(),
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

        let state = response_json(
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
            state["cursor"].as_str().unwrap(),
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

        let state = response_json(
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
        assert_eq!(state["version"], 1);
        assert_eq!(state["snapshot_required"], true);
        let server_id = state["server_id"].as_str().unwrap();
        let capabilities = response_json(
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
        assert_eq!(capabilities["version"], 1);
        assert_eq!(capabilities["server_id"], server_id);
        assert_eq!(capabilities["document_lifecycle"], 1);

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

        let (_, position, token) = decode_cursor(state["cursor"].as_str().unwrap()).unwrap();
        let ahead = encode_cursor(server_id, position + 100, &token).unwrap();
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
        assert_eq!(
            response_json(response).await["error"]["code"],
            "snapshot_required"
        );

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
        let owner = generate_device("owner".into(), vec!["review".into()]).unwrap();
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
        service
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
        let tombstone = service
            .tombstone_asset(asset_path.clone(), asset_head.revision, 6, None)
            .await
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
            .put_asset(
                prepare_asset(
                    asset_path.clone(),
                    "image.png".into(),
                    Some(tombstone.revision),
                )
                .unwrap(),
                image,
                "image/png".into(),
                7,
                None,
            )
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
        service.revoke_device(owner_id, 8).await.unwrap();

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
        attempt.response = Some(AnswerInput::Text("answer".into()));
        assert_eq!(attempt_json(&attempt)["revealed_answer"], json!(["secret"]));
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
