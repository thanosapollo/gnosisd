//! Public JSON contracts for authenticated HTTP `/v1` clients.
//!
//! These response DTOs preserve the frozen v1 JSON shape. In particular,
//! [`ReplicaStateV1::next_device_sequence`] is serialized with the legacy
//! `next_review_sequence` field name. Deserialization enforces wire shape;
//! `LocalReplica` performs cursor, identity, projection, and ordering validation
//! before persistence.

use std::{borrow::Cow, collections::BTreeMap};

use schemars::{JsonSchema, Schema, SchemaGenerator};
use serde::{Deserialize, Deserializer, Serialize, de};

use crate::application::{
    AnswerInput, ReviewAttemptOperationResult, ReviewAttemptState, ReviewAttemptView,
    TransitionSnapshot,
};

const MIN_BEARER_TOKEN_BYTES: usize = 40;
const MAX_BEARER_TOKEN_BYTES: usize = 128;

pub(crate) fn valid_bearer_token(value: &str) -> bool {
    (MIN_BEARER_TOKEN_BYTES..=MAX_BEARER_TOKEN_BYTES).contains(&value.len())
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'+' | b'/' | b'=')
        })
}

#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Server identity, durable cursor, and next device sequence used to bootstrap a replica.
pub struct ReplicaStateV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub server_id: String,
    #[schemars(length(min = 1, max = 512))]
    pub cursor: String,
    #[serde(rename = "next_review_sequence")]
    #[schemars(rename = "next_review_sequence")]
    pub next_device_sequence: u64,
    #[schemars(extend("const" = true))]
    pub snapshot_required: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicaStateWireV1 {
    version: u32,
    server_id: String,
    cursor: String,
    next_device_sequence: Option<u64>,
    next_review_sequence: u64,
    snapshot_required: bool,
}

impl<'de> Deserialize<'de> for ReplicaStateV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ReplicaStateWireV1::deserialize(deserializer)?;
        if wire
            .next_device_sequence
            .is_some_and(|sequence| sequence != wire.next_review_sequence)
        {
            return Err(de::Error::custom("replica sequence aliases disagree"));
        }
        Ok(Self {
            version: wire.version,
            server_id: wire.server_id,
            cursor: wire.cursor,
            next_device_sequence: wire.next_review_sequence,
            snapshot_required: wire.snapshot_required,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContentCapabilitiesV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub server_id: String,
    #[schemars(extend("const" = 1))]
    pub document_lifecycle: u32,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetCapabilitiesV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(regex(pattern = "^[0-9a-f]{32}$"))]
    pub server_id: String,
    #[schemars(extend("const" = 1))]
    pub put_asset: u32,
    #[schemars(extend("const" = 1048576))]
    pub max_blob_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// A bounded delta page whose `from` and `next` cursors bracket `items` in order.
pub struct ReplicaChangePageV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(length(min = 1, max = 512))]
    pub from: String,
    #[schemars(length(min = 1, max = 512))]
    pub next: String,
    pub has_more: bool,
    #[schemars(length(max = 32))]
    pub items: Vec<ReplicaChangeV1>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReplicaChangeV1 {
    #[schemars(length(min = 1, max = 512))]
    pub cursor: String,
    #[serde(flatten)]
    pub projection: ReplicaProjectionV1,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// A bounded snapshot page. `next` continues the snapshot and `cursor` finishes it.
pub struct ReplicaSnapshotPageV1 {
    pub version: u32,
    pub snapshot_id: String,
    pub cursor: Option<String>,
    pub next: Option<String>,
    pub items: Vec<ReplicaProjectionV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplicaSnapshotPageWireV1 {
    version: u32,
    snapshot_id: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    cursor: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    next: Option<String>,
    items: Vec<ReplicaProjectionV1>,
}

fn deserialize_required_option<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

impl<'de> Deserialize<'de> for ReplicaSnapshotPageV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ReplicaSnapshotPageWireV1::deserialize(deserializer)?;
        if wire.cursor.is_some() == wire.next.is_some() {
            return Err(de::Error::custom("snapshot page must continue or finish"));
        }
        Ok(Self {
            version: wire.version,
            snapshot_id: wire.snapshot_id,
            cursor: wire.cursor,
            next: wire.next,
            items: wire.items,
        })
    }
}

impl JsonSchema for ReplicaSnapshotPageV1 {
    fn schema_name() -> Cow<'static, str> {
        "ReplicaSnapshotPageV1".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        let projection = generator.subschema_for::<ReplicaProjectionV1>();
        let cursor = bounded_cursor_schema();
        schemars::json_schema!({
            "type":"object",
            "required":["version","snapshot_id","cursor","next","items"],
            "properties":{
                "version":{"type":"integer","format":"uint32","minimum":0,"const":1},
                "snapshot_id":{"type":"string","minLength":1,"maxLength":4096},
                "cursor":{},
                "next":{},
                "items":{"type":"array","maxItems":32,"items":projection}
            },
            "oneOf":[
                {"properties":{"cursor":cursor.clone(),"next":{"type":"null"}}},
                {"properties":{"cursor":{"type":"null"},"next":cursor}}
            ],
            "additionalProperties":false
        })
    }
}

fn bounded_cursor_schema() -> Schema {
    schemars::json_schema!({"type":"string","minLength":1,"maxLength":512})
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(
    tag = "entity_kind",
    content = "projection",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ReplicaProjectionV1 {
    Document(DocumentRevisionV1),
    Asset(AssetRevisionV1),
    Thema(ThemaRevisionV1),
    Settings(SettingsRevisionV1),
    ReviewState(ReviewStateRevisionV1),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DocumentOperationV1 {
    Create,
    Update,
    Rename,
    Snapshot,
    Tombstone,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetOperationV1 {
    Create,
    Update,
    Tombstone,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemaKindV1 {
    Basic,
    Cloze,
    Mcq,
    #[serde(rename = "mc-cloze")]
    McCloze,
}

impl ThemaKindV1 {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Cloze => "cloze",
            Self::Mcq => "mcq",
            Self::McCloze => "mc-cloze",
        }
    }

    fn from_application(value: &str) -> Result<Self, ReviewAttemptWireError> {
        match value {
            "basic" => Ok(Self::Basic),
            "cloze" => Ok(Self::Cloze),
            "mcq" => Ok(Self::Mcq),
            "mc-cloze" => Ok(Self::McCloze),
            _ => Err(ReviewAttemptWireError::PresentationKind),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemaOperationV1 {
    Create,
    Update,
    Archive,
    Unarchive,
    Snapshot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SettingsOperationV1 {
    Snapshot,
    Update,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentRevisionV1 {
    pub document_id: String,
    #[schemars(range(min = 1))]
    pub revision: u64,
    pub logical_path: String,
    pub content_sha256: Option<String>,
    pub blob_url: Option<String>,
    pub operation: DocumentOperationV1,
    pub created_at_us: u64,
    pub author_device_id: Option<String>,
    pub projection: Option<DocumentProjectionV1>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentProjectionV1 {
    pub title: String,
    pub file_node_id: Option<String>,
    pub file_tags: Vec<String>,
    pub nodes: Vec<ProjectionNodeV1>,
    pub link_occurrences: Vec<ProjectionLinkV1>,
    pub semantic_edges: Vec<ProjectionEdgeV1>,
    pub compatibility_sha1: String,
    pub content_sha256: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionNodeV1 {
    pub node_id: String,
    pub title: String,
    pub title_path: Vec<String>,
    pub tags: Vec<String>,
    pub parent_id: Option<String>,
    pub level: u32,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionLinkV1 {
    pub owner_id: String,
    pub target_id: String,
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionEdgeV1 {
    pub source_id: String,
    pub target_id: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetRevisionV1 {
    pub logical_path: String,
    pub revision: u64,
    pub content_sha256: Option<String>,
    pub size_bytes: Option<u64>,
    pub media_type: Option<String>,
    pub blob_url: Option<String>,
    pub operation: AssetOperationV1,
    pub created_at_us: u64,
    pub author_device_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemaRevisionV1 {
    pub thema_id: String,
    pub revision: u64,
    pub kind: ThemaKindV1,
    pub keimenon: String,
    pub hypothesis: Vec<Option<String>>,
    pub answer: Vec<String>,
    pub source_guid: Option<String>,
    pub parathema: Option<String>,
    pub review_image: Option<String>,
    pub tags: Vec<String>,
    pub node_ids: Vec<String>,
    pub asset_paths: Vec<String>,
    pub content_sha256: String,
    pub archived: bool,
    pub operation: ThemaOperationV1,
    pub created_at_us: u64,
    pub author_device_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsRevisionV1 {
    pub revision: u64,
    pub timezone: String,
    #[schemars(range(max = 23))]
    pub day_boundary_hour: u8,
    pub scheduler: ParametersV1,
    pub tag_rules: BTreeMap<String, TagRuleV1>,
    pub operation: SettingsOperationV1,
    pub created_at_us: u64,
    pub author_device_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ParametersV1 {
    pub scheduler_version: u32,
    pub amnesia: f64,
    pub epignosis: f64,
    pub agnoia: f64,
    pub anagnosis: u64,
    pub lethe: u64,
    pub synolon_max: f64,
    pub interval_fuzz: f64,
    pub proto: Vec<u32>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TagRuleV1 {
    pub amnesia: Option<f64>,
    pub epignosis: Option<f64>,
    pub agnoia: Option<f64>,
    pub anagnosis: Option<u64>,
    pub lethe: Option<u64>,
    pub proto: Option<Vec<u32>>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewStateRevisionV1 {
    pub thema_id: String,
    pub state: ReviewStateV1,
    pub event_id: Option<String>,
    pub created_at_us: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewStateV1 {
    pub state_revision: u64,
    pub review_count: u64,
    pub legacy_review_count: u64,
    pub legacy_unclassified_count: u64,
    pub last_reviewed_at_us: Option<u64>,
    pub last_review_day: Option<u32>,
    pub next_review_day: u32,
    pub interval_days: Option<u32>,
    pub consecutive_successes: u64,
    pub total_successes: u64,
    pub consecutive_failures: u64,
    pub total_failures: u64,
    pub suspended: bool,
    pub gnosis_plus: f64,
    pub gnosis_minus: f64,
    pub gnosis_synolon: f64,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetAcceptedSuccessRequestV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
    )]
    pub operation_id: String,
    #[schemars(range(max = 9223372036854775807_u64))]
    pub expected_attempt_revision: u64,
    pub accepted_success: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SetSuspensionRequestV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
    )]
    pub operation_id: String,
    #[schemars(range(max = 9223372036854775807_u64))]
    pub expected_attempt_revision: u64,
    pub desired_suspended: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAttemptOperationResponseV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    #[schemars(
        length(min = 1, max = 128),
        regex(pattern = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
    )]
    pub operation_id: String,
    pub changed: bool,
    pub attempt_revision: u64,
    pub attempt: ReviewAttemptV1,
}

impl ReviewAttemptOperationResponseV1 {
    pub(crate) fn from_application(
        value: &ReviewAttemptOperationResult,
    ) -> Result<Self, ReviewAttemptWireError> {
        Ok(Self {
            version: 1,
            operation_id: value.operation_id.clone(),
            changed: value.changed,
            attempt_revision: value.attempt_revision,
            attempt: ReviewAttemptV1::from_application(&value.attempt)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAttemptStateResponseV1 {
    #[schemars(extend("const" = 1))]
    pub version: u32,
    pub attempt_revision: u64,
    pub attempt: ReviewAttemptV1,
}

impl ReviewAttemptStateResponseV1 {
    pub(crate) fn from_application(
        value: &ReviewAttemptState,
    ) -> Result<Self, ReviewAttemptWireError> {
        Ok(Self {
            version: 1,
            attempt_revision: value.attempt_revision,
            attempt: ReviewAttemptV1::from_application(&value.attempt)?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewAttemptV1 {
    pub attempt_id: String,
    pub session_id: String,
    pub thema_id: String,
    pub presentation: ReviewPresentationV1,
    pub revealed_answer: Option<Vec<String>>,
    pub revealed_parathema: Option<String>,
    pub matcher_version: u32,
    pub response: Option<ReviewAnswerV1>,
    pub detected_success: Option<bool>,
    pub accepted_success: Option<bool>,
    pub override_count: u32,
    pub proposed_transition: Option<ReviewTransitionV1>,
    pub desired_suspended: bool,
    pub status: ReviewAttemptStatusV1,
}

impl ReviewAttemptV1 {
    pub(crate) fn from_application(
        value: &ReviewAttemptView,
    ) -> Result<Self, ReviewAttemptWireError> {
        let reveal = value.response.is_some();
        Ok(Self {
            attempt_id: value.attempt_id.clone(),
            session_id: value.session_id.clone(),
            thema_id: value.thema_id.clone(),
            presentation: ReviewPresentationV1::from_application(value)?,
            revealed_answer: reveal.then(|| value.presentation.answer.clone()),
            revealed_parathema: reveal
                .then(|| value.presentation.parathema.clone())
                .flatten(),
            matcher_version: value.matcher_version,
            response: value.response.as_ref().map(ReviewAnswerV1::from),
            detected_success: value.detected_success,
            accepted_success: value.accepted_success,
            override_count: value.override_count,
            proposed_transition: value
                .proposed_transition
                .as_ref()
                .map(ReviewTransitionV1::from),
            desired_suspended: value.desired_suspended,
            status: ReviewAttemptStatusV1::from_application(&value.status)?,
        })
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewAttemptStatusV1 {
    Pending,
    Graded,
    Committed,
}

impl ReviewAttemptStatusV1 {
    fn from_application(value: &str) -> Result<Self, ReviewAttemptWireError> {
        match value {
            "pending" => Ok(Self::Pending),
            "graded" => Ok(Self::Graded),
            "committed" => Ok(Self::Committed),
            _ => Err(ReviewAttemptWireError::Status),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPresentationV1 {
    pub kind: ThemaKindV1,
    pub keimenon: String,
    pub hypothesis: Vec<Option<String>>,
    pub source_guid: Option<String>,
    pub review_image: Option<String>,
    pub tags: Vec<String>,
}

impl ReviewPresentationV1 {
    fn from_application(value: &ReviewAttemptView) -> Result<Self, ReviewAttemptWireError> {
        Ok(Self {
            kind: ThemaKindV1::from_application(&value.presentation.kind)?,
            keimenon: value.presentation.keimenon.clone(),
            hypothesis: value.presentation.hypothesis.clone(),
            source_guid: value.presentation.source_guid.clone(),
            review_image: value.presentation.review_image.clone(),
            tags: value.presentation.tags.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReviewAttemptWireError {
    PresentationKind,
    Status,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ReviewAnswerV1 {
    Text(String),
    Texts(Vec<String>),
    Choice(String),
}

impl From<&AnswerInput> for ReviewAnswerV1 {
    fn from(value: &AnswerInput) -> Self {
        match value {
            AnswerInput::Text(value) => Self::Text(value.clone()),
            AnswerInput::Texts(value) => Self::Texts(value.clone()),
            AnswerInput::Choice(value) => Self::Choice(value.clone()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewTransitionV1 {
    pub before: ReviewStateV1,
    pub after: ReviewStateV1,
    pub parameters: ParametersV1,
    pub reviewed_at_us: u64,
    pub review_day: u32,
    pub success: bool,
    pub first_review: bool,
    pub elapsed_days: u32,
    pub algorithm_interval_days: f64,
    pub computed_interval_days: u32,
    pub fuzz_delta: f64,
}

impl From<&TransitionSnapshot> for ReviewTransitionV1 {
    fn from(value: &TransitionSnapshot) -> Self {
        Self {
            before: ReviewStateV1::from(&value.before),
            after: ReviewStateV1::from(&value.after),
            parameters: ParametersV1::from(&value.parameters),
            reviewed_at_us: value.reviewed_at_us,
            review_day: value.review_day,
            success: value.success,
            first_review: value.first_review,
            elapsed_days: value.elapsed_days,
            algorithm_interval_days: value.algorithm_interval_days,
            computed_interval_days: value.computed_interval_days,
            fuzz_delta: value.fuzz_delta,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorCodeV1 {
    Unauthorized,
    Forbidden,
    NotFound,
    InvalidRequest,
    PreconditionRequired,
    Internal,
    InvalidContent,
    RequestTooLarge,
    InvalidBody,
    RequestTimeout,
    UnsupportedOfflineReviewVersion,
    InvalidOfflineReviewOperation,
    UnsupportedOfflineContentVersion,
    InvalidOfflineContentOperation,
    UnsupportedOfflineAssetVersion,
    InvalidOfflineAssetOperation,
    ReplicaProjectionTooLarge,
    SnapshotRequired,
    RevisionConflict,
    Busy,
    ActiveSession,
    StaleAttempt,
    ReviewStateConflict,
    IdempotencyConflict,
    DeviceSequenceGap,
    BlobRequired,
    BlobMetadataConflict,
    RenameRequired,
    InvalidBlobMetadata,
}

const METADATA_ERROR_CODES: [ApiErrorCodeV1; 4] = [
    ApiErrorCodeV1::SnapshotRequired,
    ApiErrorCodeV1::RevisionConflict,
    ApiErrorCodeV1::DeviceSequenceGap,
    ApiErrorCodeV1::BlobRequired,
];

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum ApiErrorMetaV1 {
    Snapshot {
        snapshot_url: String,
    },
    Revision {
        expected_revision: u64,
        current_revision: Option<u64>,
    },
    DeviceSequence {
        expected_sequence: u64,
    },
    Blob {
        content_sha256: String,
        upload_url: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
/// A client-visible error whose metadata variant is determined by `code`.
pub struct ApiErrorBodyV1 {
    code: ApiErrorCodeV1,
    message: String,
    meta: Option<ApiErrorMetaV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApiErrorBodyWireV1 {
    code: ApiErrorCodeV1,
    message: String,
    meta: Option<ApiErrorMetaV1>,
}

impl ApiErrorBodyV1 {
    pub(crate) fn new(code: ApiErrorCodeV1, message: String, meta: Option<ApiErrorMetaV1>) -> Self {
        assert!(error_metadata_matches(code, meta.as_ref()));
        Self {
            code,
            message,
            meta,
        }
    }

    pub const fn code(&self) -> ApiErrorCodeV1 {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub const fn meta(&self) -> Option<&ApiErrorMetaV1> {
        self.meta.as_ref()
    }
}

impl<'de> Deserialize<'de> for ApiErrorBodyV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ApiErrorBodyWireV1::deserialize(deserializer)?;
        if !error_metadata_matches(wire.code, wire.meta.as_ref()) {
            return Err(de::Error::custom("error metadata does not match its code"));
        }
        Ok(Self {
            code: wire.code,
            message: wire.message,
            meta: wire.meta,
        })
    }
}

impl JsonSchema for ApiErrorBodyV1 {
    fn schema_name() -> Cow<'static, str> {
        "ApiErrorBodyV1".into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        error_schema::json_schema(generator)
    }
}

fn error_metadata_matches(code: ApiErrorCodeV1, meta: Option<&ApiErrorMetaV1>) -> bool {
    match code {
        ApiErrorCodeV1::SnapshotRequired => matches!(meta, Some(ApiErrorMetaV1::Snapshot { .. })),
        ApiErrorCodeV1::RevisionConflict => matches!(meta, Some(ApiErrorMetaV1::Revision { .. })),
        ApiErrorCodeV1::DeviceSequenceGap => {
            matches!(meta, Some(ApiErrorMetaV1::DeviceSequence { .. }))
        }
        ApiErrorCodeV1::BlobRequired => matches!(meta, Some(ApiErrorMetaV1::Blob { .. })),
        _ => meta.is_none(),
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiErrorResponseV1 {
    pub error: ApiErrorBodyV1,
}

mod error_schema {
    #![allow(dead_code)]

    use schemars::{JsonSchema, Schema, SchemaGenerator};

    use super::{ApiErrorCodeV1, METADATA_ERROR_CODES};

    pub(super) fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        ApiErrorBodySchemaV1::json_schema(generator)
    }

    // These types exist only to express code-dependent metadata in OpenAPI.
    #[derive(JsonSchema)]
    #[serde(untagged)]
    enum ApiErrorBodySchemaV1 {
        Plain(PlainApiErrorSchemaV1),
        Snapshot(SnapshotApiErrorSchemaV1),
        Revision(RevisionApiErrorSchemaV1),
        DeviceSequence(DeviceSequenceApiErrorSchemaV1),
        Blob(BlobApiErrorSchemaV1),
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct PlainApiErrorSchemaV1 {
        code: PlainApiErrorCodeSchemaV1,
        message: String,
        meta: (),
    }

    struct PlainApiErrorCodeSchemaV1;

    impl JsonSchema for PlainApiErrorCodeSchemaV1 {
        fn schema_name() -> std::borrow::Cow<'static, str> {
            "PlainApiErrorCodeSchemaV1".into()
        }

        fn json_schema(generator: &mut SchemaGenerator) -> Schema {
            let canonical = generator.subschema_for::<ApiErrorCodeV1>();
            let metadata_codes = serde_json::to_value(METADATA_ERROR_CODES).unwrap();
            schemars::json_schema!({
                "allOf":[canonical,{"not":{"enum":metadata_codes}}]
            })
        }
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct SnapshotApiErrorSchemaV1 {
        #[schemars(extend("const" = "snapshot_required"))]
        code: String,
        message: String,
        meta: SnapshotErrorMetaSchemaV1,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct SnapshotErrorMetaSchemaV1 {
        #[schemars(extend("const" = "/v1/sync/snapshot"))]
        snapshot_url: String,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct RevisionApiErrorSchemaV1 {
        #[schemars(extend("const" = "revision_conflict"))]
        code: String,
        message: String,
        meta: RevisionErrorMetaSchemaV1,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct RevisionErrorMetaSchemaV1 {
        expected_revision: u64,
        current_revision: Option<u64>,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct DeviceSequenceApiErrorSchemaV1 {
        #[schemars(extend("const" = "device_sequence_gap"))]
        code: String,
        message: String,
        meta: DeviceSequenceErrorMetaSchemaV1,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct DeviceSequenceErrorMetaSchemaV1 {
        expected_sequence: u64,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct BlobApiErrorSchemaV1 {
        #[schemars(extend("const" = "blob_required"))]
        code: String,
        message: String,
        meta: BlobErrorMetaSchemaV1,
    }

    #[derive(JsonSchema)]
    #[serde(deny_unknown_fields)]
    struct BlobErrorMetaSchemaV1 {
        #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
        content_sha256: String,
        upload_url: String,
    }
}
