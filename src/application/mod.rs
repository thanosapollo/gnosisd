mod auth;
mod backup;
mod content;
mod import_document;
mod query_document;
mod query_legacy;
mod review;
mod service;
mod settings;

pub(crate) use auth::{
    DevicePrincipal, DeviceRecord, DeviceScope, NewDevice, generate_device, parse_stored_scopes,
    stored_scopes, token_digest,
};
pub(crate) use backup::{
    BackupError, BackupSummary, create_backup, prune_backups, validate_restore, verify_backup,
};
pub(crate) use content::{
    AssetHead, ContentChange, DocumentHead, DocumentRevision, NodeRecord, PreparedAsset,
    PreparedNote, PreparedThema, ThemaRecord, canonical_asset_path, file_link_expressions,
    path_key, prepare_asset, prepare_note, prepare_thema, stored_thema_content, supported_asset,
    thema_content, validate_logical_path, validate_org_path,
};
pub(crate) use import_document::persist_import;
pub use import_document::{ApplicationError, ImportDocument, ImportSummary, PreparedImport};
pub use query_document::QueryDocument;
pub(crate) use query_legacy::DueCursor;
pub use query_legacy::{LegacyQueryError, QueryDueThemata, QueryDueThemataPage, QueryLegacyThema};
pub(crate) use review::{
    AnswerInput, MAX_REVIEW_PACK_BYTES, OfflineConflictReason, OfflineReviewOperation,
    OfflineReviewResult, ParametersSnapshot, PresentationSnapshot, ReconciliationStatus,
    ReviewAttemptView, ReviewError, ReviewPackAsset, ReviewPackItem, ReviewPackView,
    ReviewSessionMode, ReviewSessionView, ReviewSource, ReviewSourceKind, ReviewStateSnapshot,
    TransitionSnapshot, presentation_content, review_day_at,
};
pub(crate) use service::{ServiceError, ServiceHandle, ServiceRuntime, ServiceStartError};
pub(crate) use settings::{SettingsError, SettingsInput, SettingsRecord, TagRule};
