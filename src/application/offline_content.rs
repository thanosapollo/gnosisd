use super::{DocumentHead, PreparedNote, prepare_note};

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
    PutDocument {
        document_id: String,
        logical_path: String,
        content_sha256: String,
        org: String,
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
            OfflineContentMutation::PutDocument { document_id, .. } => document_id,
        }
    }

    pub(crate) const fn base(&self) -> &ContentBase {
        match &self.mutation {
            OfflineContentMutation::PutDocument { base, .. } => base,
        }
    }

    pub(crate) fn content_sha256(&self) -> &str {
        match &self.mutation {
            OfflineContentMutation::PutDocument { content_sha256, .. } => content_sha256,
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

impl From<DocumentHead> for AuthoritativeDocument {
    fn from(value: DocumentHead) -> Self {
        Self {
            document_id: value.document_id,
            logical_path: value.logical_path,
            revision: value.revision,
            content_sha256: value.blob_sha256,
            active: value.active,
        }
    }
}

pub(crate) fn prepare_offline_document(
    operation: &OfflineContentOperation,
) -> Result<PreparedNote, OfflineContentConflictReason> {
    let OfflineContentMutation::PutDocument {
        document_id,
        logical_path,
        content_sha256,
        org,
        ..
    } = &operation.mutation;
    let prepared = prepare_note(
        Some(document_id.clone()),
        logical_path.clone(),
        org.clone(),
        None,
    )
    .map_err(|_| OfflineContentConflictReason::InvalidContent)?;
    if prepared.blob_sha256 != *content_sha256 {
        return Err(OfflineContentConflictReason::InvalidContent);
    }
    Ok(prepared)
}
