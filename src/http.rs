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
        DocumentRevision, NodeRecord, ReviewAttemptView, ReviewSessionView, ReviewSource,
        ServiceError, ServiceHandle, SettingsInput, SettingsRecord, ThemaRecord,
        canonical_asset_path, prepare_asset, prepare_note, prepare_thema, review_day_at,
        thema_content,
    },
    domain::DocumentId,
    store::StoreError,
};

const MAX_BODY: usize = 1024 * 1024;
const MAX_PAGE: u16 = 500;

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
        .put_thema(prepared, settings_review_day(&settings)?, now_us_i64()?)
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
        .put_thema(prepared, settings_review_day(&settings)?, now_us_i64()?)
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
        .archive_thema(id, if_match(&headers)?, archived, now_us_i64()?)
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
        .create_review_session(input.source, input.target_count, now_us()?)
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
        .active_review_session()
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
            .resume_review_session(id)
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
            .stop_review_session(id, abort, now_us()?)
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
        .begin_review_attempt(id, now_us()?)
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
            .grade_review_attempt(id, answer, now_us()?)
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
            .override_review_attempt(id)
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
            .toggle_attempt_suspension(id)
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
        .commit_review_attempt(id, now_us()?)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(json!({"event_id":event})))
}
fn session_json(s: &ReviewSessionView) -> Value {
    json!({"session_id":s.session_id,"source":s.source,"target_count":s.target_count,"status":s.status,"current_position":s.current_position,"queue_length":s.queue_length,"started_at_us":s.started_at_us})
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
        .update_settings(input, if_match(&headers)?, now_us_i64()?)
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
    ("/v1/review/attempts/{id}/answer", "post"),
    ("/v1/review/attempts/{id}/override", "post"),
    ("/v1/review/attempts/{id}/suspension", "post"),
    ("/v1/review/attempts/{id}/commit", "post"),
];

#[cfg(test)]
mod tests {
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use serde_json::json;
    use tower::ServiceExt;

    use super::{OPENAPI_OPERATIONS, attempt_json, openapi_document, router};
    use crate::application::{
        AnswerInput, PresentationSnapshot, ReviewAttemptView, ServiceRuntime, generate_device,
    };

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
}
