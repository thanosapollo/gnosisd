use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    application::{
        AuthoritativeDocument, ContentBase, ContentReconciliationStatus,
        OfflineContentConflictReason, OfflineContentMutation, OfflineContentOperation,
        OfflineContentResult, validate_org_path,
    },
    domain::DocumentId,
    is_canonical_sha256,
    protocol::{
        http_v1::ContentCapabilitiesV1,
        replica::{decode_cursor, encode_cursor},
    },
};

pub(crate) const OFFLINE_CONTENT_VERSION: u32 = 1;
const OFFLINE_CONTENT_CAPABILITIES_VERSION: u32 = 1;
pub(crate) const MAX_OFFLINE_CONTENT_OPERATION_BYTES: usize = 1024 * 1024;
pub(crate) const MAX_OFFLINE_CONTENT_RESULT_BYTES: usize = 128 * 1024;

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

fn deserialize_version_1<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u32::deserialize(deserializer)?;
    if version == OFFLINE_CONTENT_VERSION {
        Ok(version)
    } else {
        Err(serde::de::Error::custom("protocol version is unsupported"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OfflineContentSubmission {
    version: u32,
    operation: Value,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineContentOperationEnvelopeV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    pub operation: OfflineContentOperationV1,
}

pub(crate) struct DecodedOfflineContent {
    pub(crate) version: u32,
    pub(crate) operation: OfflineContentOperation,
    pub(crate) canonical_payload: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedOfflineContentResult {
    pub(crate) operation_id: String,
    pub(crate) status: ContentReconciliationStatus,
    pub(crate) conflict_reason: Option<OfflineContentConflictReason>,
    pub(crate) authoritative: Option<AuthoritativeDocument>,
    pub(crate) cursor: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedOfflineContentCapabilities {
    pub(crate) server_id: String,
}

#[derive(Debug, Error)]
pub(crate) enum OfflineContentWireError {
    #[error("offline content protocol version is unsupported")]
    Version,
    #[error("offline content operation is invalid")]
    Operation,
}

impl OfflineContentSubmission {
    pub(crate) fn decode(self) -> Result<DecodedOfflineContent, OfflineContentWireError> {
        if self.version != OFFLINE_CONTENT_VERSION {
            return Err(OfflineContentWireError::Version);
        }
        let wire: OfflineContentOperationV1 =
            serde_json::from_value(self.operation).map_err(invalid_operation)?;
        validate_operation(&wire)?;
        let canonical_payload = encode_operation(&wire)?;
        Ok(DecodedOfflineContent {
            version: self.version,
            operation: wire.into(),
            canonical_payload,
        })
    }
}

pub(crate) fn canonical_content_operation(
    operation: &OfflineContentOperation,
) -> Result<String, OfflineContentWireError> {
    let wire = OfflineContentOperationV1::from(operation);
    validate_operation(&wire)?;
    encode_operation(&wire)
}

pub(crate) fn offline_content_capabilities(
    server_id: &str,
) -> Result<ContentCapabilitiesV1, OfflineContentWireError> {
    if !is_hex_identity(server_id) {
        return Err(OfflineContentWireError::Operation);
    }
    Ok(ContentCapabilitiesV1 {
        version: OFFLINE_CONTENT_CAPABILITIES_VERSION,
        server_id: server_id.to_owned(),
        document_lifecycle: 1,
    })
}

pub(crate) fn decode_offline_content_capabilities(
    capabilities: ContentCapabilitiesV1,
) -> Result<DecodedOfflineContentCapabilities, OfflineContentWireError> {
    if capabilities.version != OFFLINE_CONTENT_CAPABILITIES_VERSION
        || capabilities.document_lifecycle != 1
        || !is_hex_identity(&capabilities.server_id)
    {
        return Err(OfflineContentWireError::Operation);
    }
    Ok(DecodedOfflineContentCapabilities {
        server_id: capabilities.server_id,
    })
}

#[cfg(test)]
pub(crate) fn offline_content_result_json(
    result: &OfflineContentResult,
) -> Result<Value, OfflineContentWireError> {
    serde_json::to_value(offline_content_result_response(result)?).map_err(invalid_operation)
}

pub(crate) fn offline_content_result_response(
    result: &OfflineContentResult,
) -> Result<OfflineContentResultEnvelopeV1, OfflineContentWireError> {
    let cursor = encode_cursor(
        &result.cursor_server_id,
        result.cursor,
        &result.cursor_token,
    )
    .map_err(|_| OfflineContentWireError::Operation)?;
    let envelope = content_result_envelope(result, cursor.clone(), result.authoritative.as_ref())?;
    match require_serialized_size(&envelope, MAX_OFFLINE_CONTENT_RESULT_BYTES) {
        Ok(()) => Ok(envelope),
        Err(_) if result.status == ContentReconciliationStatus::Conflict => {
            let envelope = content_result_envelope(result, cursor, None)?;
            require_serialized_size(&envelope, MAX_OFFLINE_CONTENT_RESULT_BYTES)?;
            Ok(envelope)
        }
        Err(error) => Err(error),
    }
}

fn content_result_envelope(
    result: &OfflineContentResult,
    cursor: String,
    authoritative: Option<&AuthoritativeDocument>,
) -> Result<OfflineContentResultEnvelopeV1, OfflineContentWireError> {
    let wire = match result.status {
        ContentReconciliationStatus::Accepted => OfflineContentResultV1::Accepted {
            operation_id: result.operation_id.clone(),
            conflict_reason: require_absent(result.conflict_reason)?,
            authoritative: OfflineContentDocumentV1::from(
                authoritative.ok_or(OfflineContentWireError::Operation)?,
            ),
            cursor,
        },
        ContentReconciliationStatus::Conflict => OfflineContentResultV1::Conflict {
            operation_id: result.operation_id.clone(),
            conflict_reason: result
                .conflict_reason
                .ok_or(OfflineContentWireError::Operation)?
                .into(),
            authoritative: authoritative.map(OfflineContentDocumentV1::from),
            cursor,
        },
    };
    Ok(OfflineContentResultEnvelopeV1 {
        version: OFFLINE_CONTENT_VERSION,
        result: wire,
    })
}

pub(crate) fn decode_offline_content_result(
    value: Value,
) -> Result<DecodedOfflineContentResult, OfflineContentWireError> {
    require_size(&value, MAX_OFFLINE_CONTENT_RESULT_BYTES)?;
    require_response_version(&value)?;
    let envelope: OfflineContentResultEnvelopeV1 =
        serde_json::from_value(value).map_err(invalid_operation)?;
    decode_result(envelope.result)
}

fn require_response_version(value: &Value) -> Result<(), OfflineContentWireError> {
    match value.get("version").and_then(Value::as_u64) {
        Some(version) if version == u64::from(OFFLINE_CONTENT_VERSION) => Ok(()),
        Some(_) => Err(OfflineContentWireError::Version),
        None => Err(OfflineContentWireError::Operation),
    }
}

fn encode_operation(
    operation: &OfflineContentOperationV1,
) -> Result<String, OfflineContentWireError> {
    let encoded = serde_json::to_string(&VersionedOperationV1 {
        version: OFFLINE_CONTENT_VERSION,
        operation,
    })
    .map_err(invalid_operation)?;
    if encoded.len() > MAX_OFFLINE_CONTENT_OPERATION_BYTES {
        return Err(OfflineContentWireError::Operation);
    }
    Ok(encoded)
}

fn validate_operation(value: &OfflineContentOperationV1) -> Result<(), OfflineContentWireError> {
    require_identity(&value.operation_id)?;
    require_identity(&value.device_id)?;
    if value.device_sequence == 0
        || value.device_sequence > i64::MAX as u64
        || value.authored_at_us == 0
        || value.authored_at_us > i64::MAX as u64
    {
        return Err(OfflineContentWireError::Operation);
    }
    validate_cursor(&value.cursor)?;
    match &value.mutation {
        OfflineContentMutationV1::Put {
            document_id,
            logical_path,
            content_sha256,
            org,
            base,
        } => validate_document_put(document_id, logical_path, content_sha256, org, base),
        OfflineContentMutationV1::Rename {
            document_id,
            logical_path,
            base,
        } => validate_document_rename(document_id, logical_path, base),
        OfflineContentMutationV1::Tombstone { document_id, base } => {
            validate_document_tombstone(document_id, base)
        }
    }
}

fn validate_document_put(
    document_id: &str,
    logical_path: &str,
    content_sha256: &str,
    org: &str,
    base: &OfflineContentBaseV1,
) -> Result<(), OfflineContentWireError> {
    DocumentId::new(document_id.to_owned()).map_err(invalid_operation)?;
    if logical_path.len() > 4096 {
        return Err(OfflineContentWireError::Operation);
    }
    validate_org_path(logical_path).map_err(invalid_operation)?;
    require_digest(content_sha256)?;
    let actual = format!("{:x}", Sha256::digest(org.as_bytes()));
    if actual != content_sha256 || !valid_content_base(base) {
        return Err(OfflineContentWireError::Operation);
    }
    Ok(())
}

fn validate_document_rename(
    document_id: &str,
    logical_path: &str,
    base: &OfflineContentBaseV1,
) -> Result<(), OfflineContentWireError> {
    validate_document_identity(document_id)?;
    if logical_path.len() > 4096 {
        return Err(OfflineContentWireError::Operation);
    }
    validate_org_path(logical_path).map_err(invalid_operation)?;
    require_revision_base(base)
}

fn validate_document_tombstone(
    document_id: &str,
    base: &OfflineContentBaseV1,
) -> Result<(), OfflineContentWireError> {
    validate_document_identity(document_id)?;
    require_revision_base(base)
}

fn validate_document_identity(document_id: &str) -> Result<(), OfflineContentWireError> {
    DocumentId::new(document_id.to_owned())
        .map(|_| ())
        .map_err(invalid_operation)
}

fn require_revision_base(base: &OfflineContentBaseV1) -> Result<(), OfflineContentWireError> {
    if matches!(base, OfflineContentBaseV1::Revision { .. }) && valid_content_base(base) {
        Ok(())
    } else {
        Err(OfflineContentWireError::Operation)
    }
}

fn valid_content_base(base: &OfflineContentBaseV1) -> bool {
    match base {
        OfflineContentBaseV1::Absent => true,
        OfflineContentBaseV1::Revision {
            revision,
            content_sha256,
            predecessor_operation_id,
        } => {
            if *revision == 0 || *revision >= i64::MAX as u64 {
                return false;
            }
            let Some(content_sha256) = content_sha256.as_deref() else {
                return false;
            };
            if require_digest(content_sha256).is_err() {
                return false;
            }
            predecessor_operation_id
                .as_deref()
                .is_none_or(|id| require_identity(id).is_ok())
        }
    }
}

fn decode_result(
    value: OfflineContentResultV1,
) -> Result<DecodedOfflineContentResult, OfflineContentWireError> {
    let (operation_id, status, conflict_reason, authoritative, cursor) = match value {
        OfflineContentResultV1::Accepted {
            operation_id,
            authoritative,
            cursor,
            ..
        } => (
            operation_id,
            ContentReconciliationStatus::Accepted,
            None,
            Some(authoritative.into()),
            cursor,
        ),
        OfflineContentResultV1::Conflict {
            operation_id,
            conflict_reason,
            authoritative,
            cursor,
        } => (
            operation_id,
            ContentReconciliationStatus::Conflict,
            Some(conflict_reason.into()),
            authoritative.map(AuthoritativeDocument::from),
            cursor,
        ),
    };
    require_identity(&operation_id)?;
    validate_cursor(&cursor)?;
    if let Some(document) = &authoritative {
        validate_authoritative(document)?;
    }
    Ok(DecodedOfflineContentResult {
        operation_id,
        status,
        conflict_reason,
        authoritative,
        cursor,
    })
}

fn validate_authoritative(document: &AuthoritativeDocument) -> Result<(), OfflineContentWireError> {
    DocumentId::new(document.document_id.clone()).map_err(invalid_operation)?;
    validate_org_path(&document.logical_path).map_err(invalid_operation)?;
    if document.revision == 0
        || document.revision > i64::MAX as u64
        || document.active != document.content_sha256.is_some()
    {
        return Err(OfflineContentWireError::Operation);
    }
    if let Some(digest) = &document.content_sha256 {
        require_digest(digest)?;
    }
    Ok(())
}

fn require_serialized_size(
    value: &impl Serialize,
    maximum: usize,
) -> Result<(), OfflineContentWireError> {
    let bytes = serde_json::to_vec(value).map_err(invalid_operation)?;
    if bytes.len() > maximum {
        Err(OfflineContentWireError::Operation)
    } else {
        Ok(())
    }
}

fn require_size(value: &Value, maximum: usize) -> Result<(), OfflineContentWireError> {
    let bytes = serde_json::to_vec(value).map_err(invalid_operation)?;
    if bytes.len() > maximum {
        Err(OfflineContentWireError::Operation)
    } else {
        Ok(())
    }
}

fn require_absent<T>(value: Option<T>) -> Result<(), OfflineContentWireError> {
    value
        .is_none()
        .then_some(())
        .ok_or(OfflineContentWireError::Operation)
}

fn require_identity(value: &str) -> Result<(), OfflineContentWireError> {
    if value.is_empty() || value.len() > 4096 {
        Err(OfflineContentWireError::Operation)
    } else {
        Ok(())
    }
}

fn require_digest(value: &str) -> Result<(), OfflineContentWireError> {
    if is_canonical_sha256(value) {
        Ok(())
    } else {
        Err(OfflineContentWireError::Operation)
    }
}

fn validate_cursor(value: &str) -> Result<(), OfflineContentWireError> {
    let (server_id, position, token) = decode_cursor(value).map_err(invalid_operation)?;
    if position > i64::MAX as u64 || !is_hex_identity(&server_id) || !is_hex_identity(&token) {
        Err(OfflineContentWireError::Operation)
    } else {
        Ok(())
    }
}

fn is_hex_identity(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn invalid_operation<T>(_error: T) -> OfflineContentWireError {
    OfflineContentWireError::Operation
}

#[derive(Serialize)]
struct VersionedOperationV1<'a> {
    version: u32,
    operation: &'a OfflineContentOperationV1,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineContentOperationV1 {
    #[schemars(length(min = 1, max = 4096))]
    pub operation_id: String,
    #[schemars(length(min = 1, max = 4096))]
    pub device_id: String,
    #[schemars(range(min = 1))]
    pub device_sequence: u64,
    #[schemars(length(min = 1, max = 512))]
    pub cursor: String,
    #[schemars(range(min = 1))]
    pub authored_at_us: u64,
    pub mutation: OfflineContentMutationV1,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OfflineContentMutationV1 {
    #[serde(rename = "put_document")]
    Put {
        document_id: String,
        logical_path: String,
        content_sha256: String,
        org: String,
        base: OfflineContentBaseV1,
    },
    #[serde(rename = "rename_document")]
    Rename {
        document_id: String,
        logical_path: String,
        base: OfflineContentBaseV1,
    },
    #[serde(rename = "tombstone_document")]
    Tombstone {
        document_id: String,
        base: OfflineContentBaseV1,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OfflineContentBaseV1 {
    Absent,
    Revision {
        revision: u64,
        content_sha256: Option<String>,
        predecessor_operation_id: Option<String>,
    },
}

impl From<OfflineContentOperationV1> for OfflineContentOperation {
    fn from(value: OfflineContentOperationV1) -> Self {
        let (server_id, cursor, token) =
            decode_cursor(&value.cursor).expect("validated content cursor");
        Self {
            operation_id: value.operation_id,
            device_id: value.device_id,
            device_sequence: value.device_sequence,
            cursor_server_id: server_id,
            cursor,
            cursor_token: token,
            authored_at_us: value.authored_at_us,
            mutation: value.mutation.into(),
        }
    }
}

impl From<&OfflineContentOperation> for OfflineContentOperationV1 {
    fn from(value: &OfflineContentOperation) -> Self {
        Self {
            operation_id: value.operation_id.clone(),
            device_id: value.device_id.clone(),
            device_sequence: value.device_sequence,
            cursor: encode_cursor(&value.cursor_server_id, value.cursor, &value.cursor_token)
                .unwrap_or_default(),
            authored_at_us: value.authored_at_us,
            mutation: (&value.mutation).into(),
        }
    }
}

impl From<OfflineContentMutationV1> for OfflineContentMutation {
    fn from(value: OfflineContentMutationV1) -> Self {
        match value {
            OfflineContentMutationV1::Put {
                document_id,
                logical_path,
                content_sha256,
                org,
                base,
            } => Self::Put {
                document_id,
                logical_path,
                content_sha256,
                org,
                base: base.into(),
            },
            OfflineContentMutationV1::Rename {
                document_id,
                logical_path,
                base,
            } => Self::Rename {
                document_id,
                logical_path,
                base: base.into(),
            },
            OfflineContentMutationV1::Tombstone { document_id, base } => Self::Tombstone {
                document_id,
                base: base.into(),
            },
        }
    }
}

impl From<&OfflineContentMutation> for OfflineContentMutationV1 {
    fn from(value: &OfflineContentMutation) -> Self {
        match value {
            OfflineContentMutation::Put {
                document_id,
                logical_path,
                content_sha256,
                org,
                base,
            } => Self::Put {
                document_id: document_id.clone(),
                logical_path: logical_path.clone(),
                content_sha256: content_sha256.clone(),
                org: org.clone(),
                base: base.clone().into(),
            },
            OfflineContentMutation::Rename {
                document_id,
                logical_path,
                base,
            } => Self::Rename {
                document_id: document_id.clone(),
                logical_path: logical_path.clone(),
                base: base.clone().into(),
            },
            OfflineContentMutation::Tombstone { document_id, base } => Self::Tombstone {
                document_id: document_id.clone(),
                base: base.clone().into(),
            },
        }
    }
}

impl From<OfflineContentBaseV1> for ContentBase {
    fn from(value: OfflineContentBaseV1) -> Self {
        match value {
            OfflineContentBaseV1::Absent => Self::Absent,
            OfflineContentBaseV1::Revision {
                revision,
                content_sha256,
                predecessor_operation_id,
            } => Self::Revision {
                revision,
                content_sha256,
                predecessor_operation_id,
            },
        }
    }
}

impl From<ContentBase> for OfflineContentBaseV1 {
    fn from(value: ContentBase) -> Self {
        match value {
            ContentBase::Absent => Self::Absent,
            ContentBase::Revision {
                revision,
                content_sha256,
                predecessor_operation_id,
            } => Self::Revision {
                revision,
                content_sha256,
                predecessor_operation_id,
            },
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineContentResultEnvelopeV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    pub result: OfflineContentResultV1,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum OfflineContentResultV1 {
    Accepted {
        #[schemars(length(min = 1, max = 4096))]
        operation_id: String,
        conflict_reason: (),
        authoritative: OfflineContentDocumentV1,
        #[schemars(length(min = 1, max = 512))]
        cursor: String,
    },
    Conflict {
        #[schemars(length(min = 1, max = 4096))]
        operation_id: String,
        conflict_reason: OfflineContentConflictReasonV1,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(required)]
        authoritative: Option<OfflineContentDocumentV1>,
        #[schemars(length(min = 1, max = 512))]
        cursor: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflineContentConflictReasonV1 {
    CursorChanged,
    BaseChanged,
    InvalidContent,
    PathConflict,
}

impl From<OfflineContentConflictReason> for OfflineContentConflictReasonV1 {
    fn from(value: OfflineContentConflictReason) -> Self {
        match value {
            OfflineContentConflictReason::CursorChanged => Self::CursorChanged,
            OfflineContentConflictReason::BaseChanged => Self::BaseChanged,
            OfflineContentConflictReason::InvalidContent => Self::InvalidContent,
            OfflineContentConflictReason::PathConflict => Self::PathConflict,
        }
    }
}

impl From<OfflineContentConflictReasonV1> for OfflineContentConflictReason {
    fn from(value: OfflineContentConflictReasonV1) -> Self {
        match value {
            OfflineContentConflictReasonV1::CursorChanged => Self::CursorChanged,
            OfflineContentConflictReasonV1::BaseChanged => Self::BaseChanged,
            OfflineContentConflictReasonV1::InvalidContent => Self::InvalidContent,
            OfflineContentConflictReasonV1::PathConflict => Self::PathConflict,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineContentDocumentV1 {
    #[schemars(length(min = 1, max = 4096))]
    pub document_id: String,
    #[schemars(length(min = 1, max = 4096))]
    pub logical_path: String,
    #[schemars(range(min = 1))]
    pub revision: u64,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub content_sha256: Option<String>,
    pub active: bool,
}

impl From<&AuthoritativeDocument> for OfflineContentDocumentV1 {
    fn from(value: &AuthoritativeDocument) -> Self {
        Self {
            document_id: value.document_id.clone(),
            logical_path: value.logical_path.clone(),
            revision: value.revision,
            content_sha256: value.content_sha256.clone(),
            active: value.active,
        }
    }
}

impl From<OfflineContentDocumentV1> for AuthoritativeDocument {
    fn from(value: OfflineContentDocumentV1) -> Self {
        Self {
            document_id: value.document_id,
            logical_path: value.logical_path,
            revision: value.revision,
            content_sha256: value.content_sha256,
            active: value.active,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sha2::Digest as _;

    use super::{
        OfflineContentSubmission, OfflineContentWireError, decode_offline_content_capabilities,
        decode_offline_content_result, offline_content_capabilities, offline_content_result_json,
    };
    use crate::application::{
        AuthoritativeDocument, ContentReconciliationStatus, OfflineContentConflictReason,
        OfflineContentResult,
    };

    #[test]
    fn invalid_org_is_a_decodable_operation() {
        let org = "* [invalid";
        let digest = format!("{:x}", sha2::Sha256::digest(org.as_bytes()));
        let submission: OfflineContentSubmission = serde_json::from_value(json!({
            "version": 1,
            "operation": {
                "operation_id": "operation",
                "device_id": "device",
                "device_sequence": 1,
                "cursor": "eyJ2ZXJzaW9uIjoxLCJzZXJ2ZXJfaWQiOiIwMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZiIsInBvc2l0aW9uIjowLCJ0b2tlbiI6IjExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExIn0",
                "authored_at_us": 1,
                "mutation": {
                    "kind": "put_document",
                    "document_id": "document",
                    "logical_path": "note.org",
                    "content_sha256": digest,
                    "org": org,
                    "base": {"kind": "absent"}
                }
            }
        }))
        .unwrap();
        assert!(submission.decode().is_ok());
    }

    #[test]
    fn result_decoder_classifies_unknown_versions() {
        let value = json!({"version":2,"result":{}});
        assert!(matches!(
            decode_offline_content_result(value),
            Err(OfflineContentWireError::Version)
        ));
    }

    #[test]
    fn rejects_changed_content_digest() {
        let submission: OfflineContentSubmission = serde_json::from_value(json!({
            "version": 1,
            "operation": {
                "operation_id": "operation",
                "device_id": "device",
                "device_sequence": 1,
                "cursor": "bad",
                "authored_at_us": 1,
                "mutation": {
                    "kind": "put_document",
                    "document_id": "document",
                    "logical_path": "note.org",
                    "content_sha256": "a".repeat(64),
                    "org": "#+title: Note\n",
                    "base": {"kind": "absent"}
                }
            }
        }))
        .unwrap();
        assert!(matches!(
            submission.decode(),
            Err(OfflineContentWireError::Operation)
        ));
    }

    #[test]
    fn conflict_result_omits_an_authoritative_head_that_exceeds_the_budget() {
        let result = OfflineContentResult {
            operation_id: "operation".into(),
            format_version: 1,
            status: ContentReconciliationStatus::Conflict,
            conflict_reason: Some(OfflineContentConflictReason::BaseChanged),
            authoritative: Some(AuthoritativeDocument {
                document_id: "document".into(),
                logical_path: format!("{}.org", "a".repeat(200_000)),
                revision: 1,
                content_sha256: Some("a".repeat(64)),
                active: true,
            }),
            cursor: 0,
            cursor_server_id: "0".repeat(32),
            cursor_token: "1".repeat(32),
        };

        let wire = offline_content_result_json(&result).unwrap();

        assert!(wire["result"]["authoritative"].is_null());
    }

    #[test]
    fn operation_body_is_bounded_before_it_enters_an_outbox() {
        let org = "x".repeat(super::MAX_OFFLINE_CONTENT_OPERATION_BYTES);
        let digest = format!("{:x}", sha2::Sha256::digest(org.as_bytes()));
        let submission: OfflineContentSubmission = serde_json::from_value(json!({
            "version": 1,
            "operation": {
                "operation_id": "operation",
                "device_id": "device",
                "device_sequence": 1,
                "cursor": "eyJ2ZXJzaW9uIjoxLCJzZXJ2ZXJfaWQiOiIwMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZiIsInBvc2l0aW9uIjowLCJ0b2tlbiI6IjExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExIn0",
                "authored_at_us": 1,
                "mutation": {
                    "kind": "put_document",
                    "document_id": "document",
                    "logical_path": "note.org",
                    "content_sha256": digest,
                    "org": org,
                    "base": {"kind": "absent"}
                }
            }
        }))
        .unwrap();

        assert!(matches!(
            submission.decode(),
            Err(OfflineContentWireError::Operation)
        ));
    }

    #[test]
    fn revision_base_requires_a_captured_content_digest() {
        let org = "#+title: Note\n";
        let digest = format!("{:x}", sha2::Sha256::digest(org.as_bytes()));
        let submission: OfflineContentSubmission = serde_json::from_value(json!({
            "version": 1,
            "operation": {
                "operation_id": "operation",
                "device_id": "device",
                "device_sequence": 1,
                "cursor": "eyJ2ZXJzaW9uIjoxLCJzZXJ2ZXJfaWQiOiIwMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZiIsInBvc2l0aW9uIjowLCJ0b2tlbiI6IjExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExIn0",
                "authored_at_us": 1,
                "mutation": {
                    "kind": "put_document",
                    "document_id": "document",
                    "logical_path": "note.org",
                    "content_sha256": digest,
                    "org": org,
                    "base": {
                        "kind": "revision",
                        "revision": 1,
                        "content_sha256": null,
                        "predecessor_operation_id": null
                    }
                }
            }
        }))
        .unwrap();

        assert!(matches!(
            submission.decode(),
            Err(OfflineContentWireError::Operation)
        ));
    }

    #[test]
    fn lifecycle_mutations_require_a_captured_revision() {
        let cursor = "eyJ2ZXJzaW9uIjoxLCJzZXJ2ZXJfaWQiOiIwMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZiIsInBvc2l0aW9uIjowLCJ0b2tlbiI6IjExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExIn0";
        for mutation in [
            json!({
                "kind": "rename_document",
                "document_id": "document",
                "logical_path": "renamed.org",
                "base": {
                    "kind": "revision",
                    "revision": 1,
                    "content_sha256": "a".repeat(64),
                    "predecessor_operation_id": null
                }
            }),
            json!({
                "kind": "tombstone_document",
                "document_id": "document",
                "base": {
                    "kind": "revision",
                    "revision": 1,
                    "content_sha256": "a".repeat(64),
                    "predecessor_operation_id": "predecessor"
                }
            }),
        ] {
            let submission: OfflineContentSubmission = serde_json::from_value(json!({
                "version": 1,
                "operation": {
                    "operation_id": "operation",
                    "device_id": "device",
                    "device_sequence": 1,
                    "cursor": cursor,
                    "authored_at_us": 1,
                    "mutation": mutation
                }
            }))
            .unwrap();
            assert!(submission.decode().is_ok());
        }

        let absent: OfflineContentSubmission = serde_json::from_value(json!({
            "version": 1,
            "operation": {
                "operation_id": "operation",
                "device_id": "device",
                "device_sequence": 1,
                "cursor": cursor,
                "authored_at_us": 1,
                "mutation": {
                    "kind": "tombstone_document",
                    "document_id": "document",
                    "base": {"kind": "absent"}
                }
            }
        }))
        .unwrap();
        assert!(matches!(
            absent.decode(),
            Err(OfflineContentWireError::Operation)
        ));
    }

    #[test]
    fn lifecycle_capability_is_explicit_and_server_bound() {
        let server_id = "a".repeat(32);
        let value = offline_content_capabilities(&server_id).unwrap();
        assert_eq!(
            decode_offline_content_capabilities(value)
                .unwrap()
                .server_id,
            server_id
        );
        for invalid in [
            json!({"version":1,"server_id":"bad","document_lifecycle":1}),
            json!({"version":1,"server_id":"a".repeat(32),"document_lifecycle":0}),
        ] {
            let invalid = serde_json::from_value(invalid).unwrap();
            assert!(decode_offline_content_capabilities(invalid).is_err());
        }
        assert!(
            serde_json::from_value::<crate::protocol::http_v1::ContentCapabilitiesV1>(json!({
                "version":1,
                "server_id":"a".repeat(32),
                "document_lifecycle":1,
                "extra":true
            }))
            .is_err()
        );
    }
}
