//! Versioned DTOs for local replica clients.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::http_v1::{
    ApiErrorResponseV1, OfflineAnswerV1, ReviewPresentationV1, ReviewSessionRequestV1,
};

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// A request to create and acquire an offline review pack.
pub struct ReviewPackAcquireRequestV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    pub session: ReviewSessionRequestV1,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// A complete replayable input for one offline answer.
pub struct OfflineReviewAnswerRequestV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(length(min = 1, max = 128))]
    pub pack_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub attempt_id: String,
    pub session_position: u32,
    pub response: OfflineAnswerV1,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub accepted_success: Option<bool>,
    pub desired_suspended: bool,
    #[schemars(range(min = 1))]
    pub answered_at_us: u64,
    pub fuzz_sample: f64,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// The current offline review question, with captured answers concealed.
pub struct OfflineReviewQuestionV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(length(min = 1, max = 128))]
    pub pack_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub session_id: String,
    pub session_position: u32,
    pub queue_length: u32,
    #[schemars(length(min = 1, max = 128))]
    pub thema_id: String,
    pub thema_revision: u64,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub content_sha256: String,
    pub presentation: ReviewPresentationV1,
    pub asset_paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// The answer-concealed state of the active local review pack.
pub struct ReviewPackCurrentV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub question: Option<OfflineReviewQuestionV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// The durable local receipt for an offline answer.
pub struct OfflineReviewAnswerReceiptV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(length(min = 1, max = 128))]
    pub attempt_id: String,
    #[schemars(range(min = 1))]
    pub device_sequence: u64,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub question: Option<OfflineReviewQuestionV1>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// The durable identity of a review pack action.
pub struct ReviewPackReceiptV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(length(min = 1, max = 128))]
    pub pack_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// Identifies one verified asset stored in an offline review pack.
pub struct OfflineReviewAssetRequestV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(length(min = 1, max = 128))]
    pub pack_id: String,
    #[schemars(length(min = 1, max = 512))]
    pub logical_path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// A verified content-addressed asset read from the local replica.
pub struct OfflineReviewAssetV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(length(min = 1, max = 128))]
    pub pack_id: String,
    #[schemars(length(min = 1, max = 512))]
    pub logical_path: String,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub content_sha256: String,
    pub size_bytes: u64,
    #[schemars(length(min = 1, max = 128))]
    pub media_type: String,
    pub data_base64: String,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// Paging parameters for locally retained offline review results.
pub struct OfflineReviewResultPageRequestV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(required)]
    #[schemars(range(max = 9_223_372_036_854_775_807_u64))]
    pub after: Option<u64>,
    #[schemars(range(min = 1, max = 100))]
    pub limit: u16,
    pub conflicts_only: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OfflineReviewResultPageRequestWireV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    version: u32,
    #[serde(deserialize_with = "deserialize_required_option")]
    after: Option<u64>,
    limit: u16,
    conflicts_only: bool,
}

impl<'de> Deserialize<'de> for OfflineReviewResultPageRequestV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = OfflineReviewResultPageRequestWireV1::deserialize(deserializer)?;
        if !(1..=100).contains(&wire.limit)
            || wire.after.is_some_and(|value| value > i64::MAX as u64)
        {
            return Err(serde::de::Error::custom("result page request is invalid"));
        }
        Ok(Self {
            version: wire.version,
            after: wire.after,
            limit: wire.limit,
            conflicts_only: wire.conflicts_only,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflineReviewConflictReasonV1 {
    LeaseEnded,
    SessionAdvanced,
    ContentChanged,
    SchedulingChanged,
    CaptureMismatch,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum OfflineReviewResultV1 {
    Accepted {
        result_sequence: u64,
        #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
        server_id: String,
        #[schemars(length(min = 1, max = 128))]
        attempt_id: String,
        device_sequence: u64,
        #[schemars(length(min = 1, max = 128))]
        event_id: String,
    },
    Conflict {
        result_sequence: u64,
        #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
        server_id: String,
        #[schemars(length(min = 1, max = 128))]
        attempt_id: String,
        device_sequence: u64,
        conflict_reason: OfflineReviewConflictReasonV1,
    },
}

impl OfflineReviewResultV1 {
    pub const fn conflict_reason(&self) -> Option<OfflineReviewConflictReasonV1> {
        match self {
            Self::Accepted { .. } => None,
            Self::Conflict {
                conflict_reason, ..
            } => Some(*conflict_reason),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// A safe page of reconciliation outcomes without captured review operations.
pub struct OfflineReviewResultPageV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    pub items: Vec<OfflineReviewResultV1>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub next_after: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientFailureCodeV1 {
    Input,
    Credentials,
    Http,
    Server,
    Replica,
    Synchronization,
    ReviewPack,
    Protocol,
    Output,
    Path,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientDeliveryV1 {
    NotSent,
    Confirmed,
    Unknown,
}

#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// A stable machine-readable failure from the local client executable.
pub struct ClientFailureV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    pub code: ClientFailureCodeV1,
    pub delivery: ClientDeliveryV1,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub operation_id: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub api_error: Option<ApiErrorResponseV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClientFailureWireV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    version: u32,
    code: ClientFailureCodeV1,
    delivery: ClientDeliveryV1,
    #[serde(deserialize_with = "deserialize_required_option")]
    operation_id: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    api_error: Option<ApiErrorResponseV1>,
}

impl<'de> Deserialize<'de> for ClientFailureV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ClientFailureWireV1::deserialize(deserializer)?;
        let server = wire.code == ClientFailureCodeV1::Server;
        let valid_operation = wire
            .operation_id
            .as_ref()
            .is_none_or(|value| valid_client_operation_id(value));
        if server != wire.api_error.is_some() || !valid_operation {
            return Err(serde::de::Error::custom("client failure is inconsistent"));
        }
        Ok(Self {
            version: wire.version,
            code: wire.code,
            delivery: wire.delivery,
            operation_id: wire.operation_id,
            api_error: wire.api_error,
        })
    }
}

pub fn valid_client_operation_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value.len() <= 128
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}

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
    if version == 1 {
        Ok(version)
    } else {
        Err(serde::de::Error::custom("protocol version is unsupported"))
    }
}
