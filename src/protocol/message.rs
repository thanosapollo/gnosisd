use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u16 = 2;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_REQUEST_ID_LEN: usize = 128;
pub const MAX_REPAIR_BATCH_SIZE: u16 = 1_000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub version: u16,
    pub id: String,
    #[serde(flatten)]
    pub request: Request,
}

impl RequestEnvelope {
    pub(crate) fn validate(&self) -> Result<(), MessageValidationError> {
        if self.version != PROTOCOL_VERSION {
            return Err(MessageValidationError::UnsupportedVersion(self.version));
        }
        if self.id.is_empty() || self.id.len() > MAX_REQUEST_ID_LEN {
            return Err(MessageValidationError::InvalidRequestId);
        }
        self.request.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "method", content = "params", deny_unknown_fields)]
pub enum Request {
    #[serde(rename = "system.info")]
    SystemInfo {},

    #[serde(rename = "admin.device_create")]
    DeviceCreate {
        device_id: String,
        name: String,
        token: String,
        scopes: Vec<String>,
    },
    #[serde(rename = "admin.device_list")]
    DeviceList {},
    #[serde(rename = "admin.device_revoke")]
    DeviceRevoke { device_id: String },
    #[serde(rename = "admin.backup_create")]
    BackupCreate { destination: String },
    #[serde(rename = "admin.backup_verify")]
    BackupVerify { path: String },
    #[serde(rename = "admin.backup_prune")]
    BackupPrune { directory: String, keep: u16 },
    #[serde(rename = "admin.backup_restore_validate")]
    BackupRestoreValidate {
        path: String,
        temporary_directory: String,
    },
    #[serde(rename = "admin.repair")]
    Repair { batch_size: u16 },
    #[serde(rename = "admin.wipe_disposable_notes")]
    WipeDisposableNotes {},
}

impl Request {
    fn validate(&self) -> Result<(), MessageValidationError> {
        match self {
            Self::Repair { batch_size }
                if *batch_size == 0 || *batch_size > MAX_REPAIR_BATCH_SIZE =>
            {
                Err(MessageValidationError::InvalidRepairBatchSize)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub version: u16,
    pub id: String,
    #[serde(flatten)]
    pub outcome: ResponseOutcome,
}

impl ResponseEnvelope {
    pub fn success(id: String, result: ResponseResult) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            outcome: ResponseOutcome::Success {
                result: Box::new(result),
            },
        }
    }

    pub fn error(id: String, error: ProtocolError) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            outcome: ResponseOutcome::Failure { error },
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResponseOutcome {
    Success { result: Box<ResponseResult> },
    Failure { error: ProtocolError },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ResponseResult {
    SystemInfo(SystemInfo),
    DeviceCreated(DeviceCreatedView),
    Devices(Vec<DeviceView>),
    Device(DeviceView),
    Backup(BackupView),
    BackupsPruned(BackupsPrunedView),
    Repair(RepairView),
    NotesWiped(NotesWipedView),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RepairView {
    pub document_revisions_repaired: u64,
    pub blobs_verified: u64,
    pub max_batch_observed: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NotesWipedView {
    pub documents_removed: u64,
    pub themata_remaining: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackupsPrunedView {
    pub removed: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackupView {
    pub path: String,
    pub created_at_us: u64,
    pub database_sha256: String,
    pub blob_count: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceCreatedView {
    pub device: DeviceView,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceView {
    pub device_id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at_us: u64,
    pub last_used_at_us: Option<u64>,
    pub revoked_at_us: Option<u64>,
}

impl From<crate::application::DeviceRecord> for DeviceView {
    fn from(device: crate::application::DeviceRecord) -> Self {
        Self {
            device_id: device.device_id,
            name: device.name,
            scopes: device
                .scopes
                .into_iter()
                .map(|scope| scope.as_str().to_owned())
                .collect(),
            created_at_us: device.created_at_us,
            last_used_at_us: device.last_used_at_us,
            revoked_at_us: device.revoked_at_us,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SystemInfo {
    pub protocol_version: u16,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    MalformedRequest,
    UnsupportedVersion,
    InvalidDocument,
    NotFound,
    Conflict,
    RevisionConflict,
    IdempotencyConflict,
    StaleAttempt,
    ReviewStateConflict,
    Busy,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
}

impl ProtocolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum MessageValidationError {
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("request ID must contain 1 to {MAX_REQUEST_ID_LEN} bytes")]
    InvalidRequestId,
    #[error("repair batch size must contain 1 to {MAX_REPAIR_BATCH_SIZE} items")]
    InvalidRepairBatchSize,
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_REQUEST_ID_LEN, MessageValidationError, PROTOCOL_VERSION, Request, RequestEnvelope,
    };

    #[test]
    fn operator_request_round_trips() {
        let request = RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "request-1".into(),
            request: Request::SystemInfo {},
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&json).unwrap(),
            request
        );
    }

    #[test]
    fn rejects_removed_ordinary_methods() {
        for method in [
            "document.import",
            "document.get",
            "thema.get",
            "review.due",
            "review.session_create",
            "review.attempt_begin",
            "asset.chunk",
        ] {
            let json =
                format!(r#"{{"version":2,"id":"request","method":"{method}","params":{{}}}}"#);
            assert!(
                serde_json::from_str::<RequestEnvelope>(&json).is_err(),
                "accepted {method}"
            );
        }
    }

    #[test]
    fn rejects_unknown_version_and_unbounded_request_id() {
        let mut request = RequestEnvelope {
            version: 1,
            id: "request-1".into(),
            request: Request::SystemInfo {},
        };
        assert_eq!(
            request.validate(),
            Err(MessageValidationError::UnsupportedVersion(1))
        );
        request.version = PROTOCOL_VERSION;
        request.id = "x".repeat(MAX_REQUEST_ID_LEN + 1);
        assert_eq!(
            request.validate(),
            Err(MessageValidationError::InvalidRequestId)
        );
    }
}
