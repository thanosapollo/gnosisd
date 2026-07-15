use serde::{Deserialize, Serialize};
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
    protocol::replica::{decode_cursor, encode_cursor},
};

pub(crate) const OFFLINE_CONTENT_VERSION: u32 = 1;
const OFFLINE_CONTENT_CAPABILITIES_VERSION: u32 = 1;
const MAX_OFFLINE_CONTENT_OPERATION_BYTES: usize = 1024 * 1024;
const MAX_OFFLINE_CONTENT_RESULT_BYTES: usize = 128 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OfflineContentSubmission {
    version: u32,
    operation: Value,
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

pub(crate) fn offline_content_capabilities_json(
    server_id: &str,
) -> Result<Value, OfflineContentWireError> {
    if !is_hex_identity(server_id) {
        return Err(OfflineContentWireError::Operation);
    }
    serde_json::to_value(ContentCapabilitiesV1 {
        version: OFFLINE_CONTENT_CAPABILITIES_VERSION,
        server_id: server_id.to_owned(),
        document_lifecycle: 1,
    })
    .map_err(invalid_operation)
}

pub(crate) fn decode_offline_content_capabilities(
    value: Value,
) -> Result<DecodedOfflineContentCapabilities, OfflineContentWireError> {
    require_size(&value, MAX_OFFLINE_CONTENT_RESULT_BYTES)?;
    let capabilities: ContentCapabilitiesV1 =
        serde_json::from_value(value).map_err(invalid_operation)?;
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

pub(crate) fn offline_content_result_json(
    result: &OfflineContentResult,
) -> Result<Value, OfflineContentWireError> {
    let cursor = encode_cursor(
        &result.cursor_server_id,
        result.cursor,
        &result.cursor_token,
    )
    .map_err(|_| OfflineContentWireError::Operation)?;
    let envelope = content_result_envelope(result, cursor.clone(), result.authoritative.clone());
    match bounded_value(&envelope, MAX_OFFLINE_CONTENT_RESULT_BYTES) {
        Ok(value) => Ok(value),
        Err(_) if result.status == ContentReconciliationStatus::Conflict => bounded_value(
            &content_result_envelope(result, cursor, None),
            MAX_OFFLINE_CONTENT_RESULT_BYTES,
        ),
        Err(error) => Err(error),
    }
}

fn content_result_envelope(
    result: &OfflineContentResult,
    cursor: String,
    authoritative: Option<AuthoritativeDocument>,
) -> ContentResultEnvelopeV1 {
    ContentResultEnvelopeV1 {
        version: OFFLINE_CONTENT_VERSION,
        result: ContentResultV1 {
            operation_id: result.operation_id.clone(),
            status: result.status.as_str().into(),
            conflict_reason: result.conflict_reason.map(|reason| reason.as_str().into()),
            authoritative,
            cursor,
        },
    }
}

pub(crate) fn decode_offline_content_result(
    value: Value,
) -> Result<DecodedOfflineContentResult, OfflineContentWireError> {
    require_size(&value, MAX_OFFLINE_CONTENT_RESULT_BYTES)?;
    let envelope: ContentResultEnvelopeV1 =
        serde_json::from_value(value).map_err(invalid_operation)?;
    if envelope.version != OFFLINE_CONTENT_VERSION {
        return Err(OfflineContentWireError::Version);
    }
    decode_result(envelope.result)
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
        ContentMutationV1::Put {
            document_id,
            logical_path,
            content_sha256,
            org,
            base,
        } => validate_document_put(document_id, logical_path, content_sha256, org, base),
        ContentMutationV1::Rename {
            document_id,
            logical_path,
            base,
        } => validate_document_rename(document_id, logical_path, base),
        ContentMutationV1::Tombstone { document_id, base } => {
            validate_document_tombstone(document_id, base)
        }
    }
}

fn validate_document_put(
    document_id: &str,
    logical_path: &str,
    content_sha256: &str,
    org: &str,
    base: &ContentBaseV1,
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
    base: &ContentBaseV1,
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
    base: &ContentBaseV1,
) -> Result<(), OfflineContentWireError> {
    validate_document_identity(document_id)?;
    require_revision_base(base)
}

fn validate_document_identity(document_id: &str) -> Result<(), OfflineContentWireError> {
    DocumentId::new(document_id.to_owned())
        .map(|_| ())
        .map_err(invalid_operation)
}

fn require_revision_base(base: &ContentBaseV1) -> Result<(), OfflineContentWireError> {
    if matches!(base, ContentBaseV1::Revision { .. }) && valid_content_base(base) {
        Ok(())
    } else {
        Err(OfflineContentWireError::Operation)
    }
}

fn valid_content_base(base: &ContentBaseV1) -> bool {
    match base {
        ContentBaseV1::Absent => true,
        ContentBaseV1::Revision {
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
    value: ContentResultV1,
) -> Result<DecodedOfflineContentResult, OfflineContentWireError> {
    require_identity(&value.operation_id)?;
    validate_cursor(&value.cursor)?;
    let status = parse_status(&value.status)?;
    let conflict_reason = value
        .conflict_reason
        .as_deref()
        .map(parse_reason)
        .transpose()?;
    if (status == ContentReconciliationStatus::Conflict) != conflict_reason.is_some() {
        return Err(OfflineContentWireError::Operation);
    }
    let authoritative = value.authoritative;
    if let Some(document) = &authoritative {
        validate_authoritative(document)?;
    }
    if status == ContentReconciliationStatus::Accepted && authoritative.is_none() {
        return Err(OfflineContentWireError::Operation);
    }
    Ok(DecodedOfflineContentResult {
        operation_id: value.operation_id,
        status,
        conflict_reason,
        authoritative,
        cursor: value.cursor,
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

fn bounded_value(value: &impl Serialize, maximum: usize) -> Result<Value, OfflineContentWireError> {
    let bytes = serde_json::to_vec(value).map_err(invalid_operation)?;
    if bytes.len() > maximum {
        return Err(OfflineContentWireError::Operation);
    }
    serde_json::from_slice(&bytes).map_err(invalid_operation)
}

fn require_size(value: &Value, maximum: usize) -> Result<(), OfflineContentWireError> {
    let bytes = serde_json::to_vec(value).map_err(invalid_operation)?;
    if bytes.len() > maximum {
        Err(OfflineContentWireError::Operation)
    } else {
        Ok(())
    }
}

fn parse_status(value: &str) -> Result<ContentReconciliationStatus, OfflineContentWireError> {
    match value {
        "accepted" => Ok(ContentReconciliationStatus::Accepted),
        "conflict" => Ok(ContentReconciliationStatus::Conflict),
        _ => Err(OfflineContentWireError::Operation),
    }
}

fn parse_reason(value: &str) -> Result<OfflineContentConflictReason, OfflineContentWireError> {
    match value {
        "cursor_changed" => Ok(OfflineContentConflictReason::CursorChanged),
        "base_changed" => Ok(OfflineContentConflictReason::BaseChanged),
        "invalid_content" => Ok(OfflineContentConflictReason::InvalidContent),
        "path_conflict" => Ok(OfflineContentConflictReason::PathConflict),
        _ => Err(OfflineContentWireError::Operation),
    }
}

fn require_identity(value: &str) -> Result<(), OfflineContentWireError> {
    if value.is_empty() || value.len() > 4096 {
        Err(OfflineContentWireError::Operation)
    } else {
        Ok(())
    }
}

fn require_digest(value: &str) -> Result<(), OfflineContentWireError> {
    if value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
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

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContentCapabilitiesV1 {
    version: u32,
    server_id: String,
    document_lifecycle: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct OfflineContentOperationV1 {
    operation_id: String,
    device_id: String,
    device_sequence: u64,
    cursor: String,
    authored_at_us: u64,
    mutation: ContentMutationV1,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ContentMutationV1 {
    #[serde(rename = "put_document")]
    Put {
        document_id: String,
        logical_path: String,
        content_sha256: String,
        org: String,
        base: ContentBaseV1,
    },
    #[serde(rename = "rename_document")]
    Rename {
        document_id: String,
        logical_path: String,
        base: ContentBaseV1,
    },
    #[serde(rename = "tombstone_document")]
    Tombstone {
        document_id: String,
        base: ContentBaseV1,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ContentBaseV1 {
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

impl From<ContentMutationV1> for OfflineContentMutation {
    fn from(value: ContentMutationV1) -> Self {
        match value {
            ContentMutationV1::Put {
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
            ContentMutationV1::Rename {
                document_id,
                logical_path,
                base,
            } => Self::Rename {
                document_id,
                logical_path,
                base: base.into(),
            },
            ContentMutationV1::Tombstone { document_id, base } => Self::Tombstone {
                document_id,
                base: base.into(),
            },
        }
    }
}

impl From<&OfflineContentMutation> for ContentMutationV1 {
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

impl From<ContentBaseV1> for ContentBase {
    fn from(value: ContentBaseV1) -> Self {
        match value {
            ContentBaseV1::Absent => Self::Absent,
            ContentBaseV1::Revision {
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

impl From<ContentBase> for ContentBaseV1 {
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

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContentResultEnvelopeV1 {
    version: u32,
    result: ContentResultV1,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ContentResultV1 {
    operation_id: String,
    status: String,
    conflict_reason: Option<String>,
    authoritative: Option<AuthoritativeDocument>,
    cursor: String,
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sha2::Digest as _;

    use super::{
        OfflineContentSubmission, OfflineContentWireError, decode_offline_content_capabilities,
        offline_content_capabilities_json, offline_content_result_json,
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
        let value = offline_content_capabilities_json(&server_id).unwrap();
        assert_eq!(
            decode_offline_content_capabilities(value)
                .unwrap()
                .server_id,
            server_id
        );
        for invalid in [
            json!({"version":1,"server_id":"bad","document_lifecycle":1}),
            json!({"version":1,"server_id":"a".repeat(32),"document_lifecycle":0}),
            json!({"version":1,"server_id":"a".repeat(32),"document_lifecycle":1,"extra":true}),
        ] {
            assert!(decode_offline_content_capabilities(invalid).is_err());
        }
    }
}
