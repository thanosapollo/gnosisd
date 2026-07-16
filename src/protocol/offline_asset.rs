use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::{
    application::{
        AssetReconciliationStatus, AuthoritativeAsset, MAX_OFFLINE_ASSET_BYTES,
        OfflineAssetConflictReason, OfflineAssetOperation, canonical_asset_path, prepare_asset,
        validate_asset_path,
    },
    is_canonical_sha256,
    protocol::{
        http_v1::AssetCapabilitiesV1,
        replica::{decode_cursor, encode_cursor},
    },
};

pub(crate) const OFFLINE_ASSET_VERSION: u32 = 1;
pub(crate) const MAX_OFFLINE_ASSET_OPERATION_BYTES: usize = 64 * 1024;
pub(crate) const MAX_OFFLINE_ASSET_RESULT_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OfflineAssetSubmission {
    version: u32,
    operation: Value,
}

pub(crate) struct DecodedOfflineAsset {
    pub(crate) version: u32,
    pub(crate) operation: OfflineAssetOperation,
    pub(crate) canonical_payload: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedOfflineAssetCapabilities {
    pub(crate) server_id: String,
    pub(crate) max_blob_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DecodedOfflineAssetResult {
    pub(crate) operation_id: String,
    pub(crate) status: AssetReconciliationStatus,
    pub(crate) conflict_reason: Option<OfflineAssetConflictReason>,
    pub(crate) authoritative: Option<AuthoritativeAsset>,
    pub(crate) cursor: String,
}

#[derive(Debug, Error)]
pub(crate) enum OfflineAssetWireError {
    #[error("offline asset protocol version is unsupported")]
    Version,
    #[error("offline asset operation is invalid")]
    Operation,
}

impl OfflineAssetSubmission {
    pub(crate) fn decode(self) -> Result<DecodedOfflineAsset, OfflineAssetWireError> {
        if self.version != OFFLINE_ASSET_VERSION {
            return Err(OfflineAssetWireError::Version);
        }
        let wire: AssetOperationV1 =
            serde_json::from_value(self.operation).map_err(invalid_operation)?;
        validate_operation(&wire)?;
        let canonical_payload = encode_operation(&wire)?;
        Ok(DecodedOfflineAsset {
            version: self.version,
            operation: wire.into(),
            canonical_payload,
        })
    }
}

pub(crate) fn canonical_asset_operation(
    operation: &OfflineAssetOperation,
) -> Result<String, OfflineAssetWireError> {
    let wire = AssetOperationV1::from(operation);
    validate_operation(&wire)?;
    encode_operation(&wire)
}

pub(crate) fn offline_asset_capabilities(
    server_id: &str,
) -> Result<AssetCapabilitiesV1, OfflineAssetWireError> {
    if !is_hex_identity(server_id) {
        return Err(OfflineAssetWireError::Operation);
    }
    Ok(AssetCapabilitiesV1 {
        version: OFFLINE_ASSET_VERSION,
        server_id: server_id.to_owned(),
        put_asset: 1,
        max_blob_bytes: MAX_OFFLINE_ASSET_BYTES,
    })
}

pub(crate) fn decode_offline_asset_capabilities(
    value: AssetCapabilitiesV1,
) -> Result<DecodedOfflineAssetCapabilities, OfflineAssetWireError> {
    if value.version != OFFLINE_ASSET_VERSION
        || value.put_asset != 1
        || value.max_blob_bytes != MAX_OFFLINE_ASSET_BYTES
        || !is_hex_identity(&value.server_id)
    {
        return Err(OfflineAssetWireError::Operation);
    }
    Ok(DecodedOfflineAssetCapabilities {
        server_id: value.server_id,
        max_blob_bytes: value.max_blob_bytes,
    })
}

pub(crate) fn offline_asset_result_json(
    result: &crate::application::OfflineAssetResult,
) -> Result<Value, OfflineAssetWireError> {
    let cursor = encode_cursor(
        &result.cursor_server_id,
        result.cursor,
        &result.cursor_token,
    )
    .map_err(invalid_operation)?;
    bounded_value(
        &AssetResultEnvelopeV1 {
            version: result.format_version,
            result: AssetResultV1 {
                operation_id: result.operation_id.clone(),
                status: result.status.as_str().to_owned(),
                conflict_reason: result
                    .conflict_reason
                    .map(|value| value.as_str().to_owned()),
                authoritative: result
                    .authoritative
                    .as_ref()
                    .map(AuthoritativeAssetV1::from),
                cursor,
            },
        },
        MAX_OFFLINE_ASSET_RESULT_BYTES,
    )
}

pub(crate) fn decode_offline_asset_result(
    value: Value,
) -> Result<DecodedOfflineAssetResult, OfflineAssetWireError> {
    require_size(&value, MAX_OFFLINE_ASSET_RESULT_BYTES)?;
    let envelope: AssetResultEnvelopeV1 =
        serde_json::from_value(value).map_err(invalid_operation)?;
    if envelope.version != OFFLINE_ASSET_VERSION {
        return Err(OfflineAssetWireError::Version);
    }
    decode_result(envelope.result)
}

fn validate_operation(value: &AssetOperationV1) -> Result<(), OfflineAssetWireError> {
    require_identity(&value.operation_id)?;
    require_identity(&value.device_id)?;
    if value.device_sequence == 0
        || value.device_sequence > i64::MAX as u64
        || value.authored_at_us == 0
        || value.authored_at_us > i64::MAX as u64
        || value.size_bytes == 0
        || value.size_bytes > MAX_OFFLINE_ASSET_BYTES
    {
        return Err(OfflineAssetWireError::Operation);
    }
    validate_cursor(&value.cursor)?;
    require_digest(&value.content_sha256)?;
    require_media_type(&value.media_type)?;
    prepare_asset(
        value.logical_path.clone(),
        value.original_name.clone(),
        None,
    )
    .map_err(invalid_operation)?;
    if canonical_asset_path(&value.content_sha256, &value.original_name)
        .map_err(invalid_operation)?
        != value.logical_path
    {
        return Err(OfflineAssetWireError::Operation);
    }
    Ok(())
}

fn decode_result(value: AssetResultV1) -> Result<DecodedOfflineAssetResult, OfflineAssetWireError> {
    require_identity(&value.operation_id)?;
    validate_cursor(&value.cursor)?;
    let status = match value.status.as_str() {
        "accepted" => AssetReconciliationStatus::Accepted,
        "conflict" => AssetReconciliationStatus::Conflict,
        _ => return Err(OfflineAssetWireError::Operation),
    };
    let conflict_reason = value
        .conflict_reason
        .as_deref()
        .map(parse_reason)
        .transpose()?;
    if (status == AssetReconciliationStatus::Conflict) != conflict_reason.is_some() {
        return Err(OfflineAssetWireError::Operation);
    }
    if let Some(asset) = &value.authoritative {
        validate_authoritative(asset)?;
    }
    if status == AssetReconciliationStatus::Accepted && value.authoritative.is_none() {
        return Err(OfflineAssetWireError::Operation);
    }
    Ok(DecodedOfflineAssetResult {
        operation_id: value.operation_id,
        status,
        conflict_reason,
        authoritative: value.authoritative.map(AuthoritativeAsset::from),
        cursor: value.cursor,
    })
}

fn validate_authoritative(value: &AuthoritativeAssetV1) -> Result<(), OfflineAssetWireError> {
    require_digest(&value.content_sha256)?;
    require_media_type(&value.media_type)?;
    validate_asset_path(&value.logical_path).map_err(invalid_operation)?;
    if value.revision == 0 || value.revision > i64::MAX as u64 || !value.active {
        return Err(OfflineAssetWireError::Operation);
    }
    let prefix = format!("assets/{}/", value.content_sha256);
    if !value.logical_path.starts_with(&prefix)
        || value.size_bytes == 0
        || value.size_bytes > MAX_OFFLINE_ASSET_BYTES
    {
        return Err(OfflineAssetWireError::Operation);
    }
    Ok(())
}

fn encode_operation(value: &AssetOperationV1) -> Result<String, OfflineAssetWireError> {
    let encoded = serde_json::to_string(&VersionedOperationV1 {
        version: OFFLINE_ASSET_VERSION,
        operation: value,
    })
    .map_err(invalid_operation)?;
    if encoded.len() > MAX_OFFLINE_ASSET_OPERATION_BYTES {
        Err(OfflineAssetWireError::Operation)
    } else {
        Ok(encoded)
    }
}

fn validate_cursor(value: &str) -> Result<(), OfflineAssetWireError> {
    let (server_id, position, token) = decode_cursor(value).map_err(invalid_operation)?;
    if position > i64::MAX as u64 || !is_hex_identity(&server_id) || !is_hex_identity(&token) {
        Err(OfflineAssetWireError::Operation)
    } else {
        Ok(())
    }
}

fn parse_reason(value: &str) -> Result<OfflineAssetConflictReason, OfflineAssetWireError> {
    match value {
        "cursor_changed" => Ok(OfflineAssetConflictReason::CursorChanged),
        "base_changed" => Ok(OfflineAssetConflictReason::BaseChanged),
        "invalid_content" => Ok(OfflineAssetConflictReason::InvalidContent),
        "path_conflict" => Ok(OfflineAssetConflictReason::PathConflict),
        _ => Err(OfflineAssetWireError::Operation),
    }
}

fn require_identity(value: &str) -> Result<(), OfflineAssetWireError> {
    if value.is_empty() || value.len() > 4096 {
        Err(OfflineAssetWireError::Operation)
    } else {
        Ok(())
    }
}

fn require_digest(value: &str) -> Result<(), OfflineAssetWireError> {
    if is_canonical_sha256(value) {
        Ok(())
    } else {
        Err(OfflineAssetWireError::Operation)
    }
}

fn require_media_type(value: &str) -> Result<(), OfflineAssetWireError> {
    if !value.is_empty()
        && value.len() <= 255
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        && value.trim() == value
    {
        Ok(())
    } else {
        Err(OfflineAssetWireError::Operation)
    }
}

fn is_hex_identity(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn bounded_value(value: &impl Serialize, maximum: usize) -> Result<Value, OfflineAssetWireError> {
    let bytes = serde_json::to_vec(value).map_err(invalid_operation)?;
    if bytes.len() > maximum {
        return Err(OfflineAssetWireError::Operation);
    }
    serde_json::from_slice(&bytes).map_err(invalid_operation)
}

fn require_size(value: &Value, maximum: usize) -> Result<(), OfflineAssetWireError> {
    let bytes = serde_json::to_vec(value).map_err(invalid_operation)?;
    if bytes.len() > maximum {
        Err(OfflineAssetWireError::Operation)
    } else {
        Ok(())
    }
}

fn invalid_operation<T>(_error: T) -> OfflineAssetWireError {
    OfflineAssetWireError::Operation
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct AssetOperationV1 {
    operation_id: String,
    device_id: String,
    device_sequence: u64,
    cursor: String,
    authored_at_us: u64,
    kind: AssetMutationKindV1,
    logical_path: String,
    original_name: String,
    content_sha256: String,
    size_bytes: u64,
    media_type: String,
    base: AssetBaseV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AssetMutationKindV1 {
    PutAsset,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum AssetBaseV1 {
    Absent,
}

impl From<AssetOperationV1> for OfflineAssetOperation {
    fn from(value: AssetOperationV1) -> Self {
        let (cursor_server_id, cursor, cursor_token) =
            decode_cursor(&value.cursor).expect("validated asset cursor");
        Self {
            operation_id: value.operation_id,
            device_id: value.device_id,
            device_sequence: value.device_sequence,
            cursor_server_id,
            cursor,
            cursor_token,
            authored_at_us: value.authored_at_us,
            logical_path: value.logical_path,
            original_name: value.original_name,
            content_sha256: value.content_sha256,
            size_bytes: value.size_bytes,
            media_type: value.media_type,
        }
    }
}

impl From<&OfflineAssetOperation> for AssetOperationV1 {
    fn from(value: &OfflineAssetOperation) -> Self {
        Self {
            operation_id: value.operation_id.clone(),
            device_id: value.device_id.clone(),
            device_sequence: value.device_sequence,
            cursor: encode_cursor(&value.cursor_server_id, value.cursor, &value.cursor_token)
                .unwrap_or_default(),
            authored_at_us: value.authored_at_us,
            kind: AssetMutationKindV1::PutAsset,
            logical_path: value.logical_path.clone(),
            original_name: value.original_name.clone(),
            content_sha256: value.content_sha256.clone(),
            size_bytes: value.size_bytes,
            media_type: value.media_type.clone(),
            base: AssetBaseV1::Absent,
        }
    }
}

#[derive(Serialize)]
struct VersionedOperationV1<'a> {
    version: u32,
    operation: &'a AssetOperationV1,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssetResultEnvelopeV1 {
    version: u32,
    result: AssetResultV1,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AssetResultV1 {
    operation_id: String,
    status: String,
    conflict_reason: Option<String>,
    authoritative: Option<AuthoritativeAssetV1>,
    cursor: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuthoritativeAssetV1 {
    logical_path: String,
    revision: u64,
    content_sha256: String,
    size_bytes: u64,
    media_type: String,
    active: bool,
}

impl From<&AuthoritativeAsset> for AuthoritativeAssetV1 {
    fn from(value: &AuthoritativeAsset) -> Self {
        Self {
            logical_path: value.logical_path.clone(),
            revision: value.revision,
            content_sha256: value.content_sha256.clone(),
            size_bytes: value.size_bytes,
            media_type: value.media_type.clone(),
            active: value.active,
        }
    }
}

impl From<AuthoritativeAssetV1> for AuthoritativeAsset {
    fn from(value: AuthoritativeAssetV1) -> Self {
        Self {
            logical_path: value.logical_path,
            revision: value.revision,
            content_sha256: value.content_sha256,
            size_bytes: value.size_bytes,
            media_type: value.media_type,
            active: value.active,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        OfflineAssetSubmission, decode_offline_asset_capabilities, decode_offline_asset_result,
        offline_asset_capabilities,
    };

    #[test]
    fn capability_is_strict_and_server_bound() {
        let server = "a".repeat(32);
        let value = offline_asset_capabilities(&server).unwrap();
        let decoded = decode_offline_asset_capabilities(value).unwrap();
        assert_eq!(decoded.server_id, server);
        assert_eq!(decoded.max_blob_bytes, super::MAX_OFFLINE_ASSET_BYTES);
        assert!(
            serde_json::from_value::<crate::protocol::http_v1::AssetCapabilitiesV1>(json!({
                "version": 1,
                "server_id": "a".repeat(32),
                "put_asset": 1,
                "max_blob_bytes": super::MAX_OFFLINE_ASSET_BYTES,
                "future": true
            }))
            .is_err()
        );
    }

    #[test]
    fn put_requires_a_canonical_digest_path() {
        let submission: OfflineAssetSubmission = serde_json::from_value(json!({
            "version": 1,
            "operation": {
                "operation_id": "operation",
                "device_id": "device",
                "device_sequence": 1,
                "cursor": "eyJ2ZXJzaW9uIjoxLCJzZXJ2ZXJfaWQiOiIwMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZiIsInBvc2l0aW9uIjowLCJ0b2tlbiI6IjExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExIn0",
                "authored_at_us": 1,
                "kind": "put_asset",
                "logical_path": format!("assets/{}/image.png", "a".repeat(64)),
                "original_name": "image.png",
                "content_sha256": "b".repeat(64),
                "size_bytes": 4,
                "media_type": "image/png",
                "base": {"kind": "absent"}
            }
        }))
        .unwrap();
        assert!(submission.decode().is_err());
    }

    #[test]
    fn put_rejects_blobs_larger_than_the_advertised_limit() {
        let submission: OfflineAssetSubmission = serde_json::from_value(json!({
            "version": 1,
            "operation": {
                "operation_id": "operation",
                "device_id": "device",
                "device_sequence": 1,
                "cursor": "eyJ2ZXJzaW9uIjoxLCJzZXJ2ZXJfaWQiOiIwMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZiIsInBvc2l0aW9uIjowLCJ0b2tlbiI6IjExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExIn0",
                "authored_at_us": 1,
                "kind": "put_asset",
                "logical_path": format!("assets/{}/image.png", "a".repeat(64)),
                "original_name": "image.png",
                "content_sha256": "a".repeat(64),
                "size_bytes": super::MAX_OFFLINE_ASSET_BYTES + 1,
                "media_type": "image/png",
                "base": {"kind": "absent"}
            }
        }))
        .unwrap();
        assert!(submission.decode().is_err());
    }

    #[test]
    fn result_authority_rejects_unknown_fields() {
        let result = json!({
            "version": 1,
            "result": {
                "operation_id": "operation",
                "status": "accepted",
                "conflict_reason": null,
                "authoritative": {
                    "logical_path": format!("assets/{}/image.png", "a".repeat(64)),
                    "revision": 1,
                    "content_sha256": "a".repeat(64),
                    "size_bytes": 4,
                    "media_type": "image/png",
                    "active": true,
                    "future": true
                },
                "cursor": "eyJ2ZXJzaW9uIjoxLCJzZXJ2ZXJfaWQiOiIwMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZiIsInBvc2l0aW9uIjowLCJ0b2tlbiI6IjExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExIn0"
            }
        });
        assert!(decode_offline_asset_result(result).is_err());
    }
}
