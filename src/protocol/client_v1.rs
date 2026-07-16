//! Versioned DTOs for local replica clients.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::http_v1::{OfflineAnswerV1, ReviewPresentationV1, ReviewSessionRequestV1};

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
