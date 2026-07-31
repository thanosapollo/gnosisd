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
    routing::{get, post, put},
};
use chrono::NaiveDate;
use schemars::generate::SchemaSettings;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use tokio::sync::{broadcast, watch};
use tokio_stream::{
    StreamExt as _,
    wrappers::{BroadcastStream, WatchStream},
};
use tower::{ServiceBuilder, limit::ConcurrencyLimitLayer, timeout::TimeoutLayer};

use crate::{
    application::{
        ApplicationError, DeviceDesiredStateMutation, DevicePrincipal, DeviceScope, DueCursor,
        MAX_DUE_CURSOR_BYTES, MAX_OFFLINE_ASSET_BYTES, PreparedDeviceDocumentLifecycleMutation,
        PreparedNote, QueryDocument, QueryDueThemataPage, ReviewAttemptMutation,
        ReviewAttemptOperation, ReviewAttemptOperationResult, ReviewAttemptOperationStatus,
        ReviewHistoryQuery, ReviewOperationAuthority, ReviewSessionQuery, SettingsRecord,
        canonical_asset_path, prepare_asset, prepare_device_desired_state_mutation,
        prepare_device_document_mutation, prepare_device_document_rename,
        prepare_device_document_tombstone, prepare_document, prepare_thema, review_day_at,
        thema_content,
    },
    domain::{DocumentId, DueThema, NodeId, ThemaId},
    is_canonical_sha256,
    protocol::{
        DocumentView, DuePageView, MAX_FRAME_SIZE, MAX_OFFLINE_CONTENT_OPERATION_BYTES,
        MAX_OFFLINE_REVIEW_OPERATION_BYTES, OfflineAssetSubmission, OfflineAssetWireError,
        OfflineContentWireError, OfflineReviewSubmission, OfflineReviewWireError, ReplicaWireError,
        active_review_pack_response, decode_cursor, decode_snapshot_cursor,
        http_v1::{
            ActiveReviewPackEnvelopeV2, ActiveReviewSessionV1, ApiErrorBodyV1, ApiErrorCodeV1,
            ApiErrorMetaV1, ApiErrorResponseV1, AssetCapabilitiesV1, AssetHeadV1,
            ContentCapabilitiesV1, ContentChangePageV1, DashboardV1, DesiredStateMutationResultV1,
            DeviceIdentityV1, DocumentHeadV1, DocumentImportRequestV1, DocumentMutationResultV1,
            DocumentRevisionPageV1, DocumentRevisionSummaryV1, NodeCatalogPageV1, NodePageV1,
            NodeThemaAssociationPageV1, NodeV1, NoteRenameRequestV1, NoteWriteRequestV1,
            OfflineAssetOperationEnvelopeV1, OfflineAssetResultEnvelopeV1,
            OfflineContentOperationEnvelopeV1, OfflineContentResultEnvelopeV1,
            OfflineReviewOperationEnvelopeV1, OfflineReviewResultEnvelopeV2, ReplicaChangePageV1,
            ReplicaSnapshotPageV1, ReplicaStateV1, ReviewAnswerV1, ReviewAttemptBeginResponseV1,
            ReviewAttemptOperationResponseV1, ReviewAttemptStateResponseV1, ReviewAttemptV1,
            ReviewClozeCheckRequestV1, ReviewClozeCheckV1, ReviewCommitResponseV1,
            ReviewHistoryPageV1, ReviewPackEnvelopeV2, ReviewQueueSummaryRequestV1,
            ReviewQueueSummaryV1, ReviewSessionModeV1, ReviewSessionPageV1, ReviewSessionRequestV1,
            ReviewSessionStatusV1, ReviewSessionSummaryV1, ReviewSessionV1,
            SetAcceptedSuccessRequestV1, SetSuspensionRequestV1, SetThemaSuspensionRequestV1,
            SettingsV1, StagedBlobV1, ThemaCatalogPageV1, ThemaPageV1, ThemaReviewStateV1, ThemaV1,
            ThemaWriteRequestV1, UpdateSettingsRequestV1, valid_bearer_token,
        },
        offline_asset_capabilities, offline_asset_result_response, offline_content_capabilities,
        offline_content_result_response, offline_result_response, replica_change_page,
        replica_snapshot_page, replica_state_response, review_pack_response,
    },
    service::{ServiceError, ServiceHandle, protocol_logical_path},
    store::StoreError,
};

const MAX_BODY: usize = MAX_OFFLINE_ASSET_BYTES as usize;
const MAX_PAGE: u16 = 500;
const MAX_NODE_CATALOG_PAGE: u16 = 5000;
const MAX_REPLICA_PAGE: u16 = 32;

#[derive(Clone)]
pub(crate) struct HttpState {
    service: ServiceHandle,
    cursors: broadcast::Sender<u64>,
    shutdown: watch::Receiver<bool>,
}

#[cfg(test)]
pub(crate) fn router(service: ServiceHandle) -> Router {
    let (_shutdown, receiver) = watch::channel(false);
    router_with_shutdown(service, receiver)
}

pub(crate) fn router_with_shutdown(
    service: ServiceHandle,
    shutdown: watch::Receiver<bool>,
) -> Router {
    let (cursors, _) = broadcast::channel(64);
    let state = HttpState {
        service,
        cursors,
        shutdown,
    };
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
        .route("/v1/dashboard", get(dashboard))
        .route(
            "/v1/documents/{id}",
            get(get_document_projection).merge(
                put(import_document_projection)
                    .layer(DefaultBodyLimit::max(MAX_OFFLINE_CONTENT_OPERATION_BYTES)),
            ),
        )
        .route("/v1/review/due", get(due_page))
        .route("/v1/changes", get(changes))
        .route("/v1/sync/identity", get(device_identity))
        .route("/v1/sync/state", get(replica_state))
        .route("/v1/sync/content-capabilities", get(content_capabilities))
        .route("/v1/sync/asset-capabilities", get(asset_capabilities))
        .route("/v1/sync/blobs/{digest}", axum::routing::put(stage_blob))
        .route("/v1/sync/changes", get(replica_changes))
        .route("/v1/sync/snapshot", get(replica_snapshot))
        .route("/v1/events", get(events))
        .route("/v1/settings", get(get_settings).put(update_settings))
        .route("/v1/notes/{id}", get(get_note))
        .route(
            "/v1/document-mutations",
            post(create_document_mutation)
                .layer(DefaultBodyLimit::max(MAX_OFFLINE_CONTENT_OPERATION_BYTES)),
        )
        .route(
            "/v1/document-mutations/{id}",
            axum::routing::put(replace_document_mutation)
                .layer(DefaultBodyLimit::max(MAX_OFFLINE_CONTENT_OPERATION_BYTES))
                .delete(tombstone_document_mutation),
        )
        .route(
            "/v1/document-mutations/{id}/rename",
            post(rename_document_mutation),
        )
        .route("/v1/notes/{id}/revisions", get(note_revisions))
        .route("/v1/notes/{id}/revisions/{revision}", get(note_revision))
        .route(
            "/v1/assets/{digest}/{name}",
            get(get_asset).put(put_asset).delete(delete_asset),
        )
        .route("/v1/blobs/{digest}", get(get_blob))
        .route(
            "/v1/content/offline-operations",
            post(reconcile_content)
                .layer(DefaultBodyLimit::max(MAX_OFFLINE_CONTENT_OPERATION_BYTES)),
        )
        .route(
            "/v1/content/offline-asset-operations",
            post(reconcile_asset),
        )
        .route("/v1/themata", post(create_thema).get(search_themata))
        .route("/v1/themata/catalog", get(thema_catalog))
        .route("/v1/themata/{id}", get(get_thema).put(update_thema))
        .route("/v1/themata/{id}/review-state", get(get_thema_review_state))
        .route("/v1/themata/{id}/suspension", put(set_thema_suspension))
        .route("/v1/themata/{id}/archive", post(archive_thema))
        .route("/v1/themata/{id}/unarchive", post(unarchive_thema))
        .route("/v1/nodes", get(search_nodes))
        .route("/v1/nodes/catalog", get(node_catalog))
        .route("/v1/nodes/{id}", get(get_node))
        .route("/v1/nodes/{id}/themata", get(node_themata))
        .route("/v1/nodes/{id}/inbound", get(inbound_nodes))
        .route("/v1/nodes/{id}/outbound", get(outbound_nodes))
        .route(
            "/v1/review/sessions",
            get(review_sessions).post(create_session),
        )
        .route("/v1/review/sessions/active", get(active_session))
        .route("/v1/review/sessions/{id}", get(review_session))
        .route("/v1/review/sessions/{id}/resume", post(resume_session))
        .route("/v1/review/sessions/{id}/quit", post(quit_session))
        .route("/v1/review/sessions/{id}/abort", post(abort_session))
        .route("/v1/review/sessions/{id}/attempts", post(begin_attempt))
        .route("/v1/review/history", get(review_history))
        .route("/v1/review/queue/summary", post(review_queue_summary))
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
        .route(
            "/v1/review/attempts/{id}/cloze-check",
            post(check_cloze_attempt),
        )
        .route("/v1/review/attempts/{id}", get(get_attempt_state))
        .route(
            "/v1/review/attempts/{id}/accepted-success",
            axum::routing::put(set_accepted_success),
        )
        .route(
            "/v1/review/attempts/{id}/suspension",
            axum::routing::put(set_suspension),
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

async fn health(State(state): State<HttpState>) -> Response {
    match state.service.ready().await {
        Ok(()) => (StatusCode::OK, Json(json!({"status":"ready"}))).into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status":"unavailable"})),
        )
            .into_response(),
    }
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

#[derive(Deserialize)]
struct NodeCatalogQuery {
    document_id: Option<String>,
    after: Option<String>,
    limit: Option<u16>,
}

fn node_catalog_limit(value: Option<u16>) -> Result<u16, ApiError> {
    let value = value.unwrap_or(MAX_NODE_CATALOG_PAGE);
    if (1..=MAX_NODE_CATALOG_PAGE).contains(&value) {
        Ok(value)
    } else {
        Err(ApiError::bad_request("invalid node catalog page limit"))
    }
}

#[derive(Deserialize)]
struct DashboardQuery {
    days: Option<u16>,
}

fn dashboard_days(value: Option<u16>) -> Result<u16, ApiError> {
    let value = value.unwrap_or(30);
    if (1..=365).contains(&value) {
        Ok(value)
    } else {
        Err(ApiError::bad_request("invalid dashboard window"))
    }
}

async fn dashboard(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<DashboardQuery>,
) -> Result<Json<DashboardV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    require(&principal, DeviceScope::Review)?;
    let summary = state
        .service
        .dashboard(dashboard_days(query.days)?, now_us()?)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        DashboardV1::from_application(&summary).map_err(|_| ApiError::internal())?,
    ))
}

async fn import_document_projection(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<DocumentImportRequestV1>,
) -> Result<Json<DocumentMutationResultV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let operation_id = idempotency_key(&headers)?;
    let document_id =
        DocumentId::new(id).map_err(|_| ApiError::bad_request("invalid document ID"))?;
    let logical_path = state
        .service
        .note(document_id.clone())
        .await
        .map_err(ApiError::service)?
        .map(|(head, _)| head.logical_path)
        .unwrap_or_else(|| protocol_logical_path(&document_id));
    let prepared = run_blocking(move || {
        let mut prepared = prepare_document(
            Some(document_id.to_string()),
            logical_path,
            input.content_type,
            input.source,
            input.expected_revision,
        )
        .map_err(ApiError::invalid_content)?;
        prepared.logical_path_is_request_input = false;
        let view =
            DocumentView::from_projection(&prepared.document_id, u64::MAX, &prepared.projection);
        let projection_size = serde_json::to_vec(&view)
            .map_err(|_| ApiError::internal())?
            .len();
        if projection_size > crate::protocol::MAX_FRAME_SIZE {
            return Err(ApiError::invalid_content(
                "document projection is too large",
            ));
        }
        Ok(prepared)
    })
    .await?;
    let result = apply_document_mutation(&state, principal, operation_id, prepared).await?;
    Ok(Json(result))
}

async fn run_blocking<T, F>(operation: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T, ApiError> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| ApiError::internal())?
}

async fn get_document_projection(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<DocumentView>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    let query = QueryDocument::new(id).map_err(ApiError::application)?;
    let document = state
        .service
        .get(query.document_id().clone())
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    run_blocking(move || {
        let view = DocumentView::from_projection(
            document.document_id(),
            document.revision(),
            document.projection(),
        );
        ensure_projection_response_budget(&view)?;
        Ok(Json(view))
    })
    .await
}

#[derive(Deserialize)]
struct DueQuery {
    day: u32,
    limit: Option<u16>,
    after: Option<String>,
}

async fn due_page(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(input): Query<DueQuery>,
) -> Result<Json<DuePageView>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    require(&principal, DeviceScope::Review)?;
    let limit = input.limit.unwrap_or(100);
    let replica_state = state
        .service
        .replica_state(principal.device_id)
        .await
        .map_err(ApiError::service)?;
    let cursor_key = replica_state.server_id;
    let query = QueryDueThemataPage::new(input.day, limit, input.after, &cursor_key)
        .map_err(|_| ApiError::bad_request("invalid due-page query"))?;
    let due = state
        .service
        .due_themata_page(query)
        .await
        .map_err(ApiError::service)?;
    run_blocking(move || bounded_due_page(due, usize::from(limit), input.day, cursor_key))
        .await
        .map(Json)
}

fn bounded_due_page(
    due: Vec<DueThema>,
    limit: usize,
    day: u32,
    cursor_key: String,
) -> Result<DuePageView, ApiError> {
    let max_items = due.len().min(limit);
    let mut low = 0;
    let mut high = max_items;
    while low < high {
        let count = low + (high - low).div_ceil(2);
        let page = due_page_with_count(&due, count, day, &cursor_key);
        if serde_json::to_vec(&page).is_ok_and(|bytes| bytes.len() <= MAX_FRAME_SIZE) {
            low = count;
        } else {
            high = count - 1;
        }
    }
    if max_items > 0 && low == 0 {
        return Err(ApiError::invalid_content(
            "due item exceeds the projection response limit",
        ));
    }
    Ok(due_page_with_count(&due, low, day, &cursor_key))
}

fn due_page_with_count(due: &[DueThema], count: usize, day: u32, cursor_key: &str) -> DuePageView {
    let next = if due.len() > count && count > 0 {
        due.get(count - 1)
            .map(DueCursor::after)
            .map(|cursor| cursor.encode(day, cursor_key))
    } else {
        None
    };
    DuePageView::new(due.iter().take(count).cloned().collect(), next)
}

fn bounded_projection_page<T, P>(
    items: &[T],
    page: impl Fn(&[T]) -> Result<P, ApiError>,
    oversized_item_message: &'static str,
) -> Result<P, ApiError>
where
    P: Serialize,
{
    let mut low = 0;
    let mut high = items.len();
    while low < high {
        let count = low + (high - low).div_ceil(2);
        let candidate = page(&items[..count])?;
        if serde_json::to_vec(&candidate).is_ok_and(|bytes| bytes.len() <= MAX_FRAME_SIZE) {
            low = count;
        } else {
            high = count - 1;
        }
    }
    if !items.is_empty() && low == 0 {
        return Err(ApiError::invalid_content(oversized_item_message));
    }
    page(&items[..low])
}

fn ensure_projection_response_budget(value: &impl Serialize) -> Result<(), ApiError> {
    let size = serde_json::to_vec(value)
        .map_err(|_| ApiError::internal())?
        .len();
    if size > MAX_FRAME_SIZE {
        return Err(ApiError::invalid_content(
            "projection exceeds the response limit",
        ));
    }
    Ok(())
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
    run_blocking(move || {
        bounded_projection_page(
            &items,
            |page| ContentChangePageV1::from_application(page).map_err(|_| ApiError::internal()),
            "content change exceeds the response limit",
        )
    })
    .await
    .map(Json)
}

async fn device_identity(
    Extension(principal): Extension<DevicePrincipal>,
) -> Result<Json<DeviceIdentityV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    Ok(Json(DeviceIdentityV1 {
        version: 1,
        device_id: principal.device_id,
    }))
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
    let events = BroadcastStream::new(state.cursors.subscribe())
        .filter_map(|cursor| {
            cursor
                .ok()
                .map(|cursor| Ok(Event::default().event("cursor").data(cursor.to_string())))
        })
        .map(Some);
    let shutdown =
        WatchStream::new(state.shutdown.clone()).filter_map(|requested| requested.then_some(None));
    let stream = events
        .merge(shutdown)
        .take_while(Option::is_some)
        .filter_map(std::convert::identity);
    Ok(Sse::new(stream).keep_alive(axum::response::sse::KeepAlive::default()))
}

async fn create_document_mutation(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<NoteWriteRequestV1>,
) -> Result<(StatusCode, Json<DocumentMutationResultV1>), ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let operation_id = idempotency_key(&headers)?;
    let prepared = run_blocking(move || {
        prepare_document(
            None,
            input.logical_path,
            input.content_type,
            input.source,
            None,
        )
        .map_err(ApiError::invalid_content)
    })
    .await?;
    let result = apply_document_mutation(&state, principal, operation_id, prepared).await?;
    Ok((StatusCode::OK, Json(result)))
}

async fn replace_document_mutation(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<NoteWriteRequestV1>,
) -> Result<Json<DocumentMutationResultV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let operation_id = idempotency_key(&headers)?;
    let revision = if_match(&headers)?;
    let prepared = run_blocking(move || {
        prepare_document(
            Some(id),
            input.logical_path,
            input.content_type,
            input.source,
            Some(revision),
        )
        .map_err(ApiError::invalid_content)
    })
    .await?;
    let result = apply_document_mutation(&state, principal, operation_id, prepared).await?;
    Ok(Json(result))
}

async fn rename_document_mutation(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<NoteRenameRequestV1>,
) -> Result<Json<DocumentMutationResultV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let document_id =
        DocumentId::new(id).map_err(|_| ApiError::bad_request("invalid document ID"))?;
    let operation = prepare_device_document_rename(
        1,
        idempotency_key(&headers)?,
        principal.device_id,
        document_id,
        input.logical_path,
        if_match(&headers)?,
    )
    .map_err(|_| ApiError::bad_request("invalid Idempotency-Key"))?;
    apply_document_lifecycle_mutation(&state, operation).await
}

async fn tombstone_document_mutation(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<DocumentMutationResultV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let operation = prepare_device_document_tombstone(
        1,
        idempotency_key(&headers)?,
        principal.device_id,
        DocumentId::new(id).map_err(|_| ApiError::bad_request("invalid document ID"))?,
        if_match(&headers)?,
    )
    .map_err(|_| ApiError::bad_request("invalid Idempotency-Key"))?;
    apply_document_lifecycle_mutation(&state, operation).await
}

async fn apply_document_lifecycle_mutation(
    state: &HttpState,
    operation: PreparedDeviceDocumentLifecycleMutation,
) -> Result<Json<DocumentMutationResultV1>, ApiError> {
    let execution = state
        .service
        .apply_device_document_lifecycle_mutation(operation, now_us_i64()?)
        .await
        .map_err(ApiError::service)?;
    if !execution.replayed && execution.result.changed() {
        announce(state).await;
    }
    let result = DocumentMutationResultV1::from_application(&execution.result)
        .map_err(|_| ApiError::internal())?;
    Ok(Json(result))
}

async fn apply_document_mutation(
    state: &HttpState,
    principal: DevicePrincipal,
    operation_id: String,
    prepared: PreparedNote,
) -> Result<DocumentMutationResultV1, ApiError> {
    let operation =
        prepare_device_document_mutation(1, operation_id, principal.device_id, prepared)
            .map_err(|_| ApiError::bad_request("invalid Idempotency-Key"))?;
    let execution = state
        .service
        .apply_device_document_mutation(operation, now_us_i64()?)
        .await
        .map_err(ApiError::service)?;
    if !execution.replayed && execution.result.changed() {
        announce(state).await;
    }
    DocumentMutationResultV1::from_application(&execution.result).map_err(|_| ApiError::internal())
}

async fn apply_desired_state_mutation(
    state: &HttpState,
    principal: DevicePrincipal,
    operation_id: String,
    mutation: DeviceDesiredStateMutation,
    asset_bytes: Option<Vec<u8>>,
) -> Result<Json<DesiredStateMutationResultV1>, ApiError> {
    let operation =
        prepare_device_desired_state_mutation(1, operation_id, principal.device_id, mutation)
            .map_err(|error| match error {
                crate::application::DeviceMutationError::InvalidSettings => {
                    ApiError::invalid_content("invalid settings")
                }
                _ => ApiError::bad_request("invalid device mutation"),
            })?;
    let request_sha256 = crate::application::desired_state_wire_request_sha256(
        operation.identity.format_version,
        &operation.mutation,
    )
    .map_err(|_| ApiError::internal())?;
    let execution = state
        .service
        .apply_device_desired_state_mutation(operation, asset_bytes, now_us_i64()?)
        .await
        .map_err(ApiError::service)?;
    if !execution.replayed && execution.result.changed() {
        announce(state).await;
    }
    let result = DesiredStateMutationResultV1::from_application(&execution.result, &request_sha256)
        .map_err(|_| ApiError::internal())?;
    Ok(Json(result))
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
        HeaderValue::from_str(&head.content_type).map_err(|_| ApiError::internal())?,
    );
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", head.revision))
            .map_err(|_| ApiError::internal())?,
    );
    Ok(response)
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
    run_blocking(move || {
        bounded_projection_page(
            &items,
            |page| DocumentRevisionPageV1::from_application(page).map_err(|_| ApiError::internal()),
            "document revision exceeds the response limit",
        )
    })
    .await
    .map(Json)
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
        record
            .content_type
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
) -> Result<Json<DesiredStateMutationResultV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let path = canonical_asset_path(&digest, &name).map_err(ApiError::invalid_content)?;
    let expected = optional_if_match(&headers)?;
    let prepared = prepare_asset(path, name, expected).map_err(ApiError::invalid_content)?;
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_owned();
    let operation_id = idempotency_key(&headers)?;
    let size_bytes = u64::try_from(body.len()).map_err(|_| ApiError::internal())?;
    let body_sha256 = format!("{:x}", Sha256::digest(&body));
    apply_desired_state_mutation(
        &state,
        principal,
        operation_id,
        DeviceDesiredStateMutation::PutAsset {
            prepared,
            digest,
            body_sha256,
            size_bytes,
            media_type: media,
        },
        Some(body.to_vec()),
    )
    .await
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
) -> Result<Json<DesiredStateMutationResultV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let path = canonical_asset_path(&digest, &name).map_err(ApiError::invalid_content)?;
    let operation_id = idempotency_key(&headers)?;
    let expected_revision = if_match(&headers)?;
    apply_desired_state_mutation(
        &state,
        principal,
        operation_id,
        DeviceDesiredStateMutation::TombstoneAsset {
            asset_path: path,
            expected_revision,
        },
        None,
    )
    .await
}

async fn create_thema(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<ThemaWriteRequestV1>,
) -> Result<Json<DesiredStateMutationResultV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    ensure_thema_response_budget(&input)?;
    let operation_id = idempotency_key(&headers)?;
    let thema_id = durable_created_thema_id(&principal.device_id, &operation_id);
    let prepared = prepared_thema(Some(thema_id), input, None)?;
    let settings = state.service.settings().await.map_err(ApiError::service)?;
    apply_desired_state_mutation(
        &state,
        principal,
        operation_id,
        DeviceDesiredStateMutation::PutThema {
            prepared,
            initial_review_day: settings_review_day(&settings)?,
        },
        None,
    )
    .await
}

fn durable_created_thema_id(device_id: &str, operation_id: &str) -> String {
    let mut digest = Sha256::new();
    digest.update(b"gnosis-thema-v1\0");
    digest.update(device_id.as_bytes());
    digest.update(b"\0");
    digest.update(operation_id.as_bytes());
    format!("{:x}", digest.finalize())
}

async fn update_thema(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(input): ApiJson<ThemaWriteRequestV1>,
) -> Result<Json<DesiredStateMutationResultV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    ensure_thema_response_budget(&input)?;
    let prepared = prepared_thema(Some(id), input, Some(if_match(&headers)?))?;
    let settings = state.service.settings().await.map_err(ApiError::service)?;
    apply_desired_state_mutation(
        &state,
        principal,
        idempotency_key(&headers)?,
        DeviceDesiredStateMutation::PutThema {
            prepared,
            initial_review_day: settings_review_day(&settings)?,
        },
        None,
    )
    .await
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
        input
            .concepts
            .into_iter()
            .map(|concept| (concept.node_id, concept.relation.into()))
            .collect(),
        input.asset_paths,
        revision,
    )
    .map_err(ApiError::invalid_content)
}

fn ensure_thema_response_budget(input: &ThemaWriteRequestV1) -> Result<(), ApiError> {
    let upper_bound = serde_json::to_vec(&json!({
        "status": "applied",
        "format_version": 1,
        "operation_id": "x".repeat(128),
        "changed": true,
        "authoritative": {
            "kind": "thema",
            "value": {
                "thema_id": "x".repeat(128),
                "revision": u64::MAX,
                "kind": input.kind,
                "keimenon": input.keimenon,
                "hypothesis": input.hypothesis,
                "answer": input.answer,
                "source_guid": input.source_guid,
                "parathema": input.parathema,
                "review_image": input.review_image,
                "tags": input.tags,
                "concepts": input.concepts,
                "asset_paths": input.asset_paths,
                "content_sha256": "0".repeat(64),
                "archived": true
            }
        },
        "cursor": "x".repeat(512)
    }))
    .map_err(|_| ApiError::internal())?
    .len();
    if upper_bound > MAX_FRAME_SIZE {
        return Err(ApiError::invalid_content(
            "thema response exceeds the response limit",
        ));
    }
    Ok(())
}

fn bounded_thema(record: &crate::application::ThemaRecord) -> Result<Json<ThemaV1>, ApiError> {
    let response = ThemaV1::from_application(record);
    let size = serde_json::to_vec(&response)
        .map_err(|_| ApiError::internal())?
        .len();
    if size > MAX_FRAME_SIZE {
        return Err(ApiError::invalid_content(
            "thema response exceeds the response limit",
        ));
    }
    Ok(Json(response))
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
    bounded_thema(&record)
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
    run_blocking(move || {
        bounded_projection_page(
            &items,
            |page| Ok(ThemaPageV1::from_application(page)),
            "thema exceeds the response limit",
        )
    })
    .await
    .map(Json)
}

async fn thema_catalog(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<PageQuery>,
) -> Result<Json<ThemaCatalogPageV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    if let Some(after) = query.after.as_deref() {
        ThemaId::new(after.to_owned()).map_err(|_| ApiError::bad_request("invalid cursor"))?;
    }
    let page = state
        .service
        .thema_catalog(
            principal.device_id,
            query.q.unwrap_or_default(),
            page_limit(query.limit)?,
            query.after,
        )
        .await
        .map_err(ApiError::service)?;
    let generation = replica_state_response(&page.state)
        .map_err(ApiError::replica_wire)?
        .cursor;
    let item_count = page.items.len();
    run_blocking(move || {
        bounded_projection_page(
            &page.items,
            |items| {
                Ok(ThemaCatalogPageV1::from_application(
                    items,
                    generation.clone(),
                    page.has_more || items.len() < item_count,
                ))
            },
            "thema catalog item exceeds the response limit",
        )
    })
    .await
    .map(Json)
}

async fn archive_thema(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
    headers: HeaderMap,
) -> Result<Json<DesiredStateMutationResultV1>, ApiError> {
    set_archive(state, principal, path, headers, true).await
}
async fn unarchive_thema(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
    headers: HeaderMap,
) -> Result<Json<DesiredStateMutationResultV1>, ApiError> {
    set_archive(state, principal, path, headers, false).await
}

async fn get_thema_review_state(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<ThemaReviewStateV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let state = state
        .service
        .thema_review_state(id)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(ThemaReviewStateV1::from(&state)))
}

async fn set_thema_suspension(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<SetThemaSuspensionRequestV1>,
) -> Result<Json<DesiredStateMutationResultV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let operation_id = idempotency_key(&headers)?;
    let expected_state_revision = if_match(&headers)?;
    let thema_id = ThemaId::new(id).map_err(|_| ApiError::bad_request("invalid thema ID"))?;
    apply_desired_state_mutation(
        &state,
        principal,
        operation_id,
        DeviceDesiredStateMutation::SetThemaSuspended {
            thema_id,
            expected_state_revision,
            suspended: request.suspended,
        },
        None,
    )
    .await
}

async fn set_archive(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    headers: HeaderMap,
    archived: bool,
) -> Result<Json<DesiredStateMutationResultV1>, ApiError> {
    require(&principal, DeviceScope::ContentWrite)?;
    let operation_id = idempotency_key(&headers)?;
    let expected_revision = if_match(&headers)?;
    let thema_id = ThemaId::new(id).map_err(|_| ApiError::bad_request("invalid thema ID"))?;
    apply_desired_state_mutation(
        &state,
        principal,
        operation_id,
        DeviceDesiredStateMutation::SetThemaArchived {
            thema_id,
            expected_revision,
            archived,
        },
        None,
    )
    .await
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
async fn node_themata(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    Query(query): Query<PageQuery>,
) -> Result<Json<NodeThemaAssociationPageV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    NodeId::new(id.clone()).map_err(|_| ApiError::bad_request("invalid node ID"))?;
    if let Some(after) = query.after.as_deref() {
        ThemaId::new(after.to_owned()).map_err(|_| ApiError::bad_request("invalid cursor"))?;
    }
    let items = state
        .service
        .node_themata(id, page_limit(query.limit)?, query.after)
        .await
        .map_err(ApiError::service)?;
    run_blocking(move || {
        bounded_projection_page(
            &items,
            |page| Ok(NodeThemaAssociationPageV1::from_application(page)),
            "node thema association exceeds the response limit",
        )
    })
    .await
    .map(Json)
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
    run_blocking(move || {
        bounded_projection_page(
            &items,
            |page| Ok(NodePageV1::from_application(page)),
            "node exceeds the response limit",
        )
    })
    .await
    .map(Json)
}

async fn node_catalog(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<NodeCatalogQuery>,
) -> Result<Json<NodeCatalogPageV1>, ApiError> {
    require(&principal, DeviceScope::Read)?;
    if let Some(document_id) = query.document_id.as_deref() {
        DocumentId::new(document_id.to_owned())
            .map_err(|_| ApiError::bad_request("invalid document ID"))?;
    }
    if let Some(after) = query.after.as_deref() {
        NodeId::new(after.to_owned()).map_err(|_| ApiError::bad_request("invalid cursor"))?;
    }
    let page = state
        .service
        .node_catalog(
            principal.device_id,
            query.document_id,
            node_catalog_limit(query.limit)?,
            query.after,
        )
        .await
        .map_err(ApiError::service)?;
    let generation = replica_state_response(&page.state)
        .map_err(ApiError::replica_wire)?
        .cursor;
    let item_count = page.items.len();
    run_blocking(move || {
        bounded_projection_page(
            &page.items,
            |items| {
                Ok(NodeCatalogPageV1::from_application(
                    items,
                    generation.clone(),
                    page.has_more || items.len() < item_count,
                ))
            },
            "node catalog item exceeds the response limit",
        )
    })
    .await
    .map(Json)
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
    run_blocking(move || {
        bounded_projection_page(
            &items,
            |page| Ok(NodePageV1::from_application(page)),
            "node exceeds the response limit",
        )
    })
    .await
    .map(Json)
}

#[derive(Deserialize)]
struct ReviewSessionsQuery {
    after: Option<String>,
    limit: Option<u16>,
    status: Option<ReviewSessionStatusV1>,
    mode: Option<ReviewSessionModeV1>,
}

async fn review_sessions(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<ReviewSessionsQuery>,
) -> Result<Json<ReviewSessionPageV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let sessions = state
        .service
        .review_sessions(ReviewSessionQuery {
            after: query.after,
            limit: page_limit(query.limit)?,
            status: query.status.map(|status| status.as_str().to_owned()),
            mode: query.mode.map(|mode| mode.as_str().to_owned()),
        })
        .await
        .map_err(ApiError::service)?;
    run_blocking(move || {
        bounded_projection_page(
            &sessions,
            |page| ReviewSessionPageV1::from_application(page).map_err(|_| ApiError::internal()),
            "review session exceeds the response limit",
        )
    })
    .await
    .map(Json)
}

async fn review_session(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<ReviewSessionSummaryV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let session = state
        .service
        .review_session_summary(id)
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(
        ReviewSessionSummaryV1::from_application(&session).map_err(|_| ApiError::internal())?,
    ))
}

#[derive(Deserialize)]
struct ReviewHistoryHttpQuery {
    after: Option<String>,
    limit: Option<u16>,
    thema_id: Option<String>,
    session_id: Option<String>,
    from_day: Option<u32>,
    through_day: Option<u32>,
    accepted_success: Option<bool>,
}

impl ReviewHistoryHttpQuery {
    fn into_application(self) -> Result<ReviewHistoryQuery, ApiError> {
        if self.from_day.is_some_and(|day| !valid_review_day(day))
            || self.through_day.is_some_and(|day| !valid_review_day(day))
            || self
                .from_day
                .zip(self.through_day)
                .is_some_and(|(from, through)| from > through)
        {
            return Err(ApiError::bad_request("invalid review history window"));
        }
        Ok(ReviewHistoryQuery {
            after: self.after,
            limit: page_limit(self.limit)?,
            thema_id: self.thema_id,
            session_id: self.session_id,
            from_day: self.from_day,
            through_day: self.through_day,
            accepted_success: self.accepted_success,
        })
    }
}

fn valid_review_day(value: u32) -> bool {
    i32::try_from(value / 10_000)
        .is_ok_and(|year| NaiveDate::from_ymd_opt(year, value / 100 % 100, value % 100).is_some())
}

async fn review_history(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Query(query): Query<ReviewHistoryHttpQuery>,
) -> Result<Json<ReviewHistoryPageV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let events = state
        .service
        .review_history(query.into_application()?)
        .await
        .map_err(ApiError::service)?;
    run_blocking(move || {
        bounded_projection_page(
            &events,
            |page| Ok(ReviewHistoryPageV1::from_application(page)),
            "review history event exceeds the response limit",
        )
    })
    .await
    .map(Json)
}

async fn review_queue_summary(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(request): ApiJson<ReviewQueueSummaryRequestV1>,
) -> Result<Json<ReviewQueueSummaryV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let summary = state
        .service
        .review_queue_summary(request.into_application(), now_us()?)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(ReviewQueueSummaryV1::from(&summary)))
}

async fn create_session(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(input): ApiJson<ReviewSessionRequestV1>,
) -> Result<(StatusCode, Json<ReviewSessionV1>), ApiError> {
    require(&principal, DeviceScope::Review)?;
    let (source, target_count) = input.into_application();
    let session = state
        .service
        .create_review_session(source, target_count, Some(principal.device_id), now_us()?)
        .await
        .map_err(ApiError::service)?;
    Ok((
        StatusCode::CREATED,
        Json(ReviewSessionV1::from_application(&session).map_err(|_| ApiError::internal())?),
    ))
}
async fn active_session(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
) -> Result<Json<ActiveReviewSessionV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let session = state
        .service
        .active_review_session(online_review_actor())
        .await
        .map_err(ApiError::service)?;
    let session = session
        .as_ref()
        .map(ReviewSessionV1::from_application)
        .transpose()
        .map_err(|_| ApiError::internal())?;
    Ok(Json(ActiveReviewSessionV1 { session }))
}
async fn resume_session(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<ReviewSessionV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let session = state
        .service
        .resume_review_session(id, online_review_actor())
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        ReviewSessionV1::from_application(&session).map_err(|_| ApiError::internal())?,
    ))
}
async fn quit_session(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
) -> Result<Json<ReviewSessionV1>, ApiError> {
    stop_session(state, principal, path, false).await
}
async fn abort_session(
    state: State<HttpState>,
    principal: Extension<DevicePrincipal>,
    path: Path<String>,
) -> Result<Json<ReviewSessionV1>, ApiError> {
    stop_session(state, principal, path, true).await
}
async fn stop_session(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    abort: bool,
) -> Result<Json<ReviewSessionV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let session = state
        .service
        .stop_review_session(id, online_review_actor(), abort, now_us()?)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        ReviewSessionV1::from_application(&session).map_err(|_| ApiError::internal())?,
    ))
}
async fn begin_attempt(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<(StatusCode, Json<ReviewAttemptBeginResponseV1>), ApiError> {
    require(&principal, DeviceScope::Review)?;
    let result = state
        .service
        .begin_http_review_attempt(id, online_review_actor(), now_us()?)
        .await
        .map_err(ApiError::service)?;
    Ok((
        StatusCode::CREATED,
        Json(
            ReviewAttemptBeginResponseV1::from_application(&result)
                .map_err(|_| ApiError::internal())?,
        ),
    ))
}
async fn answer_attempt(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    ApiJson(answer): ApiJson<ReviewAnswerV1>,
) -> Result<Json<ReviewAttemptV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    require_safe_http_attempt(&state, &id).await?;
    let attempt = state
        .service
        .grade_review_attempt(id, online_review_actor(), answer.into(), now_us()?)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        ReviewAttemptV1::from_application(&attempt).map_err(|_| ApiError::internal())?,
    ))
}

async fn check_cloze_attempt(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
    ApiJson(request): ApiJson<ReviewClozeCheckRequestV1>,
) -> Result<Json<ReviewClozeCheckV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    require_safe_http_attempt(&state, &id).await?;
    let check = state
        .service
        .check_cloze_attempt(id, online_review_actor(), request.inputs)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(ReviewClozeCheckV1::from(check)))
}
async fn get_attempt_state(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<ReviewAttemptStateResponseV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    Ok(Json(require_safe_http_attempt(&state, &id).await?))
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
    require_safe_http_attempt(state, &operation.attempt_id).await?;
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
) -> Result<Json<ReviewCommitResponseV1>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    require_safe_http_attempt(&state, &id).await?;
    let attempt_id = id.clone();
    let event = state
        .service
        .commit_review_attempt(id, online_review_actor(), now_us()?)
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(ReviewCommitResponseV1 {
        attempt_id,
        event_id: event,
    }))
}

async fn require_safe_http_attempt(
    state: &HttpState,
    attempt_id: &str,
) -> Result<ReviewAttemptStateResponseV1, ApiError> {
    let attempt = state
        .service
        .review_attempt_state(attempt_id.to_owned(), online_review_actor())
        .await
        .map_err(ApiError::service)?;
    ReviewAttemptStateResponseV1::from_application(&attempt)
        .map_err(|_| ApiError::bad_request("review attempt presentation is invalid"))
}

async fn create_pack(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(input): ApiJson<ReviewSessionRequestV1>,
) -> Result<(StatusCode, Json<ReviewPackEnvelopeV2>), ApiError> {
    require(&principal, DeviceScope::Review)?;
    let (source, target_count) = input.into_application();
    let pack = state
        .service
        .create_review_pack(
            crate::protocol::OFFLINE_REVIEW_VERSION,
            source,
            target_count,
            principal.device_id,
            now_us()?,
        )
        .await
        .map_err(ApiError::service)?;
    Ok((
        StatusCode::CREATED,
        Json(review_pack_response(&pack).map_err(|_| ApiError::internal())?),
    ))
}

async fn get_pack(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    Path(id): Path<String>,
) -> Result<Json<ReviewPackEnvelopeV2>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let pack = state
        .service
        .review_pack(id, device_review_actor(&principal))
        .await
        .map_err(ApiError::service)?
        .ok_or_else(ApiError::not_found)?;
    Ok(Json(
        review_pack_response(&pack).map_err(|_| ApiError::internal())?,
    ))
}

async fn active_pack(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
) -> Result<Json<ActiveReviewPackEnvelopeV2>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let pack = state
        .service
        .active_review_pack(device_review_actor(&principal))
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        active_review_pack_response(pack.as_ref()).map_err(|_| ApiError::internal())?,
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
        .review_pack_asset(id, device_review_actor(&principal), &path, &digest)
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
) -> Result<Json<ReviewPackEnvelopeV2>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let pack = state
        .service
        .abandon_review_pack(id, device_review_actor(&principal), now_us()?)
        .await
        .map_err(ApiError::service)?;
    Ok(Json(
        review_pack_response(&pack).map_err(|_| ApiError::internal())?,
    ))
}

async fn reconcile_offline(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(submission): ApiJson<OfflineReviewSubmission>,
) -> Result<Json<OfflineReviewResultEnvelopeV2>, ApiError> {
    require(&principal, DeviceScope::Review)?;
    let decoded = submission.decode().map_err(ApiError::offline_review_wire)?;
    let result = state
        .service
        .reconcile_offline_operation(
            decoded.version,
            decoded.operation,
            decoded.canonical_payload,
            device_review_actor(&principal),
            now_us()?,
        )
        .await
        .map_err(ApiError::service)?;
    announce(&state).await;
    Ok(Json(
        offline_result_response(&result).map_err(|_| ApiError::internal())?,
    ))
}

async fn reconcile_content(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(submission): ApiJson<OfflineContentOperationEnvelopeV1>,
) -> Result<Json<OfflineContentResultEnvelopeV1>, ApiError> {
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
        offline_content_result_response(&result).map_err(|_| ApiError::internal())?,
    ))
}

async fn reconcile_asset(
    State(state): State<HttpState>,
    Extension(principal): Extension<DevicePrincipal>,
    ApiJson(submission): ApiJson<OfflineAssetSubmission>,
) -> Result<Json<OfflineAssetResultEnvelopeV1>, ApiError> {
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
        offline_asset_result_response(&result).map_err(|_| ApiError::internal())?,
    ))
}

fn online_review_actor() -> Option<String> {
    None
}

fn device_review_actor(principal: &DevicePrincipal) -> Option<String> {
    (!principal.scopes.contains(&DeviceScope::Admin)).then(|| principal.device_id.clone())
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

fn idempotency_key(headers: &HeaderMap) -> Result<String, ApiError> {
    let mut values = headers.get_all("idempotency-key").iter();
    let value = values
        .next()
        .ok_or_else(|| ApiError::bad_request("Idempotency-Key is required"))?;
    if values.next().is_some() {
        return Err(ApiError::bad_request("invalid Idempotency-Key"));
    }
    value
        .to_str()
        .map(str::to_owned)
        .map_err(|_| ApiError::bad_request("invalid Idempotency-Key"))
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
) -> Result<Json<DesiredStateMutationResultV1>, ApiError> {
    require(&principal, DeviceScope::SettingsWrite)?;
    let operation_id = idempotency_key(&headers)?;
    let expected_revision = if_match(&headers)?;
    apply_desired_state_mutation(
        &state,
        principal,
        operation_id,
        DeviceDesiredStateMutation::ReplaceSettings {
            input: input.into_application(),
            expected_revision,
        },
        None,
    )
    .await
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
            | ServiceError::ShutdownDeadlineExceeded
            | ServiceError::InvalidRepairBatchSize
            | ServiceError::StageBlobReferenceCheck { .. }
            | ServiceError::StageBlobCleanup { .. }
            | ServiceError::Blob(_)
            | ServiceError::Backup(_) => Self::internal(),
        }
    }

    fn store(error: StoreError) -> Self {
        match error {
            StoreError::NotFound => Self::not_found(),
            StoreError::Conflict { expected, actual } => Self::conflict(expected, actual),
            StoreError::ExpectedAbsent { .. } => Self::internal(),
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
            StoreError::InvalidReviewState | StoreError::TopicTraversalLimitExceeded => {
                Self::bad_request("review operation is invalid")
            }
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
            StoreError::InvalidCursor => Self::bad_request("invalid cursor"),
            StoreError::NodePageProjectionLimit => Self::internal(),
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
            StoreError::PathConflict => Self::invalid_content("stored projection"),
            StoreError::InvalidProjection { .. } => Self::invalid_content("stored projection"),
            StoreError::CorruptData { .. }
            | StoreError::BackupDeadlineExceeded
            | StoreError::BackupCancelled
            | StoreError::UnsupportedSchemaVersion { .. }
            | StoreError::UnsupportedSchemaLayout
            | StoreError::UnsupportedApplicationId { .. }
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
    if let Some(operation) = openapi_offline_review_operation(path, method) {
        return operation;
    }
    if let Some(operation) = openapi_online_review_operation(path, method) {
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

fn openapi_offline_review_operation(path: &str, method: &str) -> Option<Value> {
    if path == "/v1/review/packs/{id}/assets/{digest}/{name}" && method == "get" {
        return Some(offline_review_binary_operation(path, method));
    }
    let contract = offline_review_contract(path, method)?;
    let mut operation = json!({
        "operationId":format!("{}_{}", method, path.replace("/v1/", "").replace(['/', '{', '}'], "_")),
        "security":[{"bearerAuth":[]}],
        "x-gnosis-required-scopes":["review"],
        "parameters":openapi_path_parameters(path),
        "responses":content_json_responses(
            contract.success_status,
            contract.response,
            contract.error_statuses,
        )
    });
    if let Some(request) = contract.request {
        operation["requestBody"] = openapi_json_request(request);
    }
    Some(operation)
}

fn offline_review_contract(path: &str, method: &str) -> Option<ContentJsonContract> {
    match (path, method) {
        ("/v1/review/packs", "post") => Some(json_contract(
            "review",
            Some("ReviewSessionRequestV1"),
            "201",
            "ReviewPackEnvelopeV2",
            &[
                "400", "401", "403", "404", "408", "409", "413", "422", "500", "503",
            ],
        )),
        ("/v1/review/packs/active", "get") => Some(json_contract(
            "review",
            None,
            "200",
            "ActiveReviewPackEnvelopeV2",
            &["401", "403", "408", "500", "503"],
        )),
        ("/v1/review/packs/{id}", "get") => Some(json_contract(
            "review",
            None,
            "200",
            "ReviewPackEnvelopeV2",
            &["401", "403", "404", "408", "500", "503"],
        )),
        ("/v1/review/packs/{id}/abandon", "post") => Some(json_contract(
            "review",
            None,
            "200",
            "ReviewPackEnvelopeV2",
            &["400", "401", "403", "404", "408", "500", "503"],
        )),
        ("/v1/review/offline-operations", "post") => Some(json_contract(
            "review",
            Some("OfflineReviewOperationEnvelopeV1"),
            "200",
            "OfflineReviewResultEnvelopeV2",
            &[
                "400", "401", "403", "404", "408", "409", "413", "422", "500", "503",
            ],
        )),
        _ => None,
    }
}

fn offline_review_binary_operation(path: &str, method: &str) -> Value {
    json!({
        "operationId":format!("{}_{}", method, path.replace("/v1/", "").replace(['/', '{', '}'], "_")),
        "security":[{"bearerAuth":[]}],
        "x-gnosis-required-scopes":["review"],
        "parameters":offline_review_asset_parameters(path),
        "responses":{
            "200":{"description":"Pack asset bytes","content":{"*/*":{
                "schema":{"type":"string","format":"binary"}
            }}},
            "401":openapi_error_response(),
            "403":openapi_error_response(),
            "404":openapi_error_response(),
            "408":openapi_error_response(),
            "422":openapi_error_response(),
            "500":openapi_error_response(),
            "503":openapi_error_response()
        }
    })
}

fn offline_review_asset_parameters(path: &str) -> Vec<Value> {
    let mut parameters = openapi_path_parameters(path);
    if let Some(digest) = parameters
        .iter_mut()
        .find(|value| value["name"] == "digest")
    {
        digest["schema"] = json!({"type":"string","pattern":"^[0-9a-f]{64}$"});
    }
    parameters
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
    if path == "/v1/review/due" {
        operation["x-gnosis-required-scopes"] = json!(["read", "review"]);
    }
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
        ("/v1/documents/{id}", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "DocumentView",
            &["400", "401", "403", "404", "408", "422", "500", "503"],
        )),
        ("/v1/documents/{id}", "put") => Some(json_contract(
            "content:write",
            Some("DocumentImportRequestV1"),
            "200",
            "DocumentMutationResultV1",
            &[
                "400", "401", "403", "408", "409", "413", "422", "500", "503",
            ],
        )),
        ("/v1/review/due", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "DuePageView",
            &["400", "401", "403", "408", "422", "500", "503"],
        )),
        ("/v1/content/offline-operations", "post") => Some(json_contract(
            "content:write",
            Some("OfflineContentOperationEnvelopeV1"),
            "200",
            "OfflineContentResultEnvelopeV1",
            &[
                "400", "401", "403", "408", "409", "413", "422", "500", "503",
            ],
        )),
        ("/v1/content/offline-asset-operations", "post") => Some(json_contract(
            "content:write",
            Some("OfflineAssetOperationEnvelopeV1"),
            "200",
            "OfflineAssetResultEnvelopeV1",
            &[
                "400", "401", "403", "408", "409", "413", "422", "500", "503",
            ],
        )),
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
            "DesiredStateMutationResultV1",
            &[
                "400", "401", "403", "408", "409", "413", "422", "428", "500", "503",
            ],
        )),

        ("/v1/document-mutations", "post") => Some(json_contract(
            "content:write",
            Some("NoteWriteRequestV1"),
            "200",
            "DocumentMutationResultV1",
            &[
                "400", "401", "403", "408", "409", "413", "422", "500", "503",
            ],
        )),
        ("/v1/document-mutations/{id}", "put") => Some(json_contract(
            "content:write",
            Some("NoteWriteRequestV1"),
            "200",
            "DocumentMutationResultV1",
            &[
                "400", "401", "403", "408", "409", "413", "422", "428", "500", "503",
            ],
        )),
        ("/v1/document-mutations/{id}", "delete") => Some(json_contract(
            "content:write",
            None,
            "200",
            "DocumentMutationResultV1",
            &["400", "401", "403", "408", "409", "428", "500", "503"],
        )),
        ("/v1/document-mutations/{id}/rename", "post") => Some(json_contract(
            "content:write",
            Some("NoteRenameRequestV1"),
            "200",
            "DocumentMutationResultV1",
            &[
                "400", "401", "403", "408", "409", "413", "422", "428", "500", "503",
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
            "200",
            "DesiredStateMutationResultV1",
            &[
                "400", "401", "403", "408", "409", "413", "422", "500", "503",
            ],
        )),
        ("/v1/assets/{digest}/{name}", "delete") => Some(json_contract(
            "content:write",
            None,
            "200",
            "DesiredStateMutationResultV1",
            &[
                "400", "401", "403", "408", "409", "422", "428", "500", "503",
            ],
        )),
        ("/v1/themata", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "ThemaPageV1",
            &["400", "401", "403", "408", "500", "503"],
        )),
        ("/v1/themata/catalog", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "ThemaCatalogPageV1",
            &["400", "401", "403", "408", "500", "503"],
        )),
        ("/v1/themata", "post") => Some(json_contract(
            "content:write",
            Some("ThemaWriteRequestV1"),
            "200",
            "DesiredStateMutationResultV1",
            &[
                "400", "401", "403", "408", "409", "413", "422", "500", "503",
            ],
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
            "DesiredStateMutationResultV1",
            &[
                "400", "401", "403", "408", "409", "413", "422", "428", "500", "503",
            ],
        )),
        ("/v1/themata/{id}/archive", "post") | ("/v1/themata/{id}/unarchive", "post") => {
            Some(json_contract(
                "content:write",
                None,
                "200",
                "DesiredStateMutationResultV1",
                &[
                    "400", "401", "403", "408", "409", "422", "428", "500", "503",
                ],
            ))
        }
        ("/v1/themata/{id}/suspension", "put") => Some(json_contract(
            "review",
            Some("SetThemaSuspensionRequestV1"),
            "200",
            "DesiredStateMutationResultV1",
            &[
                "400", "401", "403", "408", "409", "413", "422", "428", "500", "503",
            ],
        )),
        ("/v1/nodes", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "NodePageV1",
            &["400", "401", "403", "408", "500", "503"],
        )),
        ("/v1/nodes/catalog", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "NodeCatalogPageV1",
            &["400", "401", "403", "408", "500", "503"],
        )),
        ("/v1/nodes/{id}", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "NodeV1",
            &["401", "403", "404", "408", "500", "503"],
        )),
        ("/v1/nodes/{id}/themata", "get") => Some(json_contract(
            "read",
            None,
            "200",
            "NodeThemaAssociationPageV1",
            &["400", "401", "403", "404", "408", "500", "503"],
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
                media_response("Document source", "*/*", false, &["ETag"]),
            );
            &["400", "401", "403", "404", "408", "500", "503"]
        }
        "/v1/notes/{id}/revisions/{revision}" => {
            responses.insert(
                "200".into(),
                media_response("Document revision source", "*/*", false, &["ETag"]),
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
    if content_requires_idempotency(path, method) {
        parameters.push(idempotency_key_parameter());
    }
    parameters
}

fn content_requires_idempotency(path: &str, method: &str) -> bool {
    matches!(
        (path, method),
        ("/v1/documents/{id}", "put")
            | ("/v1/document-mutations", "post")
            | ("/v1/document-mutations/{id}", "put" | "delete")
            | ("/v1/document-mutations/{id}/rename", "post")
            | ("/v1/assets/{digest}/{name}", "put" | "delete")
            | ("/v1/themata", "post")
            | ("/v1/themata/{id}", "put")
            | ("/v1/themata/{id}/archive", "post")
            | ("/v1/themata/{id}/unarchive", "post")
            | ("/v1/themata/{id}/suspension", "put")
            | ("/v1/settings", "put")
    )
}

fn idempotency_key_parameter() -> Value {
    json!({
        "name":"Idempotency-Key",
        "in":"header",
        "required":true,
        "schema":{
            "type":"string",
            "minLength":1,
            "maxLength":128,
            "pattern":"^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$"
        }
    })
}

fn content_query_parameters(path: &str) -> Vec<Value> {
    if path == "/v1/review/due" {
        return vec![
            json!({"name":"day","in":"query","required":true,"description":"Valid YYYYMMDD calendar date","schema":{"type":"integer","format":"uint32","minimum":10101}}),
            json!({"name":"limit","in":"query","required":false,"schema":{"type":"integer","format":"uint16","minimum":1,"maximum":100,"default":100}}),
            json!({"name":"after","in":"query","required":false,"schema":{"type":"string","minLength":1,"maxLength":MAX_DUE_CURSOR_BYTES}}),
        ];
    }
    if path == "/v1/nodes/catalog" {
        return vec![
            json!({"name":"after","in":"query","required":false,"schema":{"type":"string"}}),
            json!({"name":"limit","in":"query","required":false,"schema":{"type":"integer","minimum":1,"maximum":MAX_NODE_CATALOG_PAGE,"default":MAX_NODE_CATALOG_PAGE}}),
            json!({"name":"document_id","in":"query","required":false,"schema":{"type":"string"}}),
        ];
    }
    let paged = matches!(
        path,
        "/v1/changes"
            | "/v1/notes/{id}/revisions"
            | "/v1/themata"
            | "/v1/themata/catalog"
            | "/v1/nodes"
            | "/v1/nodes/{id}/themata"
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
    if matches!(path, "/v1/themata" | "/v1/themata/catalog" | "/v1/nodes") {
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
            | ("/v1/document-mutations/{id}", "put" | "delete")
            | ("/v1/document-mutations/{id}/rename", "post")
            | ("/v1/notes/{id}/rename", "post")
            | ("/v1/assets/{digest}/{name}", "delete")
            | ("/v1/themata/{id}", "put")
            | ("/v1/themata/{id}/archive", "post")
            | ("/v1/themata/{id}/unarchive", "post")
            | ("/v1/themata/{id}/suspension", "put")
    )
}

fn if_match_parameter(required: bool) -> Value {
    json!({"name":"If-Match","in":"header","required":required,"schema":{"type":"string","pattern":"^(?:[0-9]+|\"[0-9]+\")$"}})
}

fn openapi_online_review_operation(path: &str, method: &str) -> Option<Value> {
    let contract = online_review_contract(path, method)?;
    let scopes = if path == "/v1/dashboard" {
        json!(["read", "review"])
    } else {
        json!(["review"])
    };
    let mut operation = json!({
        "operationId":format!("{}_{}", method, path.replace("/v1/", "").replace(['/', '{', '}'], "_")),
        "security":[{"bearerAuth":[]}],
        "x-gnosis-required-scopes":scopes,
        "parameters":online_review_parameters(path),
        "responses":content_json_responses(
            contract.success_status,
            contract.response,
            contract.error_statuses,
        )
    });
    if let Some(request) = contract.request {
        operation["requestBody"] = openapi_json_request(request);
    }
    Some(operation)
}

fn online_review_contract(path: &str, method: &str) -> Option<ContentJsonContract> {
    match (path, method) {
        ("/v1/dashboard", "get") => Some(json_contract(
            "review",
            None,
            "200",
            "DashboardV1",
            &["400", "401", "403", "408", "500", "503"],
        )),
        ("/v1/themata/{id}/review-state", "get") => Some(json_contract(
            "review",
            None,
            "200",
            "ThemaReviewStateV1",
            &["401", "403", "404", "408", "500", "503"],
        )),
        ("/v1/review/sessions", "get") => Some(json_contract(
            "review",
            None,
            "200",
            "ReviewSessionPageV1",
            &["400", "401", "403", "408", "500", "503"],
        )),
        ("/v1/review/sessions", "post") => Some(json_contract(
            "review",
            Some("ReviewSessionRequestV1"),
            "201",
            "ReviewSessionV1",
            &[
                "400", "401", "403", "404", "408", "409", "413", "422", "500", "503",
            ],
        )),
        ("/v1/review/sessions/active", "get") => Some(json_contract(
            "review",
            None,
            "200",
            "ActiveReviewSessionV1",
            &["401", "403", "408", "500", "503"],
        )),
        ("/v1/review/sessions/{id}", "get") => Some(json_contract(
            "review",
            None,
            "200",
            "ReviewSessionSummaryV1",
            &["401", "403", "404", "408", "500", "503"],
        )),
        ("/v1/review/sessions/{id}/resume", "post") => Some(json_contract(
            "review",
            None,
            "200",
            "ReviewSessionV1",
            &["400", "401", "403", "404", "408", "409", "500", "503"],
        )),
        ("/v1/review/sessions/{id}/quit", "post") | ("/v1/review/sessions/{id}/abort", "post") => {
            Some(json_contract(
                "review",
                None,
                "200",
                "ReviewSessionV1",
                &["400", "401", "403", "404", "408", "500", "503"],
            ))
        }
        ("/v1/review/sessions/{id}/attempts", "post") => Some(json_contract(
            "review",
            None,
            "201",
            "ReviewAttemptBeginResponseV1",
            &["400", "401", "403", "404", "408", "500", "503"],
        )),
        ("/v1/review/history", "get") => Some(json_contract(
            "review",
            None,
            "200",
            "ReviewHistoryPageV1",
            &["400", "401", "403", "408", "500", "503"],
        )),
        ("/v1/review/queue/summary", "post") => Some(json_contract(
            "review",
            Some("ReviewQueueSummaryRequestV1"),
            "200",
            "ReviewQueueSummaryV1",
            &["400", "401", "403", "408", "413", "422", "500", "503"],
        )),
        ("/v1/review/attempts/{id}/answer", "post") => Some(json_contract(
            "review",
            Some("ReviewAnswerV1"),
            "200",
            "ReviewAttemptV1",
            &[
                "400", "401", "403", "404", "408", "409", "412", "413", "422", "500", "503",
            ],
        )),
        ("/v1/review/attempts/{id}/cloze-check", "post") => Some(json_contract(
            "review",
            Some("ReviewClozeCheckRequestV1"),
            "200",
            "ReviewClozeCheckV1",
            &[
                "400", "401", "403", "404", "408", "412", "413", "422", "500", "503",
            ],
        )),

        ("/v1/review/attempts/{id}/commit", "post") => Some(json_contract(
            "review",
            None,
            "200",
            "ReviewCommitResponseV1",
            &["400", "401", "403", "404", "408", "412", "500", "503"],
        )),
        _ => None,
    }
}

fn online_review_parameters(path: &str) -> Vec<Value> {
    let mut parameters = openapi_path_parameters(path);
    match path {
        "/v1/dashboard" => parameters.push(json!({
            "name":"days", "in":"query", "required":false,
            "schema":{"type":"integer","minimum":1,"maximum":365,"default":30}
        })),
        "/v1/review/sessions" => {
            parameters.extend([
                json!({"name":"after","in":"query","required":false,"schema":{"type":"string"}}),
                json!({"name":"limit","in":"query","required":false,"schema":{"type":"integer","minimum":1,"maximum":MAX_PAGE,"default":100}}),
                json!({"name":"status","in":"query","required":false,"schema":{"$ref":"#/components/schemas/ReviewSessionStatusV1"}}),
                json!({"name":"mode","in":"query","required":false,"schema":{"$ref":"#/components/schemas/ReviewSessionModeV1"}}),
            ]);
        }
        "/v1/review/history" => {
            parameters.extend([
                json!({"name":"after","in":"query","required":false,"schema":{"type":"string"}}),
                json!({"name":"limit","in":"query","required":false,"schema":{"type":"integer","minimum":1,"maximum":MAX_PAGE,"default":100}}),
                json!({"name":"thema_id","in":"query","required":false,"schema":{"type":"string"}}),
                json!({"name":"session_id","in":"query","required":false,"schema":{"type":"string"}}),
                json!({"name":"from_day","in":"query","required":false,"schema":{"type":"integer","minimum":1,"maximum":99991231}}),
                json!({"name":"through_day","in":"query","required":false,"schema":{"type":"integer","minimum":1,"maximum":99991231}}),
                json!({"name":"accepted_success","in":"query","required":false,"schema":{"type":"boolean"}}),
            ]);
        }
        _ => {}
    }
    parameters
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
        &["400", "401", "403", "404", "408", "500", "503"]
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
        ("/v1/sync/identity", "get") => Some("DeviceIdentityV1"),
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
    generator.subschema_for::<DeviceIdentityV1>();
    generator.subschema_for::<ReplicaStateV1>();
    generator.subschema_for::<ContentCapabilitiesV1>();
    generator.subschema_for::<AssetCapabilitiesV1>();
    generator.subschema_for::<ReplicaChangePageV1>();
    generator.subschema_for::<ReplicaSnapshotPageV1>();
    generator.subschema_for::<ContentChangePageV1>();
    generator.subschema_for::<DocumentHeadV1>();
    generator.subschema_for::<DocumentMutationResultV1>();
    generator.subschema_for::<DesiredStateMutationResultV1>();
    generator.subschema_for::<DocumentRevisionPageV1>();
    generator.subschema_for::<DocumentRevisionSummaryV1>();
    generator.subschema_for::<AssetHeadV1>();
    generator.subschema_for::<StagedBlobV1>();
    generator.subschema_for::<ThemaPageV1>();
    generator.subschema_for::<ThemaCatalogPageV1>();
    generator.subschema_for::<ThemaV1>();
    generator.subschema_for::<NodeCatalogPageV1>();
    generator.subschema_for::<NodePageV1>();
    generator.subschema_for::<NodeV1>();
    generator.subschema_for::<NodeThemaAssociationPageV1>();
    generator.subschema_for::<SettingsV1>();
    generator.subschema_for::<DashboardV1>();
    generator.subschema_for::<DocumentView>();
    generator.subschema_for::<DuePageView>();
    generator.subschema_for::<ReviewSessionPageV1>();
    generator.subschema_for::<ReviewSessionSummaryV1>();
    generator.subschema_for::<ReviewHistoryPageV1>();
    generator.subschema_for::<ReviewClozeCheckV1>();
    generator.subschema_for::<ReviewQueueSummaryV1>();
    generator.subschema_for::<ThemaReviewStateV1>();
    generator.subschema_for::<ActiveReviewSessionV1>();
    generator.subschema_for::<ReviewSessionV1>();
    generator.subschema_for::<ReviewAttemptBeginResponseV1>();
    generator.subschema_for::<ReviewAttemptV1>();
    generator.subschema_for::<ReviewCommitResponseV1>();
    generator.subschema_for::<ReviewAttemptStateResponseV1>();
    generator.subschema_for::<ReviewAttemptOperationResponseV1>();
    generator.subschema_for::<ReviewPackEnvelopeV2>();
    generator.subschema_for::<ActiveReviewPackEnvelopeV2>();
    generator.subschema_for::<OfflineReviewResultEnvelopeV2>();
    generator.subschema_for::<OfflineContentResultEnvelopeV1>();
    generator.subschema_for::<OfflineAssetResultEnvelopeV1>();
    generator.subschema_for::<ApiErrorCodeV1>();
    generator.subschema_for::<ApiErrorResponseV1>();
    let mut definitions = generator.take_definitions(true);

    let mut requests = openapi_schema_settings().for_deserialize().into_generator();
    requests.subschema_for::<NoteWriteRequestV1>();
    requests.subschema_for::<DocumentImportRequestV1>();
    requests.subschema_for::<NoteRenameRequestV1>();
    requests.subschema_for::<ThemaWriteRequestV1>();
    requests.subschema_for::<UpdateSettingsRequestV1>();
    requests.subschema_for::<ReviewSessionRequestV1>();
    requests.subschema_for::<ReviewQueueSummaryRequestV1>();
    requests.subschema_for::<ReviewClozeCheckRequestV1>();
    requests.subschema_for::<ReviewAnswerV1>();
    requests.subschema_for::<SetAcceptedSuccessRequestV1>();
    requests.subschema_for::<SetSuspensionRequestV1>();
    requests.subschema_for::<SetThemaSuspensionRequestV1>();
    requests.subschema_for::<OfflineReviewOperationEnvelopeV1>();
    requests.subschema_for::<OfflineContentOperationEnvelopeV1>();
    requests.subschema_for::<OfflineAssetOperationEnvelopeV1>();
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
    ("/v1/dashboard", "get"),
    ("/v1/documents/{id}", "get"),
    ("/v1/documents/{id}", "put"),
    ("/v1/themata/{id}", "get"),
    ("/v1/review/due", "get"),
    ("/v1/changes", "get"),
    ("/v1/sync/identity", "get"),
    ("/v1/sync/state", "get"),
    ("/v1/sync/content-capabilities", "get"),
    ("/v1/sync/asset-capabilities", "get"),
    ("/v1/sync/blobs/{digest}", "put"),
    ("/v1/sync/changes", "get"),
    ("/v1/sync/snapshot", "get"),
    ("/v1/events", "get"),
    ("/v1/settings", "get"),
    ("/v1/settings", "put"),
    ("/v1/notes/{id}", "get"),
    ("/v1/document-mutations", "post"),
    ("/v1/document-mutations/{id}", "put"),
    ("/v1/document-mutations/{id}", "delete"),
    ("/v1/document-mutations/{id}/rename", "post"),
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
    ("/v1/themata/catalog", "get"),
    ("/v1/themata/{id}", "get"),
    ("/v1/themata/{id}", "put"),
    ("/v1/themata/{id}/review-state", "get"),
    ("/v1/themata/{id}/suspension", "put"),
    ("/v1/themata/{id}/archive", "post"),
    ("/v1/themata/{id}/unarchive", "post"),
    ("/v1/nodes", "get"),
    ("/v1/nodes/catalog", "get"),
    ("/v1/nodes/{id}", "get"),
    ("/v1/nodes/{id}/themata", "get"),
    ("/v1/nodes/{id}/inbound", "get"),
    ("/v1/nodes/{id}/outbound", "get"),
    ("/v1/review/sessions", "get"),
    ("/v1/review/sessions", "post"),
    ("/v1/review/sessions/active", "get"),
    ("/v1/review/sessions/{id}", "get"),
    ("/v1/review/sessions/{id}/resume", "post"),
    ("/v1/review/sessions/{id}/quit", "post"),
    ("/v1/review/sessions/{id}/abort", "post"),
    ("/v1/review/sessions/{id}/attempts", "post"),
    ("/v1/review/history", "get"),
    ("/v1/review/queue/summary", "post"),
    ("/v1/review/packs", "post"),
    ("/v1/review/packs/active", "get"),
    ("/v1/review/packs/{id}", "get"),
    ("/v1/review/packs/{id}/assets/{digest}/{name}", "get"),
    ("/v1/review/packs/{id}/abandon", "post"),
    ("/v1/review/offline-operations", "post"),
    ("/v1/review/attempts/{id}", "get"),
    ("/v1/review/attempts/{id}/answer", "post"),
    ("/v1/review/attempts/{id}/cloze-check", "post"),
    ("/v1/review/attempts/{id}/accepted-success", "put"),
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

    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};
    use tower::ServiceExt;

    use super::{
        ApiError, MAX_DUE_CURSOR_BYTES, MAX_FRAME_SIZE, MAX_NODE_CATALOG_PAGE, OPENAPI_OPERATIONS,
        bounded_due_page, bounded_projection_page, durable_created_thema_id,
        ensure_thema_response_budget, openapi_document, optional_if_match, prepared_thema, router,
    };
    use crate::application::{
        AnswerInput, ApplicationError, DeviceDesiredStateMutation, OfflineAssetOperation,
        OfflineReviewOperation, PresentationSnapshot, ReviewAttemptView, ReviewPackView,
        ReviewSource, ReviewSourceKind, SettingsInput, generate_device, prepare_asset,
        prepare_document, prepare_thema, presentation_content, review_day_at, thema_content,
    };
    use crate::domain::{DocumentId, DueThema, ThemaId, compute_transition, grade};
    use crate::protocol::{
        canonical_asset_operation, decode_cursor, encode_cursor,
        http_v1::{
            ApiErrorCodeV1, ApiErrorMetaV1, ApiErrorResponseV1, ContentCapabilitiesV1,
            ReplicaStateV1, ReviewAttemptV1, ThemaWriteRequestV1,
        },
    };
    use crate::service::{ServiceError, ServiceHandle, ServiceRuntime};
    use crate::store::StoreError;

    #[tokio::test]
    async fn health_fails_when_the_store_actor_is_stopped() {
        let directory = tempfile::tempdir().unwrap();
        let service = ServiceHandle::stopped_for_test(directory.path().join("blobs")).unwrap();
        let response = router(service)
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(body, r#"{"status":"unavailable"}"#);
    }

    #[test]
    fn service_errors_have_stable_http_contracts() {
        assert_api_error(
            ServiceError::Store(StoreError::TopicTraversalLimitExceeded),
            StatusCode::BAD_REQUEST,
            ApiErrorCodeV1::InvalidRequest,
            "review operation is invalid",
            None,
        );
        assert_api_error(
            ServiceError::Store(StoreError::InvalidReviewResponse),
            StatusCode::BAD_REQUEST,
            ApiErrorCodeV1::InvalidRequest,
            "review response is invalid",
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
    fn created_thema_ids_are_stable_and_device_scoped() {
        assert_eq!(
            durable_created_thema_id("device", "operation"),
            durable_created_thema_id("device", "operation")
        );
        assert_ne!(
            durable_created_thema_id("device-a", "operation"),
            durable_created_thema_id("device-b", "operation")
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
    fn openapi_exposes_typed_thema_concepts_without_legacy_node_ids() {
        let api = openapi_document();
        let schemas = &api["components"]["schemas"];
        assert_eq!(
            schemas["ThemaConceptRelationV1"]["enum"],
            json!(["primary", "supporting", "reference"])
        );
        assert_eq!(schemas["ThemaConceptV1"]["additionalProperties"], false);
        assert_eq!(
            schemas["ThemaConceptV1"]["required"],
            json!(["node_id", "relation"])
        );
        for schema in ["ThemaWriteRequestV1", "ThemaV1"] {
            let properties = schemas[schema]["properties"].as_object().unwrap();
            assert!(properties.contains_key("concepts"), "{schema}");
            assert!(!properties.contains_key("node_ids"), "{schema}");
            assert_eq!(
                properties["concepts"]["items"]["$ref"],
                "#/components/schemas/ThemaConceptV1"
            );
        }
        assert!(
            !schemas["NodeV1"]["properties"]
                .as_object()
                .unwrap()
                .contains_key("thema_ids")
        );
        assert_eq!(
            schemas["NodeThemaAssociationV1"]["required"],
            json!(["thema_id", "relation"])
        );
        assert_eq!(
            api["paths"]["/v1/nodes/{id}/themata"]["get"]["responses"]["200"]["content"]["application/json"]
                ["schema"]["$ref"],
            "#/components/schemas/NodeThemaAssociationPageV1"
        );
    }

    #[test]
    fn openapi_types_due_query_exactly() {
        let api = openapi_document();
        let operation = &api["paths"]["/v1/review/due"]["get"];
        assert_eq!(
            operation["x-gnosis-required-scopes"],
            json!(["read", "review"])
        );
        assert!(operation["responses"].get("422").is_some());
        assert_eq!(
            operation["parameters"],
            json!([
                {"name":"day","in":"query","required":true,"description":"Valid YYYYMMDD calendar date","schema":{"type":"integer","format":"uint32","minimum":10101}},
                {"name":"limit","in":"query","required":false,"schema":{"type":"integer","format":"uint16","minimum":1,"maximum":100,"default":100}},
                {"name":"after","in":"query","required":false,"schema":{"type":"string","minLength":1,"maxLength":MAX_DUE_CURSOR_BYTES}}
            ])
        );
        let schemas = &api["components"]["schemas"];
        assert_eq!(
            schemas["DueThemaView"]["required"],
            json!(["id", "next_review_day", "review_count"])
        );
        assert_eq!(
            schemas["DueThemaView"]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            ["id", "next_review_day", "review_count"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );
        for schema in [
            "DocumentView",
            "NodeView",
            "LinkOccurrenceView",
            "NodeEdgeView",
            "DueThemaView",
            "DuePageView",
        ] {
            assert_eq!(
                api["components"]["schemas"][schema]["additionalProperties"],
                false
            );
        }

        assert_eq!(
            api["components"]["schemas"]["DocumentView"]["properties"]["content_sha256"]["maxLength"],
            64
        );
        assert_eq!(
            schemas["DocumentView"]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            [
                "content_sha256",
                "document_id",
                "file_node_id",
                "file_tags",
                "link_occurrences",
                "nodes",
                "revision",
                "semantic_edges",
                "title",
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            schemas["DocumentProjectionV1"]["properties"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect::<BTreeSet<_>>(),
            [
                "content_sha256",
                "file_node_id",
                "file_tags",
                "link_occurrences",
                "nodes",
                "semantic_edges",
                "title",
            ]
            .into_iter()
            .collect()
        );
        for (name, field) in [
            ("DocumentView", "file_node_id"),
            ("NodeView", "parent_id"),
            ("NodeView", "start_byte"),
            ("NodeView", "end_byte"),
            ("ProjectionNodeV1", "start_byte"),
            ("ProjectionNodeV1", "end_byte"),
            ("NodeV1", "start_byte"),
            ("NodeV1", "end_byte"),
            ("DuePageView", "next"),
        ] {
            assert!(
                schemas[name]["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(field)),
                "{name}.{field}"
            );
        }
        for field in [
            "document_revision",
            "content_sha256",
            "start_byte",
            "end_byte",
        ] {
            assert!(
                schemas["NodeV1"]["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!(field)),
                "NodeV1.{field}"
            );
        }
        assert_eq!(
            response_statuses(&api["paths"]["/v1/documents/{id}"]["get"]),
            set(&[
                "200", "400", "401", "403", "404", "408", "422", "500", "503"
            ])
        );
    }

    #[test]
    fn openapi_types_node_catalog_query_exactly() {
        let api = openapi_document();
        assert_eq!(
            api["paths"]["/v1/nodes/catalog"]["get"]["parameters"],
            json!([
                {"name":"after","in":"query","required":false,"schema":{"type":"string"}},
                {"name":"limit","in":"query","required":false,"schema":{"type":"integer","minimum":1,"maximum":MAX_NODE_CATALOG_PAGE,"default":MAX_NODE_CATALOG_PAGE}},
                {"name":"document_id","in":"query","required":false,"schema":{"type":"string"}}
            ])
        );
    }

    #[test]
    fn thema_response_budget_rejects_request_that_would_overflow_response() {
        let mut input = ThemaWriteRequestV1 {
            kind: "basic".into(),
            keimenon: String::new(),
            hypothesis: vec![],
            answer: vec!["answer".into()],
            source_guid: None,
            parathema: None,
            review_image: None,
            tags: vec![],
            concepts: vec![],
            asset_paths: vec![],
        };
        let base_size = serde_json::to_vec(&input).unwrap().len();
        input.keimenon = "x".repeat(MAX_FRAME_SIZE - base_size);

        assert!(serde_json::to_vec(&input).unwrap().len() <= MAX_FRAME_SIZE);
        assert!(ensure_thema_response_budget(&input).is_err());
    }

    #[test]
    fn openapi_has_no_legacy_projection_aliases() {
        let api = openapi_document();
        assert!(api["paths"]["/v1/themata/{id}"].is_object());
        assert!(api["paths"]["/v1/review/due"].is_object());
        assert!(api["paths"]["/v1/legacy-themata/{id}"].is_null());
        assert!(api["paths"]["/v1/legacy-review/due"].is_null());
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
                .contains(&json!("next_device_sequence"))
        );
        assert!(
            schemas["ReplicaStateV1"]["properties"]
                .get("next_review_sequence")
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
        assert!(
            schemas["RevisionErrorMetaSchemaV1"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("current_revision"))
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
    fn openapi_types_authenticated_sync_identity_exactly() {
        let api = openapi_document();
        let identity = &api["paths"]["/v1/sync/identity"]["get"];
        assert_eq!(
            response_statuses(identity),
            set(&["200", "401", "403", "408", "500", "503"])
        );
        assert_eq!(identity["x-gnosis-required-scopes"], json!(["read"]));
        assert_eq!(
            identity["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/DeviceIdentityV1"
        );
        let schema = &api["components"]["schemas"]["DeviceIdentityV1"];
        assert_eq!(schema["properties"]["version"]["const"], 1);
        assert_eq!(schema["properties"]["device_id"]["minLength"], 1);
        assert_eq!(schema["properties"]["device_id"]["maxLength"], 128);
    }

    #[test]
    fn openapi_types_review_attempt_operations_exactly() {
        let api = openapi_document();
        let required = api["components"]["schemas"]["ReviewStateV1"]["required"]
            .as_array()
            .unwrap();
        assert!(required.contains(&json!("stability")));
        assert!(required.contains(&json!("difficulty")));
        let sessions = &api["paths"]["/v1/review/sessions"]["post"];
        assert_eq!(sessions["x-gnosis-required-scopes"], json!(["review"]));
        assert_eq!(
            sessions["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ReviewSessionRequestV1"
        );
        let target_count =
            &api["components"]["schemas"]["ReviewSessionRequestV1"]["properties"]["target_count"];
        assert_eq!(target_count["minimum"], 1);
        assert_eq!(target_count["maximum"], 500);
        assert_eq!(target_count["default"], 100);
        let source_input = &api["components"]["schemas"]["ReviewSourceInputV1"]["properties"];
        assert_eq!(source_input["topic_forward_depth"]["maximum"], 32);
        assert_eq!(source_input["topic_backward_depth"]["maximum"], 32);
        assert_eq!(source_input["manual_ids"]["maxItems"], 500);
        let source = &api["components"]["schemas"]["ReviewSourceV1"]["properties"];
        assert_eq!(source["topic_forward_depth"]["maximum"], 32);
        assert_eq!(source["topic_backward_depth"]["maximum"], 32);
        assert_eq!(source["manual_ids"]["maxItems"], 500);
        assert_eq!(
            sessions["responses"]["201"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ReviewSessionV1"
        );
        assert!(sessions["responses"]["404"].is_object());
        let active = &api["paths"]["/v1/review/sessions/active"]["get"];
        assert_eq!(
            active["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ActiveReviewSessionV1"
        );
        let begin = &api["paths"]["/v1/review/sessions/{id}/attempts"]["post"];
        assert_eq!(
            begin["responses"]["201"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ReviewAttemptBeginResponseV1"
        );
        let answer = &api["paths"]["/v1/review/attempts/{id}/answer"]["post"];
        assert_eq!(
            answer["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ReviewAnswerV1"
        );
        assert_eq!(
            answer["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ReviewAttemptV1"
        );
        assert!(answer["responses"]["412"].is_object());
        assert!(
            api["paths"]
                .get("/v1/review/attempts/{id}/override")
                .is_none()
        );
        let commit = &api["paths"]["/v1/review/attempts/{id}/commit"]["post"];
        assert_eq!(
            commit["responses"]["200"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ReviewCommitResponseV1"
        );
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
        assert!(suspension.get("post").is_none());
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
        let attempt_cases = schemas["ReviewAttemptV1"]["oneOf"].as_array().unwrap();
        assert_eq!(attempt_cases.len(), 3);
        assert_eq!(attempt_cases[0]["properties"]["status"]["const"], "pending");
        assert_eq!(
            attempt_cases[0]["properties"]["proposed_transition"]["type"],
            "null"
        );
        assert_eq!(attempt_cases[1]["properties"]["status"]["const"], "graded");
        assert_eq!(
            attempt_cases[2]["properties"]["status"]["const"],
            "committed"
        );
        let presentation_case = |attempt_index: usize, kind: &str| {
            attempt_cases[attempt_index]["properties"]["presentation"]["oneOf"]
                .as_array()
                .unwrap()
                .iter()
                .find(|case| case["properties"]["kind"]["const"] == kind)
                .unwrap()
        };
        let pending_cloze = presentation_case(0, "cloze");
        assert_eq!(
            pending_cloze["properties"]["response"]["properties"]["kind"]["const"],
            "texts"
        );
        assert_eq!(
            pending_cloze["properties"]["cloze_spans"]["items"]["properties"]["state"]["const"],
            "concealed"
        );
        let graded_cloze = presentation_case(1, "cloze");
        assert_eq!(
            graded_cloze["properties"]["cloze_spans"]["items"]["properties"]["state"]["enum"],
            json!(["correct", "incorrect", "unanswered"])
        );
        let pending_basic = presentation_case(0, "basic");
        assert_eq!(pending_basic["properties"]["cloze_spans"]["maxItems"], 0);
        let graded_mc_cloze = presentation_case(2, "mc-cloze");
        assert_eq!(graded_mc_cloze["properties"]["cloze_spans"]["maxItems"], 1);
        assert_eq!(
            graded_mc_cloze["properties"]["cloze_spans"]["items"]["properties"]["state"]["enum"],
            json!(["correct", "incorrect"])
        );
        assert_eq!(
            schemas["ThemaKindV1"]["enum"],
            json!(["basic", "cloze", "mcq", "mc-cloze"])
        );
        let presentation = &schemas["ReviewPresentationV1"];
        assert!(presentation["properties"].get("keimenon").is_none());
        assert!(presentation["properties"].get("hypothesis").is_none());
        assert_eq!(
            presentation["properties"]["hint"]["type"],
            json!(["string", "null"])
        );
        assert!(
            presentation["required"]
                .as_array()
                .unwrap()
                .contains(&json!("hint"))
        );
        assert_eq!(
            presentation["properties"]["response"]["$ref"],
            "#/components/schemas/ReviewResponsePromptV1"
        );
        assert_eq!(
            presentation["properties"]["cloze_spans"]["items"]["$ref"],
            "#/components/schemas/ReviewClozeSpanV1"
        );
        assert_eq!(
            schemas["ReviewClozeStateV1"]["enum"],
            json!(["concealed", "correct", "incorrect", "unanswered"])
        );
        assert!(
            presentation["required"]
                .as_array()
                .unwrap()
                .contains(&json!("cloze_spans"))
        );
        assert_eq!(
            schemas["ReviewResponsePromptV1"]["oneOf"]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        let input_script =
            &schemas["ReviewResponsePromptV1"]["oneOf"][0]["properties"]["input_script"];
        assert_eq!(input_script["type"], json!(["string", "null"]));
        assert!(
            schemas["ReviewResponsePromptV1"]["oneOf"][0]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("input_script"))
        );
        assert_eq!(
            schemas["ReviewResponsePromptV1"]["oneOf"][1]["properties"]["input_scripts"]["minItems"],
            1
        );
        assert_eq!(
            schemas["ReviewResponsePromptV1"]["oneOf"][2]["properties"]["choices"]["minItems"],
            2
        );
        assert_local_refs_resolve(&api, &api);
    }

    #[test]
    fn openapi_types_client_read_views_exactly() {
        let api = openapi_document();
        let paths = &api["paths"];
        let dashboard = &paths["/v1/dashboard"]["get"];
        assert_eq!(
            dashboard["x-gnosis-required-scopes"],
            json!(["read", "review"])
        );
        assert_eq!(
            operation_parameter(dashboard, "days")["schema"]["maximum"],
            365
        );
        assert_response_schema(dashboard, "200", "DashboardV1");
        let sessions = &paths["/v1/review/sessions"]["get"];
        assert_response_schema(sessions, "200", "ReviewSessionPageV1");
        assert_eq!(
            operation_parameter(sessions, "status")["schema"]["$ref"],
            "#/components/schemas/ReviewSessionStatusV1"
        );
        assert_response_schema(
            &paths["/v1/review/sessions/{id}"]["get"],
            "200",
            "ReviewSessionSummaryV1",
        );
        let history = &paths["/v1/review/history"]["get"];
        assert_response_schema(history, "200", "ReviewHistoryPageV1");
        assert_eq!(
            operation_parameter(history, "accepted_success")["schema"]["type"],
            "boolean"
        );
        let queue = &paths["/v1/review/queue/summary"]["post"];
        assert_eq!(
            queue["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ReviewQueueSummaryRequestV1"
        );
        assert_response_schema(queue, "200", "ReviewQueueSummaryV1");
        assert_response_schema(
            &paths["/v1/themata/{id}/review-state"]["get"],
            "200",
            "ThemaReviewStateV1",
        );
        let cloze = &paths["/v1/review/attempts/{id}/cloze-check"]["post"];
        assert_eq!(
            cloze["requestBody"]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ReviewClozeCheckRequestV1"
        );
        assert_response_schema(cloze, "200", "ReviewClozeCheckV1");
        assert_eq!(
            api["components"]["schemas"]["ReviewClozeCheckRequestV1"]["properties"]["inputs"]["maxItems"],
            500
        );
        assert_eq!(
            api["components"]["schemas"]["ReviewClozeCheckRequestV1"]["properties"]["inputs"]["minItems"],
            1
        );
        let texts_answer = api["components"]["schemas"]["ReviewAnswerV1"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| variant["properties"]["kind"]["const"] == "texts")
            .unwrap();
        assert_eq!(
            texts_answer["properties"]["value"]["maxItems"],
            crate::domain::MAX_REVIEW_CLOZE_INPUTS
        );
        let text_prompt = api["components"]["schemas"]["ReviewResponsePromptV1"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .find(|variant| variant["properties"].get("input_scripts").is_some())
            .unwrap();
        assert_eq!(
            text_prompt["properties"]["input_scripts"]["maxItems"],
            crate::domain::MAX_REVIEW_CLOZE_INPUTS
        );
        let event = &api["components"]["schemas"]["ReviewHistoryEventV1"];
        assert!(event["properties"].get("response").is_none());
        assert_local_refs_resolve(&api, &api);
    }

    #[test]
    fn openapi_online_review_response_statuses_are_exact() {
        let api = openapi_document();
        let paths = &api["paths"];
        assert_response_statuses(
            &paths["/v1/review/attempts/{id}"]["get"],
            &["200", "400", "401", "403", "404", "408", "500", "503"],
        );
        assert_response_statuses(
            &paths["/v1/review/sessions"]["post"],
            &[
                "201", "400", "401", "403", "404", "408", "409", "413", "422", "500", "503",
            ],
        );
        assert_response_statuses(
            &paths["/v1/review/sessions/active"]["get"],
            &["200", "401", "403", "408", "500", "503"],
        );
        assert_response_statuses(
            &paths["/v1/review/sessions/{id}/resume"]["post"],
            &[
                "200", "400", "401", "403", "404", "408", "409", "500", "503",
            ],
        );
        for action in ["quit", "abort"] {
            assert_response_statuses(
                &paths[&format!("/v1/review/sessions/{{id}}/{action}")]["post"],
                &["200", "400", "401", "403", "404", "408", "500", "503"],
            );
        }
        assert_response_statuses(
            &paths["/v1/review/sessions/{id}/attempts"]["post"],
            &["201", "400", "401", "403", "404", "408", "500", "503"],
        );
        assert_response_statuses(
            &paths["/v1/review/attempts/{id}/answer"]["post"],
            &[
                "200", "400", "401", "403", "404", "408", "409", "412", "413", "422", "500", "503",
            ],
        );
        assert!(paths.get("/v1/review/attempts/{id}/override").is_none());
        assert_response_statuses(
            &paths["/v1/review/attempts/{id}/commit"]["post"],
            &[
                "200", "400", "401", "403", "404", "408", "412", "500", "503",
            ],
        );
    }

    #[test]
    fn openapi_offline_review_contracts_are_exact() {
        let api = openapi_document();
        let required = api["components"]["schemas"]["OfflineReviewStateV1"]["required"]
            .as_array()
            .unwrap();
        assert!(required.contains(&json!("stability")));
        assert!(required.contains(&json!("difficulty")));
        let paths = &api["paths"];
        let create = &paths["/v1/review/packs"]["post"];
        assert_eq!(create["x-gnosis-required-scopes"], json!(["review"]));
        assert_schema_ref(create, "requestBody", "ReviewSessionRequestV1");
        assert_response_schema(create, "201", "ReviewPackEnvelopeV2");
        assert_response_statuses(
            create,
            &[
                "201", "400", "401", "403", "404", "408", "409", "413", "422", "500", "503",
            ],
        );
        let active = &paths["/v1/review/packs/active"]["get"];
        assert_response_schema(active, "200", "ActiveReviewPackEnvelopeV2");
        assert_response_statuses(active, &["200", "401", "403", "408", "500", "503"]);
        let get = &paths["/v1/review/packs/{id}"]["get"];
        assert_response_schema(get, "200", "ReviewPackEnvelopeV2");
        assert_response_statuses(get, &["200", "401", "403", "404", "408", "500", "503"]);
        let abandon = &paths["/v1/review/packs/{id}/abandon"]["post"];
        assert_response_schema(abandon, "200", "ReviewPackEnvelopeV2");
        assert_response_statuses(
            abandon,
            &["200", "400", "401", "403", "404", "408", "500", "503"],
        );
        assert_offline_review_operation_contract(paths);
        assert_offline_review_asset_contract(paths);
        assert_offline_review_schemas(&api["components"]["schemas"]);
    }

    #[test]
    fn openapi_offline_content_contract_is_exact() {
        let api = openapi_document();
        let operation = &api["paths"]["/v1/content/offline-operations"]["post"];
        assert_eq!(
            operation["x-gnosis-required-scopes"],
            json!(["content:write"])
        );
        assert_schema_ref(
            operation,
            "requestBody",
            "OfflineContentOperationEnvelopeV1",
        );
        assert_response_schema(operation, "200", "OfflineContentResultEnvelopeV1");
        assert_response_statuses(
            operation,
            &[
                "200", "400", "401", "403", "408", "409", "413", "422", "500", "503",
            ],
        );
        let schemas = &api["components"]["schemas"];
        assert_eq!(
            schemas["OfflineContentOperationEnvelopeV1"]["properties"]["version"]["const"],
            1
        );
        assert_eq!(
            schemas["OfflineContentResultEnvelopeV1"]["properties"]["version"]["const"],
            1
        );
        assert_tagged_variants(
            &schemas["OfflineContentMutationV1"],
            "kind",
            &["put_document", "rename_document", "tombstone_document"],
        );
        assert_tagged_variants(
            &schemas["OfflineContentBaseV1"],
            "kind",
            &["absent", "revision"],
        );
        assert_eq!(
            schemas["OfflineContentConflictReasonV1"]["enum"],
            json!([
                "cursor_changed",
                "base_changed",
                "invalid_content",
                "path_conflict"
            ])
        );
        assert_offline_content_result_schema(schemas);
    }

    #[test]
    fn openapi_offline_asset_contract_is_exact() {
        let api = openapi_document();
        let operation = &api["paths"]["/v1/content/offline-asset-operations"]["post"];
        assert_eq!(
            operation["x-gnosis-required-scopes"],
            json!(["content:write"])
        );
        assert_schema_ref(operation, "requestBody", "OfflineAssetOperationEnvelopeV1");
        assert_response_schema(operation, "200", "OfflineAssetResultEnvelopeV1");
        assert_response_statuses(
            operation,
            &[
                "200", "400", "401", "403", "408", "409", "413", "422", "500", "503",
            ],
        );
        let schemas = &api["components"]["schemas"];
        assert_eq!(
            schemas["OfflineAssetOperationEnvelopeV1"]["properties"]["version"]["const"],
            1
        );
        assert_eq!(
            schemas["OfflineAssetResultEnvelopeV1"]["properties"]["version"]["const"],
            1
        );
        assert_eq!(
            schemas["OfflineAssetMutationKindV1"]["enum"],
            json!(["put_asset"])
        );
        assert_tagged_variants(&schemas["OfflineAssetBaseV1"], "kind", &["absent"]);
        assert_eq!(
            schemas["OfflineAssetConflictReasonV1"]["enum"],
            json!([
                "cursor_changed",
                "base_changed",
                "invalid_content",
                "path_conflict"
            ])
        );
        let size = &schemas["OfflineAssetOperationV1"]["properties"]["size_bytes"];
        assert_eq!(size["minimum"], 1);
        assert_eq!(size["maximum"], 1_048_576);
        let operation = &schemas["OfflineAssetOperationV1"]["properties"];
        assert_eq!(operation["logical_path"]["maxLength"], 327);
        assert_eq!(operation["original_name"]["maxLength"], 255);
        assert_eq!(
            schemas["OfflineAssetProjectionV1"]["properties"]["active"]["const"],
            true
        );
        assert_offline_asset_result_schema(schemas);
    }

    fn assert_offline_asset_result_schema(schemas: &Value) {
        let results = schemas["OfflineAssetResultV1"]["oneOf"].as_array().unwrap();
        assert_eq!(
            results
                .iter()
                .map(|schema| schema["properties"]["status"]["const"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["accepted", "conflict"]
        );
        assert_eq!(results[0]["properties"]["conflict_reason"]["type"], "null");
        for result in results {
            for required in ["conflict_reason", "authoritative"] {
                assert!(
                    result["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(required))
                );
            }
        }
    }

    fn assert_offline_content_result_schema(schemas: &Value) {
        let results = schemas["OfflineContentResultV1"]["oneOf"]
            .as_array()
            .unwrap();
        assert_eq!(
            results
                .iter()
                .map(|schema| schema["properties"]["status"]["const"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["accepted", "conflict"]
        );
        assert_eq!(results[0]["properties"]["conflict_reason"]["type"], "null");
        assert!(
            results[0]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("conflict_reason"))
        );
        assert!(
            results[1]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("authoritative"))
        );
        assert!(
            schemas["OfflineContentDocumentV1"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("content_sha256"))
        );
    }

    fn assert_tagged_variants(schema: &Value, tag: &str, expected: &[&str]) {
        let actual = schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|variant| variant["properties"][tag]["const"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }

    fn assert_offline_review_operation_contract(paths: &Value) {
        let operation = &paths["/v1/review/offline-operations"]["post"];
        assert_schema_ref(operation, "requestBody", "OfflineReviewOperationEnvelopeV1");
        assert_response_schema(operation, "200", "OfflineReviewResultEnvelopeV2");
        assert_response_statuses(
            operation,
            &[
                "200", "400", "401", "403", "404", "408", "409", "413", "422", "500", "503",
            ],
        );
    }

    fn assert_offline_review_asset_contract(paths: &Value) {
        let asset = &paths["/v1/review/packs/{id}/assets/{digest}/{name}"]["get"];
        assert_eq!(asset["x-gnosis-required-scopes"], json!(["review"]));
        assert_eq!(
            asset["responses"]["200"]["content"]["*/*"]["schema"],
            json!({"type":"string","format":"binary"})
        );
        let digest = asset["parameters"]
            .as_array()
            .unwrap()
            .iter()
            .find(|parameter| parameter["name"] == "digest")
            .unwrap();
        assert_eq!(digest["schema"]["pattern"], "^[0-9a-f]{64}$");
        assert_response_statuses(
            asset,
            &["200", "401", "403", "404", "408", "422", "500", "503"],
        );
    }

    fn assert_offline_review_schemas(schemas: &Value) {
        assert_eq!(
            schemas["OfflineReviewOperationEnvelopeV1"]["properties"]["version"]["const"],
            1
        );
        assert_eq!(
            schemas["ReviewPackEnvelopeV2"]["properties"]["version"]["const"],
            2
        );
        assert_eq!(
            schemas["ActiveReviewPackEnvelopeV2"]["properties"]["version"]["const"],
            2
        );
        assert_eq!(
            schemas["OfflineReviewResultEnvelopeV2"]["properties"]["version"]["const"],
            2
        );
        assert_eq!(
            schemas["ReviewPackStatusV2"]["enum"],
            json!(["active", "completed", "abandoned"])
        );
        assert_eq!(
            schemas["OfflineThemaKindV1"]["enum"],
            json!(["basic", "cloze", "mcq", "mc-cloze"])
        );
        assert_eq!(
            schemas["ReviewPackV2"]["properties"]["items"]["minItems"],
            1
        );
        let size = &schemas["ReviewPackAssetV2"]["properties"]["size_bytes"];
        assert_eq!(size["minimum"], 0);
        assert_eq!(size["maximum"], crate::application::MAX_OFFLINE_ASSET_BYTES);
        let results = schemas["OfflineReviewResultV2"]["oneOf"]
            .as_array()
            .unwrap();
        assert_eq!(results.len(), 2);
        for (schema, status, null_field) in [
            (&results[0], "accepted", "conflict_reason"),
            (&results[1], "conflict", "event_id"),
        ] {
            assert_eq!(schema["properties"]["status"]["const"], status);
            assert_eq!(schema["properties"][null_field]["type"], "null");
            for required in ["conflict_reason", "event_id"] {
                assert!(
                    schema["required"]
                        .as_array()
                        .unwrap()
                        .contains(&json!(required))
                );
            }
        }
    }

    fn assert_schema_ref(operation: &Value, location: &str, schema: &str) {
        assert_eq!(
            operation[location]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{schema}")
        );
    }

    fn assert_response_schema(operation: &Value, status: &str, schema: &str) {
        assert_eq!(
            operation["responses"][status]["content"]["application/json"]["schema"]["$ref"],
            format!("#/components/schemas/{schema}")
        );
    }

    #[test]
    fn openapi_types_content_resources_exactly() {
        let api = openapi_document();
        let contracts = [
            expected_json_contract(
                "/v1/documents/{id}",
                "put",
                "content:write",
                Some("DocumentImportRequestV1"),
                "200",
                "DocumentMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "413", "422", "500", "503",
                ],
            ),
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
                "DesiredStateMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "413", "422", "428", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/document-mutations",
                "post",
                "content:write",
                Some("NoteWriteRequestV1"),
                "200",
                "DocumentMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "413", "422", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/document-mutations/{id}",
                "put",
                "content:write",
                Some("NoteWriteRequestV1"),
                "200",
                "DocumentMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "413", "422", "428", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/document-mutations/{id}",
                "delete",
                "content:write",
                None,
                "200",
                "DocumentMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "428", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/document-mutations/{id}/rename",
                "post",
                "content:write",
                Some("NoteRenameRequestV1"),
                "200",
                "DocumentMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "413", "422", "428", "500", "503",
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
                "200",
                "DesiredStateMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "413", "422", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/assets/{digest}/{name}",
                "delete",
                "content:write",
                None,
                "200",
                "DesiredStateMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "422", "428", "500", "503",
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
                "200",
                "DesiredStateMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "413", "422", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/themata/catalog",
                "get",
                "read",
                None,
                "200",
                "ThemaCatalogPageV1",
                &["200", "400", "401", "403", "408", "500", "503"],
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
                "DesiredStateMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "413", "422", "428", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/themata/{id}/archive",
                "post",
                "content:write",
                None,
                "200",
                "DesiredStateMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "422", "428", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/themata/{id}/unarchive",
                "post",
                "content:write",
                None,
                "200",
                "DesiredStateMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "422", "428", "500", "503",
                ],
            ),
            expected_json_contract(
                "/v1/themata/{id}/suspension",
                "put",
                "review",
                Some("SetThemaSuspensionRequestV1"),
                "200",
                "DesiredStateMutationResultV1",
                &[
                    "200", "400", "401", "403", "408", "409", "413", "422", "428", "500", "503",
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
                "/v1/nodes/catalog",
                "get",
                "read",
                None,
                "200",
                "NodeCatalogPageV1",
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
        assert_required_header(&api, "/v1/document-mutations", "post", "Idempotency-Key");
        assert_required_header(
            &api,
            "/v1/document-mutations/{id}",
            "put",
            "Idempotency-Key",
        );
        assert_required_header(&api, "/v1/document-mutations/{id}", "put", "If-Match");
        assert_required_header(
            &api,
            "/v1/document-mutations/{id}",
            "delete",
            "Idempotency-Key",
        );
        assert_required_header(&api, "/v1/document-mutations/{id}", "delete", "If-Match");
        assert_required_header(
            &api,
            "/v1/document-mutations/{id}/rename",
            "post",
            "Idempotency-Key",
        );
        assert_required_header(
            &api,
            "/v1/document-mutations/{id}/rename",
            "post",
            "If-Match",
        );
        assert_optional_header(&api, "/v1/assets/{digest}/{name}", "put", "If-Match");
        assert_required_header(&api, "/v1/assets/{digest}/{name}", "put", "Idempotency-Key");
        assert_required_header(&api, "/v1/assets/{digest}/{name}", "delete", "If-Match");
        assert_required_header(
            &api,
            "/v1/assets/{digest}/{name}",
            "delete",
            "Idempotency-Key",
        );
        assert_required_header(&api, "/v1/themata", "post", "Idempotency-Key");
        assert_required_header(&api, "/v1/themata/{id}", "put", "If-Match");
        assert_required_header(&api, "/v1/themata/{id}", "put", "Idempotency-Key");
        assert_required_header(&api, "/v1/themata/{id}/archive", "post", "If-Match");
        assert_required_header(&api, "/v1/themata/{id}/archive", "post", "Idempotency-Key");
        assert_required_header(&api, "/v1/themata/{id}/unarchive", "post", "If-Match");
        assert_required_header(
            &api,
            "/v1/themata/{id}/unarchive",
            "post",
            "Idempotency-Key",
        );
        assert_required_header(&api, "/v1/themata/{id}/suspension", "put", "If-Match");
        assert_required_header(
            &api,
            "/v1/themata/{id}/suspension",
            "put",
            "Idempotency-Key",
        );
        assert_required_header(&api, "/v1/settings", "put", "Idempotency-Key");
        assert_eq!(
            operation_parameter(&api["paths"]["/v1/settings"]["put"], "If-Match")["schema"]["pattern"],
            "^(?:[0-9]+|\"[0-9]+\")$"
        );
        let mutation = &api["components"]["schemas"]["DocumentMutationResultV1"]["oneOf"];
        for result in mutation.as_array().unwrap() {
            assert_eq!(
                result["properties"]["operation_id"]["pattern"],
                "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$"
            );
        }

        let changes = &api["paths"]["/v1/changes"]["get"];
        let after = operation_parameter(changes, "after");
        assert_eq!(after["schema"]["pattern"], "^[0-9]+$");
        assert_eq!(after["schema"]["default"], "0");
        let limit = operation_parameter(changes, "limit");
        assert_eq!(limit["schema"]["maximum"], 500);
        assert_eq!(limit["schema"]["default"], 100);

        let revision = &api["paths"]["/v1/notes/{id}/revisions/{revision}"]["get"];
        assert_eq!(
            revision["responses"]["200"]["content"]["*/*"]["schema"]["type"],
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
        assert_eq!(note["content"]["*/*"]["schema"]["type"], "string");
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
        let mut obsolete_scheduler_schemas = schemas
            .as_object()
            .unwrap()
            .keys()
            .filter(|name| name.contains("Scheduler") || name.contains("TagRule"))
            .map(String::as_str)
            .collect::<Vec<_>>();
        obsolete_scheduler_schemas.sort_unstable();
        assert!(
            obsolete_scheduler_schemas.is_empty(),
            "{obsolete_scheduler_schemas:?}"
        );
        assert_eq!(
            schemas["ThemaWriteRequestV1"]["properties"]["kind"]["pattern"],
            "^(basic|cloze|mcq|mc-cloze)$"
        );
        assert!(
            schemas["UpdateSettingsRequestV1"]["required"]
                .as_array()
                .unwrap()
                .contains(&json!("desired_retention"))
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
            "concepts",
            "asset_paths",
        ] {
            assert!(!thema_required.contains(&json!(optional)), "{optional}");
        }
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

    #[test]
    fn due_pages_fit_the_shared_projection_budget() {
        let due = |id: &str, bytes: usize| DueThema {
            id: ThemaId::new(id.to_owned()).unwrap(),
            kind: "basic".into(),
            keimenon: "x".repeat(bytes),
            hypothesis: Vec::new(),
            answer: vec!["answer".into()],
            next_review_day: 1,
            review_count: 0,
        };
        let page = bounded_due_page(
            vec![due("a", 600_000), due("b", 600_000)],
            2,
            20260718,
            "server".into(),
        )
        .unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(page.next.is_none());
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_FRAME_SIZE);
    }

    #[test]
    fn projection_pages_shrink_to_the_shared_budget() {
        let items = vec!["x".repeat(600_000), "y".repeat(600_000)];
        let page = bounded_projection_page(
            &items,
            |items| {
                Ok(json!({
                    "items": items,
                    "next": items.last().map(|_| "cursor"),
                }))
            },
            "item too large",
        )
        .unwrap();
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        assert_eq!(page["next"], "cursor");
        assert!(serde_json::to_vec(&page).unwrap().len() <= MAX_FRAME_SIZE);

        let oversized = vec!["x".repeat(MAX_FRAME_SIZE)];
        assert!(
            bounded_projection_page(
                &oversized,
                |items| Ok(json!({ "items": items, "next": null })),
                "item too large",
            )
            .is_err()
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocking_import_preparation_does_not_stall_health() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let app = router(runtime.handle());
        let (started_tx, mut started_rx) = tokio::sync::mpsc::channel(1);
        let (release_tx, mut release_rx) = tokio::sync::mpsc::channel(1);
        let preparation = tokio::spawn(super::run_blocking(move || {
            started_tx.blocking_send(()).unwrap();
            release_rx.blocking_recv().unwrap();
            Ok(())
        }));

        started_rx.recv().await.unwrap();
        let health = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            app.oneshot(Request::get("/health").body(Body::empty()).unwrap()),
        )
        .await
        .expect("health request stalled behind blocking preparation")
        .unwrap();
        assert_eq!(health.status(), StatusCode::OK);

        release_tx.send(()).await.unwrap();
        preparation.await.unwrap().unwrap();
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

        let missing_node_themata_auth = app
            .clone()
            .oneshot(
                Request::get("/v1/nodes/missing/themata")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing_node_themata_auth.status(), StatusCode::UNAUTHORIZED);

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

        let missing_node_themata = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/v1/nodes/missing/themata",
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(missing_node_themata.status(), StatusCode::NOT_FOUND);

        let malformed_node_themata_cursor = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/v1/nodes/missing/themata?after=%20",
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(
            malformed_node_themata_cursor.status(),
            StatusCode::BAD_REQUEST
        );

        let settings = serde_json::to_vec(&json!({
            "timezone": "UTC",
            "day_boundary_hour": 0,
            "desired_retention": 0.9
        }))
        .unwrap();
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
        assert_eq!(oversized.status(), StatusCode::NOT_FOUND);

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
    async fn node_catalog_route_returns_compact_filterable_records() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device("catalog".into(), vec!["read".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        for (document_id, path, node_id, title) in [
            ("document-a", "notes/a.org", "node-a", "Alpha"),
            ("document-b", "notes/b.org", "node-b", "Beta"),
        ] {
            let prepared = prepare_document(
                Some(document_id.into()),
                path.into(),
                "text/org; charset=utf-8".into(),
                format!(":PROPERTIES:\n:ID: {node_id}\n:END:\n#+title: {title}\n"),
                None,
            )
            .unwrap();
            service.replace_note(prepared, 2, None).await.unwrap();
        }
        let app = router(service.clone());

        let response = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/v1/nodes/catalog?document_id=document-a&limit=5000",
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response_json(response).await;
        assert!(
            body["generation"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert_eq!(body["items"].as_array().unwrap().len(), 1);
        assert_eq!(body["items"][0]["node_id"], "node-a");
        assert_eq!(body["items"][0]["title_path"], json!(["Alpha"]));
        assert!(body["items"][0].get("document_revision").is_none());
        assert!(body["items"][0].get("content_sha256").is_none());
        assert!(body["next"].is_null());

        let invalid = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/v1/nodes/catalog?limit=5001",
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_settings_mutations_are_rejected_before_persistence() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device("settings".into(), vec!["settings:write".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let app = router(service.clone());

        for (operation_id, timezone, retention) in [
            ("invalid-timezone", "Mars/Olympus", 0.9),
            ("invalid-retention", "UTC", 1.0),
        ] {
            let response = app
                .clone()
                .oneshot(authorized_mutation(
                    "PUT",
                    "/v1/settings",
                    &token,
                    operation_id,
                    Some("1"),
                    Body::from(
                        serde_json::to_vec(&json!({
                            "timezone": timezone,
                            "day_boundary_hour": 4,
                            "desired_retention": retention
                        }))
                        .unwrap(),
                    ),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        }

        assert_eq!(service.settings().await.unwrap().revision, 1);
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

        let mut request = authorized_with_type(
            "PUT",
            &path,
            &token,
            "image/png",
            Body::from(bytes.to_vec()),
        );
        request.headers_mut().insert(
            "Idempotency-Key",
            axum::http::HeaderValue::from_static("asset-create"),
        );
        let created = app.clone().oneshot(request).await.unwrap();
        assert_eq!(created.status(), StatusCode::OK);
        let created_body = response_json(created).await;
        let mut replay_request = authorized_with_type(
            "PUT",
            &path,
            &token,
            "image/png",
            Body::from(bytes.to_vec()),
        );
        replay_request.headers_mut().insert(
            "Idempotency-Key",
            axum::http::HeaderValue::from_static("asset-create"),
        );
        let replay = app.clone().oneshot(replay_request).await.unwrap();
        assert_eq!(replay.status(), StatusCode::OK);
        assert_eq!(response_json(replay).await, created_body);

        let mut duplicate_request = authorized_with_type(
            "PUT",
            &path,
            &token,
            "image/png",
            Body::from(bytes.to_vec()),
        );
        duplicate_request.headers_mut().insert(
            "Idempotency-Key",
            axum::http::HeaderValue::from_static("asset-create-duplicate"),
        );
        let duplicate = app.clone().oneshot(duplicate_request).await.unwrap();
        assert_eq!(duplicate.status(), StatusCode::OK);
        let duplicate_body = response_json(duplicate).await;
        assert_eq!(duplicate_body["status"], "rejected");
        assert_eq!(duplicate_body["rejection"]["reason"], "revision_conflict");
        assert!(duplicate_body["rejection"]["expected_revision"].is_null());
        assert_eq!(duplicate_body["rejection"]["current_revision"], 1);
        assert_eq!(duplicate_body["cursor"], created_body["cursor"]);

        let mut reused_request = authorized_with_type(
            "PUT",
            &format!("/v1/assets/{digest}/image-2.png"),
            &token,
            "image/png",
            Body::from(bytes.to_vec()),
        );
        reused_request.headers_mut().insert(
            "Idempotency-Key",
            axum::http::HeaderValue::from_static("asset-create"),
        );
        let reused = app.clone().oneshot(reused_request).await.unwrap();
        assert_eq!(reused.status(), StatusCode::CONFLICT);

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
    async fn asset_digest_mismatch_is_repeatable_and_writes_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let runtime = ServiceRuntime::start(database.clone(), directory.path().join("blobs"), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let device = generate_device("asset-digest".into(), vec!["content:write".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let app = router(service.clone());
        let digest = format!("{:x}", Sha256::digest(b"expected"));
        let path = format!("/v1/assets/{digest}/image.png");
        let request = || {
            let mut request = authorized_with_type(
                "PUT",
                &path,
                &token,
                "image/png",
                Body::from(b"different".to_vec()),
            );
            request.headers_mut().insert(
                "Idempotency-Key",
                axum::http::HeaderValue::from_static("asset-digest-mismatch"),
            );
            request
        };

        let first = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(first.status(), StatusCode::BAD_REQUEST);
        let first = response_json(first).await;
        assert_eq!(first["error"]["code"], "invalid_request");
        assert_eq!(
            first["error"]["message"],
            "asset digest does not match content"
        );
        let replay = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(replay.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response_json(replay).await, first);
        assert_eq!(service.latest_content_cursor().await.unwrap(), 0);
        drop(app);
        drop(service);
        runtime.finish().await.unwrap();

        let connection = rusqlite::Connection::open(database).unwrap();
        for table in [
            "blob",
            "managed_asset",
            "content_change",
            "device_mutation_receipt",
        ] {
            let count: i64 = connection
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "unexpected {table} row");
        }
    }

    #[tokio::test]
    async fn asset_key_reuse_with_different_bytes_conflicts_after_restart() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let blobs = directory.path().join("blobs");
        let device = generate_device("asset-replay".into(), vec!["content:write".into()]).unwrap();
        let token = device.token.clone();
        let accepted = b"accepted";
        let digest = format!("{:x}", Sha256::digest(accepted));
        let path = format!("/v1/assets/{digest}/image.png");

        let runtime = ServiceRuntime::start(database.clone(), blobs.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        service.create_device(device, 1).await.unwrap();
        let mut request = authorized_with_type(
            "PUT",
            &path,
            &token,
            "image/png",
            Body::from(accepted.to_vec()),
        );
        request.headers_mut().insert(
            "Idempotency-Key",
            axum::http::HeaderValue::from_static("asset-replay"),
        );
        let response = router(service.clone()).oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        drop(service);
        runtime.finish().await.unwrap();

        let runtime = ServiceRuntime::start(database, blobs, 8).await.unwrap();
        let service = runtime.handle();
        let mut request = authorized_with_type(
            "PUT",
            &path,
            &token,
            "image/png",
            Body::from(b"rejected".to_vec()),
        );
        request.headers_mut().insert(
            "Idempotency-Key",
            axum::http::HeaderValue::from_static("asset-replay"),
        );
        let response = router(service.clone()).oneshot(request).await.unwrap();

        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "idempotency_conflict"
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn durable_thema_create_conflict_is_retained_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let blobs = directory.path().join("blobs");
        let operation_id = "thema-create-conflict";
        let device = generate_device(
            "thema-create".into(),
            vec!["content:write".into(), "read".into()],
        )
        .unwrap();
        let token = device.token.clone();
        let thema_id = durable_created_thema_id(&device.device_id, operation_id);
        let input = ThemaWriteRequestV1 {
            kind: "basic".into(),
            keimenon: "Question".into(),
            hypothesis: vec![],
            answer: vec!["Answer".into()],
            source_guid: None,
            parathema: None,
            review_image: None,
            tags: vec![],
            concepts: vec![],
            asset_paths: vec![],
        };
        let runtime = ServiceRuntime::start(database.clone(), blobs.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        service.create_device(device, 1).await.unwrap();
        service
            .put_thema(
                prepare_thema(
                    Some(thema_id.clone()),
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
                    vec![],
                    vec![],
                    vec![],
                    None,
                )
                .unwrap(),
                20260719,
                2,
                None,
            )
            .await
            .unwrap();
        let app = router(service.clone());
        let expected_request_sha256 = crate::application::desired_state_wire_request_sha256(
            1,
            &DeviceDesiredStateMutation::PutThema {
                prepared: prepared_thema(Some(thema_id.clone()), input.clone(), None).unwrap(),
                initial_review_day: 0,
            },
        )
        .unwrap();
        let request = || {
            authorized_mutation(
                "POST",
                "/v1/themata",
                &token,
                operation_id,
                None,
                Body::from(serde_json::to_vec(&input).unwrap()),
            )
        };

        let first = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let first = response_json(first).await;
        assert_eq!(first["status"], "rejected");
        assert_eq!(first["request_sha256"], expected_request_sha256);
        assert_eq!(first["rejection"]["reason"], "revision_conflict");
        assert!(first["rejection"]["expected_revision"].is_null());
        assert_eq!(first["rejection"]["current_revision"], 1);
        let fetched = authenticated_json(&app, &token, &format!("/v1/themata/{thema_id}")).await;
        assert_eq!(fetched["revision"], 1);
        assert_eq!(fetched["keimenon"], "Question");
        drop(app);
        drop(service);
        runtime.finish().await.unwrap();

        let runtime = ServiceRuntime::start(database, blobs, 8).await.unwrap();
        let service = runtime.handle();
        let app = router(service.clone());
        let replay = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(replay.status(), StatusCode::OK);
        assert_eq!(response_json(replay).await, first);
        let fetched = authenticated_json(&app, &token, &format!("/v1/themata/{thema_id}")).await;
        assert_eq!(fetched["revision"], 1);
        assert_eq!(fetched["keimenon"], "Question");
        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_thema_archive_id_is_rejected_before_receipt_persistence() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device("thema".into(), vec!["content:write".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let app = router(service.clone());

        for action in ["archive", "unarchive"] {
            let operation_id = format!("invalid-{action}");
            let invalid_path = format!("/v1/themata/{}/{action}", "x".repeat(257));
            let invalid = app
                .clone()
                .oneshot(authorized_mutation(
                    "POST",
                    &invalid_path,
                    &token,
                    &operation_id,
                    Some("1"),
                    Body::empty(),
                ))
                .await
                .unwrap();
            assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);

            let retained_not_found = app
                .clone()
                .oneshot(authorized_mutation(
                    "POST",
                    &format!("/v1/themata/missing/{action}"),
                    &token,
                    &operation_id,
                    Some("1"),
                    Body::empty(),
                ))
                .await
                .unwrap();
            assert_eq!(retained_not_found.status(), StatusCode::OK);
            assert_eq!(
                response_json(retained_not_found).await["rejection"]["reason"],
                "not_found"
            );
        }

        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn direct_suspension_is_revisioned_idempotent_and_separate_from_content() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let manager = generate_device(
            "manager".into(),
            vec!["read".into(), "review".into(), "content:write".into()],
        )
        .unwrap();
        let manager_token = manager.token.clone();
        service.create_device(manager, 1).await.unwrap();
        let content_only = generate_device("content".into(), vec!["content:write".into()]).unwrap();
        let content_token = content_only.token.clone();
        service.create_device(content_only, 2).await.unwrap();
        put_review_thema(&service, "Question", None, 3).await;
        put_custom_review_thema(
            &service,
            "z-thema",
            "basic",
            "Second",
            vec![None],
            vec!["answer".into()],
            None,
        )
        .await;
        let app = router(service.clone());

        let initial = authenticated_json(&app, &manager_token, "/v1/themata/catalog?limit=1").await;
        assert_eq!(initial["items"][0]["state_revision"], 0);
        assert_eq!(initial["items"][0]["suspended"], false);
        assert!(
            initial["generation"]
                .as_str()
                .is_some_and(|value| !value.is_empty())
        );
        assert_eq!(initial["next"], "thema");
        let remainder = authenticated_json(
            &app,
            &manager_token,
            "/v1/themata/catalog?limit=1&after=thema",
        )
        .await;
        assert_eq!(remainder["generation"], initial["generation"]);
        assert_eq!(remainder["items"][0]["thema_id"], "z-thema");
        assert!(remainder["next"].is_null());

        let forbidden = app
            .clone()
            .oneshot(authorized_mutation(
                "PUT",
                "/v1/themata/thema/suspension",
                &content_token,
                "forbidden-suspend",
                Some("0"),
                Body::from(r#"{"suspended":true}"#),
            ))
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let suspend_request = || {
            authorized_mutation(
                "PUT",
                "/v1/themata/thema/suspension",
                &manager_token,
                "suspend-thema",
                Some("0"),
                Body::from(r#"{"suspended":true}"#),
            )
        };
        let suspended = app.clone().oneshot(suspend_request()).await.unwrap();
        assert_eq!(suspended.status(), StatusCode::OK);
        let suspended = response_json(suspended).await;
        assert_eq!(suspended["status"], "applied");
        assert_eq!(suspended["changed"], true);
        assert_eq!(suspended["authoritative"]["kind"], "review_state");
        assert_eq!(
            suspended["authoritative"]["value"]["state"]["state_revision"],
            1
        );
        assert_eq!(
            suspended["authoritative"]["value"]["state"]["suspended"],
            true
        );
        let replay = app.clone().oneshot(suspend_request()).await.unwrap();
        assert_eq!(response_json(replay).await, suspended);

        let reused = app
            .clone()
            .oneshot(authorized_mutation(
                "PUT",
                "/v1/themata/thema/suspension",
                &manager_token,
                "suspend-thema",
                Some("0"),
                Body::from(r#"{"suspended":false}"#),
            ))
            .await
            .unwrap();
        assert_eq!(reused.status(), StatusCode::CONFLICT);
        assert_eq!(
            response_json(reused).await["error"]["code"],
            "idempotency_conflict"
        );

        let stale = app
            .clone()
            .oneshot(authorized_mutation(
                "PUT",
                "/v1/themata/thema/suspension",
                &manager_token,
                "stale-unsuspend",
                Some("0"),
                Body::from(r#"{"suspended":false}"#),
            ))
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::OK);
        let stale = response_json(stale).await;
        assert_eq!(stale["status"], "rejected");
        assert_eq!(stale["rejection"]["reason"], "revision_conflict");
        assert_eq!(stale["rejection"]["expected_revision"], 0);
        assert_eq!(stale["rejection"]["current_revision"], 1);

        service
            .archive_thema("thema".into(), 1, true, 4, None)
            .await
            .unwrap();
        let unsuspended = app
            .clone()
            .oneshot(authorized_mutation(
                "PUT",
                "/v1/themata/thema/suspension",
                &manager_token,
                "unsuspend-thema",
                Some("1"),
                Body::from(r#"{"suspended":false}"#),
            ))
            .await
            .unwrap();
        assert_eq!(unsuspended.status(), StatusCode::OK);
        assert_eq!(
            response_json(unsuspended).await["authoritative"]["value"]["state"]["state_revision"],
            2
        );
        put_review_thema(&service, "Edited", Some(2), 5).await;

        let catalog = authenticated_json(&app, &manager_token, "/v1/themata/catalog").await;
        assert_eq!(catalog["items"][0]["thema_revision"], 3);
        assert_eq!(catalog["items"][0]["state_revision"], 2);
        assert_eq!(catalog["items"][0]["keimenon"], "Edited");
        assert_eq!(catalog["items"][0]["suspended"], false);
        let changes = authenticated_json(&app, &manager_token, "/v1/changes?limit=100").await;
        assert!(changes["items"].as_array().unwrap().iter().any(|change| {
            change["entity_kind"] == "review" && change["operation"] == "suspension"
        }));

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
        let cross_device_state = app
            .clone()
            .oneshot(authorized(
                "GET",
                &format!("/v1/review/attempts/{attempt_id}"),
                &other_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(cross_device_state.status(), StatusCode::OK);
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
        assert_eq!(reused.status(), StatusCode::PRECONDITION_FAILED);

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

    #[tokio::test]
    async fn online_review_recovery_errors_match_the_openapi_contract() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device("reviewer".into(), vec!["review".into()]).unwrap();
        let token = device.token.clone();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let app = router(service.clone());
        let empty = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/sessions",
                &token,
                Body::from(review_session_body("missing")),
            ))
            .await
            .unwrap();
        assert_eq!(empty.status(), StatusCode::NOT_FOUND);

        put_review_thema(&service, "Question", None, 2).await;
        let (session_id, pending_id) = pending_online_attempt(&service, &device_id, 3, 4).await;
        put_review_thema(&service, "Changed", Some(1), 5).await;
        let stale_answer = app
            .clone()
            .oneshot(authorized(
                "POST",
                &format!("/v1/review/attempts/{pending_id}/answer"),
                &token,
                Body::from(r#"{"kind":"text","value":"answer"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(stale_answer.status(), StatusCode::PRECONDITION_FAILED);
        let stale_toggle = app
            .clone()
            .oneshot(authorized(
                "POST",
                &format!("/v1/review/attempts/{pending_id}/suspension"),
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(stale_toggle.status(), StatusCode::METHOD_NOT_ALLOWED);
        service
            .stop_review_session(session_id, Some(device_id.clone()), true, 6)
            .await
            .unwrap();

        let (_, graded_id) = pending_online_attempt(&service, &device_id, 7, 8).await;
        service
            .grade_review_attempt(
                graded_id.clone(),
                Some(device_id.clone()),
                AnswerInput::Text("answer".into()),
                9,
            )
            .await
            .unwrap();
        put_review_thema(&service, "Changed again", Some(2), 10).await;
        let stale_override = app
            .oneshot(authorized(
                "POST",
                &format!("/v1/review/attempts/{graded_id}/override"),
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(stale_override.status(), StatusCode::NOT_FOUND);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn client_read_views_and_online_sessions_work_across_devices() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let first = generate_device("first".into(), vec!["read".into(), "review".into()]).unwrap();
        let first_token = first.token.clone();
        let first_id = first.device_id.clone();
        service.create_device(first, 1).await.unwrap();
        let second =
            generate_device("second".into(), vec!["read".into(), "review".into()]).unwrap();
        let second_token = second.token.clone();
        service.create_device(second, 2).await.unwrap();
        put_review_thema(&service, "Question", None, 3).await;
        let app = router(service.clone());

        let queue = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/queue/summary",
                &second_token,
                Body::from(review_queue_body("thema")),
            ))
            .await
            .unwrap();
        assert_eq!(queue.status(), StatusCode::OK);
        assert_eq!(response_json(queue).await["matched_count"], 1);

        let session = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/sessions",
                &first_token,
                Body::from(review_session_body("thema")),
            ))
            .await
            .unwrap();
        let session = response_json(session).await;
        let session_id = session["session_id"].as_str().unwrap();
        assert_eq!(session["owner_device_id"], first_id);

        let active = app
            .clone()
            .oneshot(authorized(
                "GET",
                "/v1/review/sessions/active",
                &second_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(
            response_json(active).await["session"]["session_id"],
            session_id
        );
        for action in ["quit", "resume"] {
            let response = app
                .clone()
                .oneshot(authorized(
                    "POST",
                    &format!("/v1/review/sessions/{session_id}/{action}"),
                    &second_token,
                    Body::empty(),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        let attempt = app
            .clone()
            .oneshot(authorized(
                "POST",
                &format!("/v1/review/sessions/{session_id}/attempts"),
                &second_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        let attempt = response_json(attempt).await;
        let attempt_id = attempt["attempt"]["attempt_id"].as_str().unwrap();
        let answer = app
            .clone()
            .oneshot(authorized(
                "POST",
                &format!("/v1/review/attempts/{attempt_id}/answer"),
                &second_token,
                Body::from(r#"{"kind":"text","value":"answer"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(answer.status(), StatusCode::OK);
        let commit = app
            .clone()
            .oneshot(authorized(
                "POST",
                &format!("/v1/review/attempts/{attempt_id}/commit"),
                &second_token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(commit.status(), StatusCode::OK);

        assert_client_read_views(&app, &second_token, session_id).await;
        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    async fn assert_client_read_views(app: &axum::Router, token: &str, session_id: &str) {
        let dashboard = authenticated_json(app, token, "/v1/dashboard?days=30").await;
        assert_eq!(dashboard["today"]["review_count"], 1);
        assert_eq!(dashboard["active_session"], Value::Null);
        let sessions = authenticated_json(
            app,
            token,
            "/v1/review/sessions?status=completed&mode=online",
        )
        .await;
        assert_eq!(sessions["next"], session_id);
        let history =
            authenticated_json(app, token, "/v1/review/history?accepted_success=true").await;
        assert_eq!(history["items"].as_array().unwrap().len(), 1);
        assert_eq!(history["items"][0]["session_id"], session_id);
        let state = authenticated_json(app, token, "/v1/themata/thema/review-state").await;
        assert_eq!(state["state"]["review_count"], 1);
        let detail =
            authenticated_json(app, token, &format!("/v1/review/sessions/{session_id}")).await;
        assert_eq!(detail["review_count"], 1);
    }

    async fn authenticated_json(app: &axum::Router, token: &str, uri: &str) -> Value {
        let response = app
            .clone()
            .oneshot(authorized("GET", uri, token, Body::empty()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        response_json(response).await
    }

    fn review_queue_body(thema_id: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "source": {
                "kind": "manual",
                "tags": [],
                "exclude_tags": [],
                "topic_node_id": null,
                "topic_forward_depth": null,
                "topic_backward_depth": null,
                "manual_ids": [thema_id]
            }
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn live_cloze_checks_are_partial_order_independent_and_non_mutating() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device("reviewer".into(), vec!["review".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        put_custom_review_thema(
            &service,
            "multi-cloze",
            "cloze",
            "Literal (...) then cat and cut and dog",
            vec![None, None, None],
            vec!["cat".into(), "cut".into(), "dog".into()],
            None,
        )
        .await;
        let app = router(service.clone());
        let (_, attempts_uri) = start_http_review(&app, &token, "multi-cloze").await;
        let (_, result) = begin_http_review_attempt(&app, &token, &attempts_uri).await;
        let attempt = &result["attempt"];
        let attempt_id = attempt["attempt_id"].as_str().unwrap();

        let empty = app
            .clone()
            .oneshot(authorized(
                "POST",
                &format!("/v1/review/attempts/{attempt_id}/cloze-check"),
                &token,
                Body::from(r#"{"inputs":[]}"#),
            ))
            .await
            .unwrap();
        assert_eq!(empty.status(), StatusCode::BAD_REQUEST);
        let partial = check_cloze_inputs(&app, &token, attempt_id, &["cut"]).await;
        assert_eq!(partial["matches"], json!([true]));
        assert_eq!(partial["all_entered_correct"], true);
        assert_eq!(partial["complete"], false);
        assert_eq!(
            partial["question"],
            "Literal (...) then cat and (...) and (...)"
        );
        assert_eq!(partial["next_input_script"], Value::Null);
        let wrong_first = check_cloze_inputs(&app, &token, attempt_id, &["wrong", "cut"]).await;
        assert_eq!(wrong_first["matches"], json!([false, false]));
        assert_eq!(
            wrong_first["question"],
            "Literal (...) then (...) and (...) and (...)"
        );
        let wrong_middle =
            check_cloze_inputs(&app, &token, attempt_id, &["cat", "wrong", "dog"]).await;
        assert_eq!(wrong_middle["matches"], json!([true, false, false]));
        assert_eq!(
            wrong_middle["question"],
            "Literal (...) then cat and (...) and (...)"
        );
        let complete = check_cloze_inputs(&app, &token, attempt_id, &["cot", "bat", "dog"]).await;
        assert_eq!(complete["matches"], json!([true, true, true]));
        assert_eq!(complete["complete"], true);
        assert_eq!(
            complete["question"],
            "Literal (...) then cat and cut and dog"
        );
        let state =
            authenticated_json(&app, &token, &format!("/v1/review/attempts/{attempt_id}")).await;
        assert_eq!(state["attempt"]["status"], "pending");
        assert_eq!(state["attempt"]["response"], Value::Null);
        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    async fn check_cloze_inputs(
        app: &axum::Router,
        token: &str,
        attempt_id: &str,
        inputs: &[&str],
    ) -> Value {
        let response = app
            .clone()
            .oneshot(authorized(
                "POST",
                &format!("/v1/review/attempts/{attempt_id}/cloze-check"),
                token,
                Body::from(serde_json::to_vec(&json!({"inputs": inputs})).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response_json(response).await
    }

    async fn online_attempt(service: &crate::service::ServiceHandle) -> (String, String, String) {
        let device = generate_device("reviewer".into(), vec!["review".into()]).unwrap();
        let token = device.token.clone();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        put_review_thema(service, "Question", None, 1).await;
        let (_, attempt_id) = pending_online_attempt(service, &device_id, 2, 3).await;
        service
            .grade_review_attempt(
                attempt_id.clone(),
                Some(device_id.clone()),
                AnswerInput::Text("answer".into()),
                4,
            )
            .await
            .unwrap();
        (token, device_id, attempt_id)
    }

    async fn pending_online_attempt(
        service: &crate::service::ServiceHandle,
        device_id: &str,
        session_time: u64,
        attempt_time: u64,
    ) -> (String, String) {
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
                Some(device_id.to_owned()),
                session_time,
            )
            .await
            .unwrap();
        let session_id = session.session_id;
        let attempt = service
            .begin_http_review_attempt(session_id.clone(), Some(device_id.to_owned()), attempt_time)
            .await
            .unwrap()
            .attempt
            .unwrap();
        (session_id, attempt.attempt_id)
    }

    async fn put_review_thema(
        service: &crate::service::ServiceHandle,
        question: &str,
        revision: Option<u64>,
        updated_at: i64,
    ) {
        service
            .put_thema(
                prepare_thema(
                    Some("thema".into()),
                    thema_content(
                        "basic",
                        question.into(),
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
                    revision,
                )
                .unwrap(),
                20260716,
                updated_at,
                None,
            )
            .await
            .unwrap();
    }

    async fn put_custom_review_thema(
        service: &crate::service::ServiceHandle,
        id: &str,
        kind: &str,
        question: &str,
        hypothesis: Vec<Option<String>>,
        answer: Vec<String>,
        revision: Option<u64>,
    ) {
        let content =
            thema_content(kind, question.into(), hypothesis, answer, None, None, None).unwrap();
        let updated_at = revision.map_or(1, |value| i64::try_from(value + 1).unwrap());
        service
            .put_thema(
                prepare_thema(Some(id.into()), content, vec![], vec![], vec![], revision).unwrap(),
                20260716,
                updated_at,
                None,
            )
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn online_review_sessions_conceal_due_content_and_enforce_target_bounds() {
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
            generate_device("reviewer".into(), vec!["read".into(), "review".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        put_review_thema(&service, "Question", None, 2).await;
        let app = router(service.clone());

        let due = authenticated_json(&app, &token, "/v1/review/due?day=20260716").await;
        assert_eq!(
            due["items"][0]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>(),
            ["id", "next_review_day", "review_count"]
                .into_iter()
                .map(str::to_owned)
                .collect()
        );

        for index in 0..500 {
            put_custom_review_thema(
                &service,
                &format!("thema-{index:03}"),
                "basic",
                "Question",
                vec![None],
                vec!["answer".into()],
                None,
            )
            .await;
        }

        for (target_count, expected) in [(None, 100), (Some(500), 500)] {
            let response = app
                .clone()
                .oneshot(authorized(
                    "POST",
                    "/v1/review/sessions",
                    &token,
                    Body::from(review_queue_session_body(500, target_count)),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::CREATED);
            let session = response_json(response).await;
            assert_eq!(session["target_count"], expected);
            assert_eq!(session["queue_length"], expected);
            abort_http_review(&app, &token, session["session_id"].as_str().unwrap()).await;
        }

        for target_count in [0, 501] {
            let rejected = app
                .clone()
                .oneshot(authorized(
                    "POST",
                    "/v1/review/sessions",
                    &token,
                    Body::from(review_queue_session_body(500, Some(target_count))),
                ))
                .await
                .unwrap();
            assert_eq!(rejected.status(), StatusCode::BAD_REQUEST);
            let active = authenticated_json(&app, &token, "/v1/review/sessions/active").await;
            assert_eq!(active["session"], Value::Null);
        }

        let oversized_source = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/sessions",
                &token,
                Body::from(review_queue_session_body(501, Some(1))),
            ))
            .await
            .unwrap();
        assert_eq!(oversized_source.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            authenticated_json(&app, &token, "/v1/review/sessions/active").await["session"],
            Value::Null
        );

        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    async fn assert_http_attempt_presentation(
        app: &axum::Router,
        token: &str,
        thema_id: &str,
        question: &str,
        hint: Option<&str>,
        response_prompt: serde_json::Value,
    ) {
        let (session_id, uri) = start_http_review(app, token, thema_id).await;
        let (status, first) = begin_http_review_attempt(app, token, &uri).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(first["attempt"]["presentation"]["question"], question);
        assert_eq!(first["attempt"]["presentation"]["hint"], json!(hint));
        assert_eq!(
            first["attempt"]["presentation"]["response"],
            response_prompt
        );
        assert_eq!(first["start_position"], 0);
        assert_eq!(first["skipped"], json!([]));
        let (_, retry) = begin_http_review_attempt(app, token, &uri).await;
        assert_eq!(retry, first);
        abort_http_review(app, token, &session_id).await;
    }

    async fn start_http_review(
        app: &axum::Router,
        token: &str,
        thema_id: &str,
    ) -> (String, String) {
        let session = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/review/sessions",
                token,
                Body::from(review_session_body(thema_id)),
            ))
            .await
            .unwrap();
        let session = response_json(session).await;
        let session_id = session["session_id"].as_str().unwrap().to_owned();
        let uri = format!("/v1/review/sessions/{session_id}/attempts");
        (session_id, uri)
    }

    async fn begin_http_review_attempt(
        app: &axum::Router,
        token: &str,
        uri: &str,
    ) -> (StatusCode, serde_json::Value) {
        let response = app
            .clone()
            .oneshot(authorized("POST", uri, token, Body::empty()))
            .await
            .unwrap();
        let status = response.status();
        (status, response_json(response).await)
    }

    async fn abort_http_review(app: &axum::Router, token: &str, session_id: &str) {
        let response = app
            .clone()
            .oneshot(authorized(
                "POST",
                &format!("/v1/review/sessions/{session_id}/abort"),
                token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    fn review_session_body(thema_id: &str) -> Vec<u8> {
        review_session_body_with_target(thema_id, None)
    }

    fn review_session_body_with_target(thema_id: &str, target_count: Option<u32>) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "source": {
                "kind": "manual",
                "tags": [],
                "exclude_tags": [],
                "topic_node_id": null,
                "topic_forward_depth": null,
                "topic_backward_depth": null,
                "manual_ids": [thema_id]
            },
            "target_count": target_count
        }))
        .unwrap()
    }

    fn review_queue_session_body(item_count: usize, target_count: Option<u32>) -> Vec<u8> {
        let manual_ids = (0..item_count)
            .map(|index| format!("thema-{index:03}"))
            .collect::<Vec<_>>();
        serde_json::to_vec(&json!({
            "source": {
                "kind": "manual",
                "tags": [],
                "exclude_tags": [],
                "topic_node_id": null,
                "topic_forward_depth": null,
                "topic_backward_depth": null,
                "manual_ids": manual_ids
            },
            "target_count": target_count
        }))
        .unwrap()
    }

    fn assert_response_statuses(operation: &serde_json::Value, expected: &[&str]) {
        let mut actual = operation["responses"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>();
        actual.sort_unstable();
        assert_eq!(actual, expected);
    }

    #[tokio::test]
    async fn missing_note_assets_are_invalid_content() {
        let directory = tempfile::tempdir().unwrap();
        let blob_root = directory.path().join("blobs");
        let runtime =
            ServiceRuntime::start(directory.path().join("gnosis.sqlite"), blob_root.clone(), 8)
                .await
                .unwrap();
        let service = runtime.handle();
        let device = generate_device("writer".into(), vec!["content:write".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let asset = format!("assets/{}/missing.png", "a".repeat(64));
        let org = format!("#+title: Missing\n[[file:{asset}]]\n");
        let digest = format!("{:x}", Sha256::digest(org.as_bytes()));
        let body = serde_json::to_vec(&json!({
            "logical_path": "note.org",
            "content_type": "text/org; charset=utf-8",
            "source": org
        }))
        .unwrap();

        let response = router(service.clone())
            .oneshot(authorized_mutation(
                "POST",
                "/v1/document-mutations",
                &token,
                "missing-asset",
                None,
                Body::from(body),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response_json(response).await["rejection"]["reason"],
            "invalid_content"
        );
        assert_eq!(service.latest_content_cursor().await.unwrap(), 0);
        assert!(!blob_root.join(&digest[..2]).join(digest).exists());
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn direct_document_http_mutations_are_durable_and_replayable() {
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
            generate_device("writer".into(), vec!["read".into(), "content:write".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let app = router(service.clone());
        let body = note_body("note.org", "First");

        let missing = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/document-mutations",
                &token,
                Body::from(body.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);

        let create = app
            .clone()
            .oneshot(authorized_mutation(
                "POST",
                "/v1/document-mutations",
                &token,
                "create",
                None,
                Body::from(body.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::OK);
        let created = response_json(create).await;
        assert_eq!(created["status"], "applied");
        assert_eq!(created["operation_id"], "create");
        assert_eq!(created["authoritative"]["revision"], 1);
        let document_id = created["authoritative"]["document_id"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(
            decode_cursor(created["cursor"].as_str().unwrap())
                .unwrap()
                .1,
            1
        );

        let retry = app
            .clone()
            .oneshot(authorized_mutation(
                "POST",
                "/v1/document-mutations",
                &token,
                "create",
                None,
                Body::from(body),
            ))
            .await
            .unwrap();
        assert_eq!(retry.status(), StatusCode::OK);
        assert_eq!(response_json(retry).await, created);

        let changed = app
            .clone()
            .oneshot(authorized_mutation(
                "POST",
                "/v1/document-mutations",
                &token,
                "create",
                None,
                Body::from(note_body("note.org", "Changed")),
            ))
            .await
            .unwrap();
        assert_eq!(changed.status(), StatusCode::CONFLICT);
        assert_eq!(
            response_json(changed).await["error"]["code"],
            "idempotency_conflict"
        );

        let stale_path = format!("/v1/document-mutations/{document_id}");
        let stale = app
            .clone()
            .oneshot(authorized_mutation(
                "PUT",
                &stale_path,
                &token,
                "stale",
                Some("\"0\""),
                Body::from(note_body("note.org", "Stale")),
            ))
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::OK);
        let stale = response_json(stale).await;
        assert_eq!(stale["status"], "rejected");
        assert_eq!(stale["rejection"]["reason"], "revision_conflict");
        assert_eq!(stale["rejection"]["current_revision"], 1);

        let update = app
            .clone()
            .oneshot(authorized_mutation(
                "PUT",
                &stale_path,
                &token,
                "update",
                Some("1"),
                Body::from(note_body("note.org", "Current")),
            ))
            .await
            .unwrap();
        assert_eq!(response_json(update).await["authoritative"]["revision"], 2);
        let stale_retry = app
            .oneshot(authorized_mutation(
                "PUT",
                &stale_path,
                &token,
                "stale",
                Some("0"),
                Body::from(note_body("note.org", "Stale")),
            ))
            .await
            .unwrap();
        assert_eq!(response_json(stale_retry).await, stale);
        assert_eq!(
            service
                .note_revisions(DocumentId::new(document_id).unwrap(), 0, 10)
                .await
                .unwrap()
                .len(),
            2
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn direct_document_lifecycle_mutations_are_durable_and_replayable() {
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
            generate_device("writer".into(), vec!["read".into(), "content:write".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let app = router(service.clone());
        let create = app
            .clone()
            .oneshot(authorized_mutation(
                "POST",
                "/v1/document-mutations",
                &token,
                "create",
                None,
                Body::from(note_body("note.org", "Note")),
            ))
            .await
            .unwrap();
        let created = response_json(create).await;
        let document_id = created["authoritative"]["document_id"].as_str().unwrap();
        let rename_path = format!("/v1/document-mutations/{document_id}/rename");
        let rename_body = serde_json::to_vec(&json!({"logical_path":"renamed.org"})).unwrap();

        let renamed = app
            .clone()
            .oneshot(authorized_mutation(
                "POST",
                &rename_path,
                &token,
                "rename",
                Some("1"),
                Body::from(rename_body.clone()),
            ))
            .await
            .unwrap();
        assert_eq!(renamed.status(), StatusCode::OK);
        let renamed = response_json(renamed).await;
        assert_eq!(renamed["authoritative"]["logical_path"], "renamed.org");
        assert_eq!(renamed["authoritative"]["revision"], 2);
        let rename_retry = app
            .clone()
            .oneshot(authorized_mutation(
                "POST",
                &rename_path,
                &token,
                "rename",
                Some("1"),
                Body::from(rename_body),
            ))
            .await
            .unwrap();
        assert_eq!(response_json(rename_retry).await, renamed);

        let mutation_path = format!("/v1/document-mutations/{document_id}");
        let stale = app
            .clone()
            .oneshot(authorized_mutation(
                "DELETE",
                &mutation_path,
                &token,
                "stale-delete",
                Some("1"),
                Body::empty(),
            ))
            .await
            .unwrap();
        let stale = response_json(stale).await;
        assert_eq!(stale["status"], "rejected");
        assert_eq!(stale["rejection"]["reason"], "revision_conflict");
        assert_eq!(stale["rejection"]["current_revision"], 2);

        let deleted = app
            .clone()
            .oneshot(authorized_mutation(
                "DELETE",
                &mutation_path,
                &token,
                "delete",
                Some("2"),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(deleted.status(), StatusCode::OK);
        let deleted = response_json(deleted).await;
        assert_eq!(deleted["authoritative"]["revision"], 3);
        assert_eq!(deleted["authoritative"]["active"], false);
        let delete_retry = app
            .oneshot(authorized_mutation(
                "DELETE",
                &mutation_path,
                &token,
                "delete",
                Some("2"),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response_json(delete_retry).await, deleted);
        assert_eq!(
            service
                .note_revisions(DocumentId::new(document_id).unwrap(), 0, 10)
                .await
                .unwrap()
                .len(),
            3
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn legacy_note_write_routes_are_removed() {
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
        let app = router(service.clone());

        let create = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/notes",
                &token,
                Body::from(note_body("note.org", "Original")),
            ))
            .await
            .unwrap();
        assert_eq!(create.status(), StatusCode::NOT_FOUND);
        let path = "/v1/notes/document";

        let update = app
            .clone()
            .oneshot(authorized_if_match(
                "PUT",
                path,
                &token,
                "1",
                Body::from(note_body("note.org", "Updated")),
            ))
            .await
            .unwrap();
        assert_eq!(update.status(), StatusCode::METHOD_NOT_ALLOWED);

        let stale = app
            .oneshot(authorized_if_match(
                "PUT",
                path,
                &token,
                "1",
                Body::from(note_body("note.org", "Stale")),
            ))
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::METHOD_NOT_ALLOWED);
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
        let malformed = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/content/offline-operations",
                &token,
                Body::from(r#"{"version":1,"operation":{}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            response_json(malformed).await["error"]["code"],
            "invalid_body"
        );
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

    async fn assert_asset_version_rejected(app: &axum::Router, token: &str, body: &str) {
        let mut unsupported: Value = serde_json::from_str(body).unwrap();
        unsupported["version"] = json!(2);
        let response = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/content/offline-asset-operations",
                token,
                Body::from(serde_json::to_vec(&unsupported).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_json(response).await["error"]["code"],
            "unsupported_offline_asset_version"
        );
    }

    async fn assert_asset_path_rejected_without_sequence(
        app: &axum::Router,
        service: &crate::service::ServiceHandle,
        token: &str,
        device_id: &str,
        body: &str,
    ) {
        let before = service.replica_state(device_id.into()).await.unwrap();
        let mut oversized: Value = serde_json::from_str(body).unwrap();
        let name = format!("{}.png", "x".repeat(252));
        let digest = oversized["operation"]["content_sha256"]
            .as_str()
            .unwrap()
            .to_owned();
        oversized["operation"]["original_name"] = json!(name);
        oversized["operation"]["logical_path"] = json!(format!("assets/{digest}/{name}"));
        let response = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/content/offline-asset-operations",
                token,
                Body::from(serde_json::to_vec(&oversized).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let after = service.replica_state(device_id.into()).await.unwrap();
        assert_eq!(after.next_device_sequence, before.next_device_sequence);
    }

    async fn assert_blob_size_boundaries(app: &axum::Router, token: &str) {
        let empty = Vec::new();
        let empty_digest = format!("{:x}", Sha256::digest(&empty));
        let empty = authorized_with_type(
            "PUT",
            &format!("/v1/sync/blobs/{empty_digest}"),
            token,
            "application/octet-stream",
            Body::from(empty),
        );
        assert_eq!(
            app.clone().oneshot(empty).await.unwrap().status(),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        let maximum = vec![7; crate::application::MAX_OFFLINE_ASSET_BYTES as usize];
        let digest = format!("{:x}", Sha256::digest(&maximum));
        let maximum = authorized_with_type(
            "PUT",
            &format!("/v1/sync/blobs/{digest}"),
            token,
            "application/octet-stream",
            Body::from(maximum),
        );
        assert_eq!(
            app.clone().oneshot(maximum).await.unwrap().status(),
            StatusCode::CREATED
        );
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
            device_id: device_id.clone(),
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

        assert_asset_version_rejected(&app, &token, &body).await;
        assert_asset_path_rejected_without_sequence(&app, &service, &token, &device_id, &body)
            .await;
        assert_blob_size_boundaries(&app, &token).await;

        let malformed = app
            .clone()
            .oneshot(authorized(
                "POST",
                "/v1/content/offline-asset-operations",
                &token,
                Body::from(r#"{"version":1,"operation":{}}"#),
            ))
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            response_json(malformed).await["error"]["code"],
            "invalid_offline_asset_operation"
        );

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
            "content_type": "text/org; charset=utf-8",
            "source": "#+title: Note\n\n* Heading\n:PROPERTIES:\n:ID: node\n:END:\nBody\n"
        });
        let created = app
            .clone()
            .oneshot(authorized_mutation(
                "POST",
                "/v1/document-mutations",
                &owner_token,
                "create-note",
                None,
                Body::from(serde_json::to_vec(&note).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::OK);

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
        assert_eq!(
            capabilities.supported_content_types,
            ["text/org; charset=utf-8"]
        );
        assert_eq!(capabilities.max_source_bytes, 1024 * 1024);
        assert_eq!(capabilities.max_projection_bytes, 1024 * 1024);

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
                    desired_retention: settings.scheduler.desired_retention,
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
        assert_eq!(snapshot_settings["timezone"], "Europe/Athens");

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
            .oneshot(authorized_mutation(
                "POST",
                "/v1/notes",
                &owner_token,
                "invalid-note",
                None,
                Body::from(serde_json::to_vec(&invalid).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(service.latest_content_cursor().await.unwrap(), before);

        drop(app);
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
        let image = Vec::new();
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
        assert_eq!(created["pack"]["assets"][0]["size_bytes"], 0);
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

    #[tokio::test]
    async fn http_begin_attempt_materializes_safe_presentations_and_recovers_safely() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device("reviewer".into(), vec!["review".into()]).unwrap();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        let app = router(service.clone());

        put_custom_review_thema(
            &service,
            "mixed-cloze",
            "cloze",
            "Alpha",
            vec![Some("hint".into())],
            vec!["alpha".into()],
            None,
        )
        .await;
        assert_http_attempt_presentation(
            &app,
            &token,
            "mixed-cloze",
            "(hint)",
            None,
            json!({"kind":"texts","input_scripts":[null]}),
        )
        .await;

        put_custom_review_thema(
            &service,
            "basic-hint",
            "basic",
            "Question",
            vec![Some("Remember this".into())],
            vec!["answer".into()],
            None,
        )
        .await;
        assert_http_attempt_presentation(
            &app,
            &token,
            "basic-hint",
            "Question",
            Some("Remember this"),
            json!({"kind":"text","input_script":null}),
        )
        .await;

        put_custom_review_thema(
            &service,
            "mixed-choice",
            "mc-cloze",
            "Alpha",
            vec![Some("alpha".into()), Some("beta".into())],
            vec!["alpha".into()],
            None,
        )
        .await;
        assert_http_attempt_presentation(
            &app,
            &token,
            "mixed-choice",
            "(...)",
            None,
            json!({"kind":"choice","choices":["alpha","beta"]}),
        )
        .await;

        put_custom_review_thema(
            &service,
            "unsafe",
            "cloze",
            "secret and secret",
            vec![],
            vec!["secret".into()],
            None,
        )
        .await;
        let (_, uri) = start_http_review(&app, &token, "unsafe").await;
        let (status, skipped) = begin_http_review_attempt(&app, &token, &uri).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(skipped["attempt"], Value::Null);
        assert_eq!(skipped["session"]["status"], "completed");
        assert_eq!(skipped["skipped"][0]["reason"], "unsafe_presentation");

        put_custom_review_thema(
            &service,
            "unsafe",
            "cloze",
            "secret once",
            vec![],
            vec!["secret".into()],
            Some(1),
        )
        .await;
        let (_, uri) = start_http_review(&app, &token, "unsafe").await;
        let (status, recovered) = begin_http_review_attempt(&app, &token, &uri).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            recovered["attempt"]["presentation"]["question"],
            "(...) once"
        );

        drop(app);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[test]
    fn pending_attempts_do_not_reveal_captured_answers() {
        let attempt = ReviewAttemptView {
            attempt_id: "attempt".into(),
            session_id: "session".into(),
            thema_id: "thema".into(),
            presentation: PresentationSnapshot {
                kind: "cloze".into(),
                keimenon: "Question with μυστικό and тайна".into(),
                hypothesis: vec![Some("Greek hint".into()), Some("Cyrillic hint".into())],
                answer: vec!["μυστικό".into(), "тайна".into()],
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
        let pending =
            serde_json::to_value(ReviewAttemptV1::from_application(&attempt).unwrap()).unwrap();
        assert!(pending["revealed_answer"].is_null());
        assert!(pending["presentation"].get("answer").is_none());
        assert!(pending["presentation"].get("keimenon").is_none());
        assert_eq!(
            pending["presentation"]["question"],
            "Question with (Greek hint) and (Cyrillic hint)"
        );
        assert!(pending["presentation"]["hint"].is_null());
        assert_eq!(
            pending["presentation"]["response"],
            json!({"kind":"texts","input_scripts":["greek","cyrillic"]})
        );
        let encoded = serde_json::to_string(&pending).unwrap();
        assert!(!encoded.contains("μυστικό"));
        assert!(!encoded.contains("тайна"));
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

    fn authorized_mutation(
        method: &str,
        uri: &str,
        token: &str,
        operation_id: &str,
        if_match: Option<&str>,
        body: Body,
    ) -> Request<Body> {
        let mut request = authorized(method, uri, token, body);
        request
            .headers_mut()
            .insert("idempotency-key", operation_id.parse().unwrap());
        if let Some(value) = if_match {
            request
                .headers_mut()
                .insert("if-match", value.parse().unwrap());
        }
        request
    }

    fn authorized_if_match(
        method: &str,
        uri: &str,
        token: &str,
        if_match: &str,
        body: Body,
    ) -> Request<Body> {
        let mut request = authorized(method, uri, token, body);
        request
            .headers_mut()
            .insert("if-match", if_match.parse().unwrap());
        request
    }

    fn note_body(logical_path: &str, title: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "logical_path":logical_path,
            "content_type":"text/org; charset=utf-8",
            "source":format!("#+title: {title}\n")
        }))
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
        source: &str,
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
                    "content_sha256": format!("{:x}", Sha256::digest(source.as_bytes())),
                    "content_type": "text/org; charset=utf-8",
                    "source": source,
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
        let config = item.config.clone().into();
        let transition = compute_transition(
            &state,
            &config,
            detected_success,
            review_day,
            answered_at_us,
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
            config: item.config.clone(),
            matcher_version: item.matcher_version,
            response,
            detected_success,
            accepted_success: detected_success,
            override_count: 0,
            proposed_transition: (&transition).into(),
            desired_suspended: false,
            answered_at_us,
        }
    }
}
