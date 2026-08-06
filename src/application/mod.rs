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
pub(crate) const DEFAULT_ONLINE_REVIEW_TARGET_COUNT: u32 = 100;
pub(crate) const MAX_ONLINE_REVIEW_TARGET_COUNT: u32 = 500;

#[cfg(test)]
pub(crate) use auth::generate_device;
pub use auth::{
    AuthError, ProvisioningCredential, generate_provisioning_credential,
    validate_provisioning_credential,
};
pub(crate) use auth::{
    DevicePrincipal, DeviceRecord, DeviceScope, NewDevice, parse_stored_scopes,
    prepare_provisioned_device, stored_scopes, token_digest,
};
pub(crate) use client_views::{
    ContentCounts, DashboardSummary, ReviewHistoryEvent, ReviewHistoryQuery, ReviewQueueCounts,
    ReviewQueueSummary, ReviewSessionQuery, ReviewSessionSummary, ReviewTotals, ReviewWindow,
    ThemaReviewState,
};
pub(crate) use content::{
    AssetHead, AssetRevision, ContentChange, ContentSearchGeneration, ContentSearchHit,
    ContentSearchItem, ContentSearchPage, DocumentHead, DocumentProjectionSnapshot,
    DocumentReplicaRevision, DocumentRevision, MAX_DOCUMENT_PROJECTION_BYTES,
    MAX_DOCUMENT_SOURCE_BYTES, NodeCatalogRecord, NodeRecord, NodeThemaAssociation, PreparedAsset,
    PreparedNote, PreparedThema, ProjectionEdge, ProjectionLinkOccurrence, ProjectionNode,
    ThemaCatalogRecord, ThemaRecord, ThemaRevision, canonical_asset_path, path_key, prepare_asset,
    prepare_document, prepare_thema, retained_asset_paths, stored_thema_content, thema_content,
    validate_asset_path, validate_blob_metadata, validate_document_path,
    validate_offline_thema_assets,
};
#[cfg(test)]
pub(crate) use content::{ContentError, prepare_note};
pub(crate) use device_mutation::{
    DeviceDesiredStateMutation, DeviceDocumentLifecycleMutation, DeviceMutationCursor,
    DeviceMutationError, DeviceMutationIdentity, DeviceMutationKind, DeviceMutationProjection,
    DeviceMutationRejection, DeviceMutationResult, PreparedDeviceDesiredStateMutation,
    PreparedDeviceDocumentLifecycleMutation, PreparedDeviceDocumentMutation,
    ThemaMutationProjection, desired_state_wire_request_sha256,
    prepare_device_desired_state_mutation, prepare_device_document_mutation,
    prepare_device_document_rename, prepare_device_document_tombstone,
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
    FsrsModelSnapshot, MAX_REVIEW_PACK_BYTES, OfflineConflictReason, OfflineReviewOperation,
    OfflineReviewResult, PresentationSnapshot, RecallOutcomeSnapshot, ReconciliationStatus,
    ReviewAttemptBeginResult, ReviewAttemptMutation, ReviewAttemptOperation,
    ReviewAttemptOperationResult, ReviewAttemptOperationStatus, ReviewAttemptState,
    ReviewAttemptView, ReviewClozeCheck, ReviewClozeSpan, ReviewClozeState, ReviewError,
    ReviewItemSkipReason, ReviewOperationAuthority, ReviewPackAsset, ReviewPackItem,
    ReviewPackView, ReviewQuestion, ReviewResponsePrompt, ReviewSessionMode, ReviewSessionView,
    ReviewSource, ReviewSourceKind, ReviewStateSnapshot, SkippedReviewItem, TransitionSnapshot,
    presentation_content, progressive_cloze_check, review_attempt_question, review_day_at,
    review_question, valid_operation_id, validate_review_pack_projection,
};
pub(crate) use settings::{SettingsError, SettingsInput, SettingsRecord};
pub(crate) use sync::{
    NodeCatalogPage, ReplicaChange, ReplicaProjection, ReplicaState, ReviewStateRevision,
    SettingsRevision, SnapshotPage, SnapshotPosition, ThemaCatalogPage,
};
