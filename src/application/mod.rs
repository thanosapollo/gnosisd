mod auth;
mod client_views;
mod content;
mod device_mutation;
mod import_document;
mod offline_asset;
mod offline_content;
mod query_document;
mod query_legacy;
mod review;
mod settings;
mod sync;

pub(crate) const MAX_REVIEW_PACK_ID_CHARS: usize = 128;
pub(crate) const MAX_REVIEW_PACK_ITEMS: usize = 500;
pub(crate) const MAX_REVIEW_PACK_MEDIA_TYPE_CHARS: usize = 255;

pub(crate) use auth::{
    DevicePrincipal, DeviceRecord, DeviceScope, NewDevice, generate_device, parse_stored_scopes,
    stored_scopes, token_digest,
};
pub(crate) use client_views::{
    ContentCounts, DashboardSummary, ReviewHistoryEvent, ReviewHistoryQuery, ReviewQueueCounts,
    ReviewQueueSummary, ReviewSessionQuery, ReviewSessionSummary, ReviewTotals, ReviewWindow,
    ThemaReviewState,
};
#[cfg(test)]
pub(crate) use content::ContentError;
pub(crate) use content::{
    AssetHead, AssetRevision, ContentChange, DocumentHead, DocumentProjectionSnapshot,
    DocumentReplicaRevision, DocumentRevision, NodeRecord, PreparedAsset, PreparedNote,
    PreparedThema, ProjectionEdge, ProjectionLinkOccurrence, ProjectionNode, ThemaRecord,
    ThemaRevision, asset_paths_in_org, canonical_asset_path, path_key, prepare_asset, prepare_note,
    prepare_thema, retained_asset_paths_in_org, stored_thema_content, thema_content,
    validate_asset_path, validate_offline_thema_assets, validate_org_path,
};
pub(crate) use device_mutation::{
    DeviceDocumentLifecycleMutation, DeviceMutationCursor, DeviceMutationIdentity,
    DeviceMutationKind, DeviceMutationProjection, DeviceMutationRejection, DeviceMutationResult,
    PreparedDeviceDocumentLifecycleMutation, PreparedDeviceDocumentMutation,
    prepare_device_document_mutation, prepare_device_document_rename,
    prepare_device_document_tombstone,
};
pub use import_document::ApplicationError;
#[cfg(test)]
pub use import_document::{ImportDocument, ImportSummary};
#[cfg(test)]
pub(crate) use import_document::{PreparedServerImport, summarize_import};
pub(crate) use offline_asset::{
    AssetReconciliationStatus, AuthoritativeAsset, MAX_OFFLINE_ASSET_BYTES,
    OfflineAssetConflictReason, OfflineAssetOperation, OfflineAssetResult,
};
pub(crate) use offline_content::{
    AuthoritativeDocument, ContentBase, ContentReconciliationStatus, OfflineContentConflictReason,
    OfflineContentMutation, OfflineContentOperation, OfflineContentResult,
    prepare_offline_document,
};
pub use query_document::QueryDocument;
pub(crate) use query_legacy::QueryDueThemataPage;
pub(crate) use query_legacy::{DueCursor, MAX_DUE_CURSOR_BYTES};
pub(crate) use review::{
    AnswerInput, FsrsAlgorithmSnapshot, FsrsConfigSnapshot, FsrsImplementationSnapshot,
    FsrsModelSnapshot, MAX_REVIEW_PACK_BYTES, OfflineAnswerError, OfflineAnswerInput,
    OfflineConflictReason, OfflineReviewOperation, OfflineReviewResult, PresentationSnapshot,
    RecallOutcomeSnapshot, ReconciliationStatus, ReviewAttemptMutation, ReviewAttemptOperation,
    ReviewAttemptOperationResult, ReviewAttemptOperationStatus, ReviewAttemptState,
    ReviewAttemptView, ReviewError, ReviewOperationAuthority, ReviewPackAsset, ReviewPackItem,
    ReviewPackView, ReviewResponsePrompt, ReviewSessionMode, ReviewSessionView, ReviewSource,
    ReviewSourceKind, ReviewStateSnapshot, TransitionSnapshot, prepare_offline_operation,
    presentation_content, review_day_at, review_question, valid_operation_id,
    validate_review_pack_projection,
};
pub(crate) use settings::{SettingsError, SettingsInput, SettingsRecord};
pub(crate) use sync::{
    ReplicaChange, ReplicaProjection, ReplicaState, ReviewStateRevision, SettingsRevision,
    SnapshotPage, SnapshotPosition,
};
