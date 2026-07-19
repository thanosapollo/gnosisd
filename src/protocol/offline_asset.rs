use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
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
const MAX_ASSET_NAME_CHARS: usize = 255;
const MAX_ASSET_PATH_CHARS: usize = 327;

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
    if version == OFFLINE_ASSET_VERSION {
        Ok(version)
    } else {
        Err(serde::de::Error::custom("protocol version is unsupported"))
    }
}

fn deserialize_true<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    let value = bool::deserialize(deserializer)?;
    value
        .then_some(value)
        .ok_or_else(|| serde::de::Error::custom("active must be true"))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OfflineAssetSubmission {
    version: u32,
    operation: Value,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineAssetOperationEnvelopeV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    pub operation: OfflineAssetOperationV1,
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
        let wire: OfflineAssetOperationV1 =
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
    let wire = OfflineAssetOperationV1::from(operation);
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

#[cfg(test)]
pub(crate) fn offline_asset_result_json(
    result: &crate::application::OfflineAssetResult,
) -> Result<Value, OfflineAssetWireError> {
    serde_json::to_value(offline_asset_result_response(result)?).map_err(invalid_operation)
}

pub(crate) fn offline_asset_result_response(
    result: &crate::application::OfflineAssetResult,
) -> Result<OfflineAssetResultEnvelopeV1, OfflineAssetWireError> {
    if result.format_version != OFFLINE_ASSET_VERSION {
        return Err(OfflineAssetWireError::Version);
    }
    require_identity(&result.operation_id)?;
    let cursor = encode_cursor(
        &result.cursor_server_id,
        result.cursor,
        &result.cursor_token,
    )
    .map_err(invalid_operation)?;
    let envelope = OfflineAssetResultEnvelopeV1 {
        version: result.format_version,
        result: asset_result_wire(result, cursor)?,
    };
    require_serialized_size(&envelope, MAX_OFFLINE_ASSET_RESULT_BYTES)?;
    Ok(envelope)
}

fn asset_result_wire(
    result: &crate::application::OfflineAssetResult,
    cursor: String,
) -> Result<OfflineAssetResultV1, OfflineAssetWireError> {
    Ok(match result.status {
        AssetReconciliationStatus::Accepted => OfflineAssetResultV1::Accepted {
            operation_id: result.operation_id.clone(),
            conflict_reason: require_absent(result.conflict_reason)?,
            authoritative: asset_projection(
                result
                    .authoritative
                    .as_ref()
                    .ok_or(OfflineAssetWireError::Operation)?,
            )?,
            cursor,
        },
        AssetReconciliationStatus::Conflict => OfflineAssetResultV1::Conflict {
            operation_id: result.operation_id.clone(),
            conflict_reason: result
                .conflict_reason
                .ok_or(OfflineAssetWireError::Operation)?
                .into(),
            authoritative: result
                .authoritative
                .as_ref()
                .map(asset_projection)
                .transpose()?,
            cursor,
        },
    })
}

fn asset_projection(
    value: &AuthoritativeAsset,
) -> Result<OfflineAssetProjectionV1, OfflineAssetWireError> {
    let projection = OfflineAssetProjectionV1::from(value);
    validate_authoritative(&projection)?;
    Ok(projection)
}

pub(crate) fn decode_offline_asset_result(
    value: Value,
) -> Result<DecodedOfflineAssetResult, OfflineAssetWireError> {
    require_size(&value, MAX_OFFLINE_ASSET_RESULT_BYTES)?;
    require_response_version(&value)?;
    let envelope: OfflineAssetResultEnvelopeV1 =
        serde_json::from_value(value).map_err(invalid_operation)?;
    decode_result(envelope.result)
}

fn require_response_version(value: &Value) -> Result<(), OfflineAssetWireError> {
    match value.get("version").and_then(Value::as_u64) {
        Some(version) if version == u64::from(OFFLINE_ASSET_VERSION) => Ok(()),
        Some(_) => Err(OfflineAssetWireError::Version),
        None => Err(OfflineAssetWireError::Operation),
    }
}

fn validate_operation(value: &OfflineAssetOperationV1) -> Result<(), OfflineAssetWireError> {
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
    validate_path_lengths(&value.logical_path, &value.original_name)?;
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

fn decode_result(
    value: OfflineAssetResultV1,
) -> Result<DecodedOfflineAssetResult, OfflineAssetWireError> {
    let (operation_id, status, conflict_reason, authoritative, cursor) = match value {
        OfflineAssetResultV1::Accepted {
            operation_id,
            authoritative,
            cursor,
            ..
        } => (
            operation_id,
            AssetReconciliationStatus::Accepted,
            None,
            Some(authoritative),
            cursor,
        ),
        OfflineAssetResultV1::Conflict {
            operation_id,
            conflict_reason,
            authoritative,
            cursor,
        } => (
            operation_id,
            AssetReconciliationStatus::Conflict,
            Some(conflict_reason.into()),
            authoritative,
            cursor,
        ),
    };
    require_identity(&operation_id)?;
    validate_cursor(&cursor)?;
    if let Some(asset) = &authoritative {
        validate_authoritative(asset)?;
    }
    Ok(DecodedOfflineAssetResult {
        operation_id,
        status,
        conflict_reason,
        authoritative: authoritative.map(AuthoritativeAsset::from),
        cursor,
    })
}

fn validate_authoritative(value: &OfflineAssetProjectionV1) -> Result<(), OfflineAssetWireError> {
    require_digest(&value.content_sha256)?;
    require_media_type(&value.media_type)?;
    let name = value
        .logical_path
        .rsplit_once('/')
        .map(|(_, name)| name)
        .ok_or(OfflineAssetWireError::Operation)?;
    validate_path_lengths(&value.logical_path, name)?;
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

fn encode_operation(value: &OfflineAssetOperationV1) -> Result<String, OfflineAssetWireError> {
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

fn validate_path_lengths(path: &str, name: &str) -> Result<(), OfflineAssetWireError> {
    if path.chars().count() <= MAX_ASSET_PATH_CHARS && name.chars().count() <= MAX_ASSET_NAME_CHARS
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

fn require_serialized_size(
    value: &impl Serialize,
    maximum: usize,
) -> Result<(), OfflineAssetWireError> {
    let bytes = serde_json::to_vec(value).map_err(invalid_operation)?;
    if bytes.len() > maximum {
        Err(OfflineAssetWireError::Operation)
    } else {
        Ok(())
    }
}

fn require_absent<T>(value: Option<T>) -> Result<(), OfflineAssetWireError> {
    value
        .is_none()
        .then_some(())
        .ok_or(OfflineAssetWireError::Operation)
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineAssetOperationV1 {
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
    pub kind: OfflineAssetMutationKindV1,
    #[schemars(length(min = 1, max = 327))]
    pub logical_path: String,
    #[schemars(length(min = 1, max = 255))]
    pub original_name: String,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub content_sha256: String,
    #[schemars(range(min = 1, max = 1048576))]
    pub size_bytes: u64,
    #[schemars(length(min = 1, max = 255))]
    pub media_type: String,
    pub base: OfflineAssetBaseV1,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflineAssetMutationKindV1 {
    PutAsset,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum OfflineAssetBaseV1 {
    Absent,
}

impl From<OfflineAssetOperationV1> for OfflineAssetOperation {
    fn from(value: OfflineAssetOperationV1) -> Self {
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

impl From<&OfflineAssetOperation> for OfflineAssetOperationV1 {
    fn from(value: &OfflineAssetOperation) -> Self {
        Self {
            operation_id: value.operation_id.clone(),
            device_id: value.device_id.clone(),
            device_sequence: value.device_sequence,
            cursor: encode_cursor(&value.cursor_server_id, value.cursor, &value.cursor_token)
                .unwrap_or_default(),
            authored_at_us: value.authored_at_us,
            kind: OfflineAssetMutationKindV1::PutAsset,
            logical_path: value.logical_path.clone(),
            original_name: value.original_name.clone(),
            content_sha256: value.content_sha256.clone(),
            size_bytes: value.size_bytes,
            media_type: value.media_type.clone(),
            base: OfflineAssetBaseV1::Absent,
        }
    }
}

#[derive(Serialize)]
struct VersionedOperationV1<'a> {
    version: u32,
    operation: &'a OfflineAssetOperationV1,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineAssetResultEnvelopeV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    pub result: OfflineAssetResultV1,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum OfflineAssetResultV1 {
    Accepted {
        #[schemars(length(min = 1, max = 4096))]
        operation_id: String,
        conflict_reason: (),
        authoritative: OfflineAssetProjectionV1,
        #[schemars(length(min = 1, max = 512))]
        cursor: String,
    },
    Conflict {
        #[schemars(length(min = 1, max = 4096))]
        operation_id: String,
        conflict_reason: OfflineAssetConflictReasonV1,
        #[serde(deserialize_with = "deserialize_required_option")]
        #[schemars(
            with = "crate::protocol::RequiredNullable<OfflineAssetProjectionV1>",
            required
        )]
        authoritative: Option<OfflineAssetProjectionV1>,
        #[schemars(length(min = 1, max = 512))]
        cursor: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflineAssetConflictReasonV1 {
    CursorChanged,
    BaseChanged,
    InvalidContent,
    PathConflict,
}

impl From<OfflineAssetConflictReason> for OfflineAssetConflictReasonV1 {
    fn from(value: OfflineAssetConflictReason) -> Self {
        match value {
            OfflineAssetConflictReason::CursorChanged => Self::CursorChanged,
            OfflineAssetConflictReason::BaseChanged => Self::BaseChanged,
            OfflineAssetConflictReason::InvalidContent => Self::InvalidContent,
            OfflineAssetConflictReason::PathConflict => Self::PathConflict,
        }
    }
}

impl From<OfflineAssetConflictReasonV1> for OfflineAssetConflictReason {
    fn from(value: OfflineAssetConflictReasonV1) -> Self {
        match value {
            OfflineAssetConflictReasonV1::CursorChanged => Self::CursorChanged,
            OfflineAssetConflictReasonV1::BaseChanged => Self::BaseChanged,
            OfflineAssetConflictReasonV1::InvalidContent => Self::InvalidContent,
            OfflineAssetConflictReasonV1::PathConflict => Self::PathConflict,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineAssetProjectionV1 {
    #[schemars(length(min = 1, max = 327))]
    pub logical_path: String,
    #[schemars(range(min = 1))]
    pub revision: u64,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub content_sha256: String,
    #[schemars(range(min = 1, max = 1048576))]
    pub size_bytes: u64,
    #[schemars(length(min = 1, max = 255))]
    pub media_type: String,
    #[serde(deserialize_with = "deserialize_true")]
    #[schemars(extend("const" = true))]
    pub active: bool,
}

impl From<&AuthoritativeAsset> for OfflineAssetProjectionV1 {
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

impl From<OfflineAssetProjectionV1> for AuthoritativeAsset {
    fn from(value: OfflineAssetProjectionV1) -> Self {
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
        offline_asset_capabilities, offline_asset_result_response,
    };
    use crate::application::{AssetReconciliationStatus, AuthoritativeAsset, OfflineAssetResult};

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
    fn put_accepts_the_maximum_asset_name_length() {
        let name = format!("{}.png", "x".repeat(super::MAX_ASSET_NAME_CHARS - 4));
        let digest = "a".repeat(64);
        let submission: OfflineAssetSubmission = serde_json::from_value(json!({
            "version":1,
            "operation":{
                "operation_id":"operation","device_id":"device","device_sequence":1,
                "cursor":"eyJ2ZXJzaW9uIjoxLCJzZXJ2ZXJfaWQiOiIwMTIzNDU2Nzg5YWJjZGVmMDEyMzQ1Njc4OWFiY2RlZiIsInBvc2l0aW9uIjowLCJ0b2tlbiI6IjExMTExMTExMTExMTExMTExMTExMTExMTExMTExMTExIn0",
                "authored_at_us":1,"kind":"put_asset",
                "logical_path":format!("assets/{digest}/{name}"),"original_name":name,
                "content_sha256":digest,"size_bytes":4,"media_type":"image/png",
                "base":{"kind":"absent"}
            }
        }))
        .unwrap();
        assert!(submission.decode().is_ok());
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

    #[test]
    fn result_decoder_classifies_unknown_versions() {
        let value = json!({"version":2,"result":{}});
        assert!(matches!(
            decode_offline_asset_result(value),
            Err(super::OfflineAssetWireError::Version)
        ));
    }

    #[test]
    fn response_builder_rejects_incoherent_authority() {
        let mut result = OfflineAssetResult {
            operation_id: "operation".into(),
            format_version: 1,
            status: AssetReconciliationStatus::Accepted,
            conflict_reason: None,
            authoritative: None,
            cursor: 0,
            cursor_server_id: "0".repeat(32),
            cursor_token: "1".repeat(32),
        };
        assert!(offline_asset_result_response(&result).is_err());
        result.authoritative = Some(AuthoritativeAsset {
            logical_path: format!("assets/{}/image.png", "a".repeat(64)),
            revision: 1,
            content_sha256: "a".repeat(64),
            size_bytes: 4,
            media_type: "image/png".into(),
            active: false,
        });
        assert!(offline_asset_result_response(&result).is_err());
        let name = format!("{}.png", "x".repeat(super::MAX_ASSET_NAME_CHARS - 3));
        result.authoritative.as_mut().unwrap().active = true;
        result.authoritative.as_mut().unwrap().logical_path =
            format!("assets/{}/{name}", "a".repeat(64));
        assert!(offline_asset_result_response(&result).is_err());
        result.status = AssetReconciliationStatus::Conflict;
        result.authoritative = None;
        assert!(offline_asset_result_response(&result).is_err());
    }
}
