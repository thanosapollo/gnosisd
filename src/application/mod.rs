mod auth;
mod backup;
mod content;
mod import_document;
mod offline_content;
mod query_document;
mod query_legacy;
mod review;
mod service;
mod settings;
mod sync;

pub(crate) use auth::{
    DevicePrincipal, DeviceRecord, DeviceScope, NewDevice, generate_device, parse_stored_scopes,
    stored_scopes, token_digest,
};
pub(crate) use backup::{
    BackupError, BackupSummary, create_backup, prune_backups, validate_restore, verify_backup,
};
pub(crate) use content::{
    AssetHead, AssetRevision, ContentChange, DocumentHead, DocumentProjectionSnapshot,
    DocumentReplicaRevision, DocumentRevision, NodeRecord, PreparedAsset, PreparedNote,
    PreparedThema, ProjectionEdge, ProjectionLinkOccurrence, ProjectionNode, ThemaRecord,
    ThemaRevision, canonical_asset_path, file_link_expressions, path_key, prepare_asset,
    prepare_note, prepare_thema, retained_asset_paths_in_org, stored_thema_content,
    supported_asset, thema_content, validate_asset_path, validate_logical_path,
    validate_offline_thema_assets, validate_org_path,
};
pub use import_document::{ApplicationError, ImportDocument, ImportSummary, PreparedImport};
pub(crate) use import_document::{PreparedServerImport, persist_server_import};
pub(crate) use offline_content::{
    AuthoritativeDocument, ContentBase, ContentReconciliationStatus, OfflineContentConflictReason,
    OfflineContentMutation, OfflineContentOperation, OfflineContentResult,
    prepare_offline_document,
};
pub use query_document::QueryDocument;
pub(crate) use query_legacy::DueCursor;
pub use query_legacy::{LegacyQueryError, QueryDueThemata, QueryDueThemataPage, QueryLegacyThema};
pub(crate) use review::{
    AnswerInput, MAX_REVIEW_PACK_BYTES, OfflineAnswerError, OfflineAnswerInput,
    OfflineConflictReason, OfflineReviewOperation, OfflineReviewResult, ParametersSnapshot,
    PresentationSnapshot, ReconciliationStatus, ReviewAttemptView, ReviewError, ReviewPackAsset,
    ReviewPackItem, ReviewPackView, ReviewSessionMode, ReviewSessionView, ReviewSource,
    ReviewSourceKind, ReviewStateSnapshot, TransitionSnapshot, prepare_offline_operation,
    presentation_content, review_day_at,
};
pub(crate) use service::{ServiceError, ServiceHandle, ServiceRuntime, ServiceStartError};
pub(crate) use settings::{SettingsError, SettingsInput, SettingsRecord, TagRule};
pub(crate) use sync::{
    ReplicaChange, ReplicaProjection, ReplicaState, ReviewStateRevision, SettingsRevision,
    SnapshotPage, SnapshotPosition,
};
