pub(crate) const MAX_OFFLINE_ASSET_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OfflineAssetOperation {
    pub(crate) operation_id: String,
    pub(crate) device_id: String,
    pub(crate) device_sequence: u64,
    pub(crate) cursor_server_id: String,
    pub(crate) cursor: u64,
    pub(crate) cursor_token: String,
    pub(crate) authored_at_us: u64,
    pub(crate) logical_path: String,
    pub(crate) original_name: String,
    pub(crate) content_sha256: String,
    pub(crate) size_bytes: u64,
    pub(crate) media_type: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OfflineAssetConflictReason {
    CursorChanged,
    BaseChanged,
    InvalidContent,
    PathConflict,
}

impl OfflineAssetConflictReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::CursorChanged => "cursor_changed",
            Self::BaseChanged => "base_changed",
            Self::InvalidContent => "invalid_content",
            Self::PathConflict => "path_conflict",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AssetReconciliationStatus {
    Accepted,
    Conflict,
}

impl AssetReconciliationStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Conflict => "conflict",
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
pub(crate) struct AuthoritativeAsset {
    pub(crate) logical_path: String,
    pub(crate) revision: u64,
    pub(crate) content_sha256: String,
    pub(crate) size_bytes: u64,
    pub(crate) media_type: String,
    pub(crate) active: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OfflineAssetResult {
    pub(crate) operation_id: String,
    pub(crate) format_version: u32,
    pub(crate) status: AssetReconciliationStatus,
    pub(crate) conflict_reason: Option<OfflineAssetConflictReason>,
    pub(crate) authoritative: Option<AuthoritativeAsset>,
    pub(crate) cursor: u64,
    pub(crate) cursor_server_id: String,
    pub(crate) cursor_token: String,
}
