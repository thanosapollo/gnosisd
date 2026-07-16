//! Versioned DTOs for local replica clients.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};

use super::http_v1::ReviewPresentationV1;

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
