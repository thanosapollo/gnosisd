use super::{PreparedNote, prepare_document};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ContentBase {
    Absent,
    Revision {
        revision: u64,
        content_sha256: Option<String>,
        predecessor_operation_id: Option<String>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum OfflineContentMutation {
    Put {
        document_id: String,
        logical_path: String,
        content_sha256: String,
        content_type: String,
        source: String,
        base: ContentBase,
    },
    Rename {
        document_id: String,
        logical_path: String,
        base: ContentBase,
    },
    Tombstone {
        document_id: String,
        base: ContentBase,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct OfflineContentOperation {
    pub(crate) operation_id: String,
    pub(crate) device_id: String,
    pub(crate) device_sequence: u64,
    pub(crate) cursor_server_id: String,
    pub(crate) cursor: u64,
    pub(crate) cursor_token: String,
    pub(crate) authored_at_us: u64,
    pub(crate) mutation: OfflineContentMutation,
}

impl OfflineContentOperation {
    pub(crate) fn document_id(&self) -> &str {
        match &self.mutation {
            OfflineContentMutation::Put { document_id, .. } => document_id,
            OfflineContentMutation::Rename { document_id, .. } => document_id,
            OfflineContentMutation::Tombstone { document_id, .. } => document_id,
        }
    }

    pub(crate) const fn base(&self) -> &ContentBase {
        match &self.mutation {
            OfflineContentMutation::Put { base, .. } => base,
            OfflineContentMutation::Rename { base, .. } => base,
            OfflineContentMutation::Tombstone { base, .. } => base,
        }
    }

    pub(crate) const fn mutation_kind(&self) -> &'static str {
        match &self.mutation {
            OfflineContentMutation::Put { .. } => "put_document",
            OfflineContentMutation::Rename { .. } => "rename_document",
            OfflineContentMutation::Tombstone { .. } => "tombstone_document",
        }
    }

    pub(crate) fn submitted_content_sha256(&self) -> Option<&str> {
        match &self.mutation {
            OfflineContentMutation::Put { content_sha256, .. } => Some(content_sha256),
            OfflineContentMutation::Rename { .. } | OfflineContentMutation::Tombstone { .. } => {
                None
            }
        }
    }

    pub(crate) fn logical_path(&self) -> Option<&str> {
        match &self.mutation {
            OfflineContentMutation::Put { logical_path, .. }
            | OfflineContentMutation::Rename { logical_path, .. } => Some(logical_path),
            OfflineContentMutation::Tombstone { .. } => None,
        }
    }
}

impl ContentBase {
    pub(crate) fn content_sha256(&self) -> Option<&str> {
        match self {
            Self::Absent => None,
            Self::Revision { content_sha256, .. } => content_sha256.as_deref(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OfflineContentConflictReason {
    CursorChanged,
    BaseChanged,
    InvalidContent,
    PathConflict,
}

impl OfflineContentConflictReason {
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
pub(crate) enum ContentReconciliationStatus {
    Accepted,
    Conflict,
}

impl ContentReconciliationStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Conflict => "conflict",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OfflineContentResult {
    pub(crate) operation_id: String,
    pub(crate) format_version: u32,
    pub(crate) status: ContentReconciliationStatus,
    pub(crate) conflict_reason: Option<OfflineContentConflictReason>,
    pub(crate) authoritative: Option<AuthoritativeDocument>,
    pub(crate) cursor: u64,
    pub(crate) cursor_server_id: String,
    pub(crate) cursor_token: String,
}

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
pub(crate) struct AuthoritativeDocument {
    pub(crate) document_id: String,
    pub(crate) logical_path: String,
    pub(crate) revision: u64,
    pub(crate) content_sha256: Option<String>,
    pub(crate) active: bool,
}

pub(crate) fn prepare_offline_document(
    operation: &OfflineContentOperation,
) -> Result<Option<PreparedNote>, OfflineContentConflictReason> {
    let OfflineContentMutation::Put {
        document_id,
        logical_path,
        content_sha256,
        content_type,
        source,
        ..
    } = &operation.mutation
    else {
        return Ok(None);
    };
    let prepared = prepare_document(
        Some(document_id.clone()),
        logical_path.clone(),
        content_type.clone(),
        source.clone(),
        None,
    )
    .map_err(|_| OfflineContentConflictReason::InvalidContent)?;
    if prepared.blob_sha256 != *content_sha256 {
        return Err(OfflineContentConflictReason::InvalidContent);
    }
    Ok(Some(prepared))
}
