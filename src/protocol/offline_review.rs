use std::{collections::BTreeSet, io};

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::application::{
    AnswerInput, MAX_OFFLINE_ASSET_BYTES, MAX_REVIEW_PACK_BYTES, OfflineConflictReason,
    OfflineReviewOperation, OfflineReviewResult, ParametersSnapshot, PresentationSnapshot,
    ReconciliationStatus, ReviewPackAsset, ReviewPackItem, ReviewPackView, ReviewSessionMode,
    ReviewSessionView, ReviewSource, ReviewStateSnapshot, TransitionSnapshot, presentation_content,
    review_day_at,
};
use crate::{is_canonical_sha256, protocol::replica::encode_cursor};

pub(crate) const OFFLINE_REVIEW_VERSION: u32 = 1;
pub(crate) const MAX_OFFLINE_REVIEW_OPERATION_BYTES: usize =
    MAX_REVIEW_PACK_BYTES + 2 * MAX_OFFLINE_ASSET_BYTES as usize;
pub(crate) const MAX_OFFLINE_REVIEW_RESULT_BYTES: usize = MAX_REVIEW_PACK_BYTES + 64 * 1024;
const OFFLINE_REVIEW_RESPONSE_VERSION: u32 = 2;

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
    deserialize_version::<D, 1>(deserializer)
}

fn deserialize_version_2<'de, D>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    deserialize_version::<D, 2>(deserializer)
}

fn deserialize_version<'de, D, const VERSION: u32>(deserializer: D) -> Result<u32, D::Error>
where
    D: Deserializer<'de>,
{
    let version = u32::deserialize(deserializer)?;
    if version == VERSION {
        Ok(version)
    } else {
        Err(serde::de::Error::custom("protocol version is unsupported"))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OfflineReviewSubmission {
    version: u32,
    operation: Value,
}

pub(crate) struct DecodedOfflineReview {
    pub(crate) version: u32,
    pub(crate) operation: OfflineReviewOperation,
    pub(crate) canonical_payload: String,
}

impl OfflineReviewSubmission {
    pub(crate) fn decode(self) -> Result<DecodedOfflineReview, OfflineReviewWireError> {
        if self.version != OFFLINE_REVIEW_VERSION {
            return Err(OfflineReviewWireError::Version);
        }
        let operation: OfflineReviewOperationV1 = serde_json::from_value(self.operation)
            .map_err(|_| OfflineReviewWireError::Operation)?;
        let canonical_payload = encode_v1_operation(&operation)?;
        Ok(DecodedOfflineReview {
            version: self.version,
            operation: operation.into(),
            canonical_payload,
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum OfflineReviewWireError {
    #[error("offline review protocol version is unsupported")]
    Version,
    #[error("offline review operation is invalid")]
    Operation,
}

pub(crate) fn canonical_offline_operation(
    version: u32,
    operation: &OfflineReviewOperation,
) -> Result<String, OfflineReviewWireError> {
    if version != OFFLINE_REVIEW_VERSION {
        return Err(OfflineReviewWireError::Version);
    }
    encode_v1_operation(&OfflineReviewOperationV1::try_from(operation)?)
}

fn encode_v1_operation(
    operation: &OfflineReviewOperationV1,
) -> Result<String, OfflineReviewWireError> {
    serde_json::to_string(&VersionedOperationV1 {
        version: OFFLINE_REVIEW_VERSION,
        operation,
    })
    .map_err(|_| OfflineReviewWireError::Operation)
}

#[cfg(test)]
pub(crate) fn review_pack_json(pack: &ReviewPackView) -> Result<Value, serde_json::Error> {
    serde_json::to_value(review_pack_response(pack)?)
}

pub(crate) fn review_pack_response(
    pack: &ReviewPackView,
) -> Result<ReviewPackEnvelopeV2, serde_json::Error> {
    Ok(ReviewPackEnvelopeV2 {
        version: OFFLINE_REVIEW_RESPONSE_VERSION,
        pack: ReviewPackV2::try_from(pack)?,
    })
}

pub(crate) fn active_review_pack_response(
    pack: Option<&ReviewPackView>,
) -> Result<ActiveReviewPackEnvelopeV2, serde_json::Error> {
    Ok(ActiveReviewPackEnvelopeV2 {
        version: OFFLINE_REVIEW_RESPONSE_VERSION,
        pack: pack.map(ReviewPackV2::try_from).transpose()?,
    })
}

#[cfg(test)]
pub(crate) fn offline_result_json(
    result: &OfflineReviewResult,
) -> Result<Value, serde_json::Error> {
    serde_json::to_value(offline_result_response(result)?)
}

pub(crate) fn offline_result_response(
    result: &OfflineReviewResult,
) -> Result<OfflineReviewResultEnvelopeV2, serde_json::Error> {
    Ok(OfflineReviewResultEnvelopeV2 {
        version: OFFLINE_REVIEW_RESPONSE_VERSION,
        result: OfflineReviewResultV2::try_from(result)?,
    })
}

pub(crate) fn decode_review_pack(value: Value) -> Result<ReviewPackView, OfflineReviewWireError> {
    require_response_size(&value)?;
    require_response_version(&value)?;
    let envelope: ReviewPackEnvelopeV2 =
        serde_json::from_value(value).map_err(|_| OfflineReviewWireError::Operation)?;
    ReviewPackView::try_from(envelope.pack)
}

pub(crate) fn decode_offline_result(
    value: Value,
) -> Result<OfflineReviewResult, OfflineReviewWireError> {
    require_response_size(&value)?;
    require_response_version(&value)?;
    let envelope: OfflineReviewResultEnvelopeV2 =
        serde_json::from_value(value).map_err(|_| OfflineReviewWireError::Operation)?;
    OfflineReviewResult::try_from(envelope.result)
}

fn require_response_version(value: &Value) -> Result<(), OfflineReviewWireError> {
    let version = value
        .get("version")
        .and_then(Value::as_u64)
        .ok_or(OfflineReviewWireError::Operation)?;
    if version == u64::from(OFFLINE_REVIEW_RESPONSE_VERSION) {
        Ok(())
    } else {
        Err(OfflineReviewWireError::Version)
    }
}

fn require_response_size(value: &Value) -> Result<(), OfflineReviewWireError> {
    let bytes = serde_json::to_vec(value).map_err(|_| OfflineReviewWireError::Operation)?;
    if bytes.len() <= MAX_OFFLINE_REVIEW_RESULT_BYTES {
        Ok(())
    } else {
        Err(OfflineReviewWireError::Operation)
    }
}

#[derive(Serialize)]
struct VersionedOperationV1<'a> {
    version: u32,
    operation: &'a OfflineReviewOperationV1,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineReviewOperationEnvelopeV1 {
    #[serde(deserialize_with = "deserialize_version_1")]
    #[schemars(extend("const" = 1))]
    pub version: u32,
    pub operation: OfflineReviewOperationV1,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineReviewOperationV1 {
    #[schemars(length(min = 1, max = 128))]
    pub attempt_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub pack_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub session_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub lease_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub device_id: String,
    #[schemars(range(min = 1))]
    pub device_sequence: u64,
    pub session_position: u32,
    #[schemars(length(min = 1, max = 128))]
    pub thema_id: String,
    pub thema_revision: u64,
    pub base_state_revision: u64,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub content_sha256: String,
    pub presentation: OfflinePresentationV1,
    pub parameters: OfflineParametersV1,
    pub matcher_version: u32,
    pub response: OfflineAnswerV1,
    pub detected_success: bool,
    pub accepted_success: bool,
    pub override_count: u32,
    pub proposed_transition: OfflineTransitionV1,
    pub desired_suspended: bool,
    #[schemars(range(min = 1))]
    pub answered_at_us: u64,
    pub fuzz_sample: f64,
}

impl From<OfflineReviewOperationV1> for OfflineReviewOperation {
    fn from(value: OfflineReviewOperationV1) -> Self {
        Self {
            attempt_id: value.attempt_id,
            pack_id: value.pack_id,
            session_id: value.session_id,
            lease_id: value.lease_id,
            device_id: value.device_id,
            device_sequence: value.device_sequence,
            session_position: value.session_position,
            thema_id: value.thema_id,
            thema_revision: value.thema_revision,
            base_state_revision: value.base_state_revision,
            content_sha256: value.content_sha256,
            presentation: value.presentation.into(),
            parameters: value.parameters.into(),
            matcher_version: value.matcher_version,
            response: value.response.into(),
            detected_success: value.detected_success,
            accepted_success: value.accepted_success,
            override_count: value.override_count,
            proposed_transition: value.proposed_transition.into(),
            desired_suspended: value.desired_suspended,
            answered_at_us: value.answered_at_us,
            fuzz_sample: value.fuzz_sample,
        }
    }
}

impl TryFrom<&OfflineReviewOperation> for OfflineReviewOperationV1 {
    type Error = OfflineReviewWireError;

    fn try_from(value: &OfflineReviewOperation) -> Result<Self, Self::Error> {
        Ok(Self {
            attempt_id: value.attempt_id.clone(),
            pack_id: value.pack_id.clone(),
            session_id: value.session_id.clone(),
            lease_id: value.lease_id.clone(),
            device_id: value.device_id.clone(),
            device_sequence: value.device_sequence,
            session_position: value.session_position,
            thema_id: value.thema_id.clone(),
            thema_revision: value.thema_revision,
            base_state_revision: value.base_state_revision,
            content_sha256: value.content_sha256.clone(),
            presentation: OfflinePresentationV1::try_from(&value.presentation)
                .map_err(|_| OfflineReviewWireError::Operation)?,
            parameters: (&value.parameters).into(),
            matcher_version: value.matcher_version,
            response: (&value.response).into(),
            detected_success: value.detected_success,
            accepted_success: value.accepted_success,
            override_count: value.override_count,
            proposed_transition: (&value.proposed_transition).into(),
            desired_suspended: value.desired_suspended,
            answered_at_us: value.answered_at_us,
            fuzz_sample: value.fuzz_sample,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflinePresentationV1 {
    pub kind: OfflineThemaKindV1,
    pub keimenon: String,
    pub hypothesis: Vec<Option<String>>,
    pub answer: Vec<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub source_guid: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub parathema: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub review_image: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum OfflineThemaKindV1 {
    Basic,
    Cloze,
    Mcq,
    McCloze,
}

impl OfflineThemaKindV1 {
    fn parse(value: &str) -> Result<Self, serde_json::Error> {
        match value {
            "basic" => Ok(Self::Basic),
            "cloze" => Ok(Self::Cloze),
            "mcq" => Ok(Self::Mcq),
            "mc-cloze" => Ok(Self::McCloze),
            _ => Err(invalid_response("offline thema kind is invalid")),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Basic => "basic",
            Self::Cloze => "cloze",
            Self::Mcq => "mcq",
            Self::McCloze => "mc-cloze",
        }
    }
}

impl From<OfflinePresentationV1> for PresentationSnapshot {
    fn from(value: OfflinePresentationV1) -> Self {
        Self {
            kind: value.kind.as_str().into(),
            keimenon: value.keimenon,
            hypothesis: value.hypothesis,
            answer: value.answer,
            source_guid: value.source_guid,
            parathema: value.parathema,
            review_image: value.review_image,
            tags: value.tags,
        }
    }
}

impl TryFrom<&PresentationSnapshot> for OfflinePresentationV1 {
    type Error = serde_json::Error;

    fn try_from(value: &PresentationSnapshot) -> Result<Self, Self::Error> {
        Ok(Self {
            kind: OfflineThemaKindV1::parse(&value.kind)?,
            keimenon: value.keimenon.clone(),
            hypothesis: value.hypothesis.clone(),
            answer: value.answer.clone(),
            source_guid: value.source_guid.clone(),
            parathema: value.parathema.clone(),
            review_image: value.review_image.clone(),
            tags: value.tags.clone(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineReviewStateV1 {
    pub state_revision: u64,
    pub review_count: u64,
    pub legacy_review_count: u64,
    pub legacy_unclassified_count: u64,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub last_reviewed_at_us: Option<u64>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub last_review_day: Option<u32>,
    pub next_review_day: u32,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
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

impl From<OfflineReviewStateV1> for ReviewStateSnapshot {
    fn from(value: OfflineReviewStateV1) -> Self {
        Self {
            state_revision: value.state_revision,
            review_count: value.review_count,
            legacy_review_count: value.legacy_review_count,
            legacy_unclassified_count: value.legacy_unclassified_count,
            last_reviewed_at_us: value.last_reviewed_at_us,
            last_review_day: value.last_review_day,
            next_review_day: value.next_review_day,
            interval_days: value.interval_days,
            consecutive_successes: value.consecutive_successes,
            total_successes: value.total_successes,
            consecutive_failures: value.consecutive_failures,
            total_failures: value.total_failures,
            suspended: value.suspended,
            gnosis_plus: value.gnosis_plus,
            gnosis_minus: value.gnosis_minus,
            gnosis_synolon: value.gnosis_synolon,
        }
    }
}

impl From<&ReviewStateSnapshot> for OfflineReviewStateV1 {
    fn from(value: &ReviewStateSnapshot) -> Self {
        Self {
            state_revision: value.state_revision,
            review_count: value.review_count,
            legacy_review_count: value.legacy_review_count,
            legacy_unclassified_count: value.legacy_unclassified_count,
            last_reviewed_at_us: value.last_reviewed_at_us,
            last_review_day: value.last_review_day,
            next_review_day: value.next_review_day,
            interval_days: value.interval_days,
            consecutive_successes: value.consecutive_successes,
            total_successes: value.total_successes,
            consecutive_failures: value.consecutive_failures,
            total_failures: value.total_failures,
            suspended: value.suspended,
            gnosis_plus: value.gnosis_plus,
            gnosis_minus: value.gnosis_minus,
            gnosis_synolon: value.gnosis_synolon,
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineParametersV1 {
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

impl From<OfflineParametersV1> for ParametersSnapshot {
    fn from(value: OfflineParametersV1) -> Self {
        Self {
            scheduler_version: value.scheduler_version,
            amnesia: value.amnesia,
            epignosis: value.epignosis,
            agnoia: value.agnoia,
            anagnosis: value.anagnosis,
            lethe: value.lethe,
            synolon_max: value.synolon_max,
            interval_fuzz: value.interval_fuzz,
            proto: value.proto,
        }
    }
}

impl From<&ParametersSnapshot> for OfflineParametersV1 {
    fn from(value: &ParametersSnapshot) -> Self {
        Self {
            scheduler_version: value.scheduler_version,
            amnesia: value.amnesia,
            epignosis: value.epignosis,
            agnoia: value.agnoia,
            anagnosis: value.anagnosis,
            lethe: value.lethe,
            synolon_max: value.synolon_max,
            interval_fuzz: value.interval_fuzz,
            proto: value.proto.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum OfflineAnswerV1 {
    Text(String),
    Texts(Vec<String>),
    Choice(String),
}

impl From<OfflineAnswerV1> for AnswerInput {
    fn from(value: OfflineAnswerV1) -> Self {
        match value {
            OfflineAnswerV1::Text(value) => Self::Text(value),
            OfflineAnswerV1::Texts(value) => Self::Texts(value),
            OfflineAnswerV1::Choice(value) => Self::Choice(value),
        }
    }
}

impl From<&AnswerInput> for OfflineAnswerV1 {
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
pub struct OfflineTransitionV1 {
    pub before: OfflineReviewStateV1,
    pub after: OfflineReviewStateV1,
    pub parameters: OfflineParametersV1,
    pub reviewed_at_us: u64,
    pub review_day: u32,
    pub success: bool,
    pub first_review: bool,
    pub elapsed_days: u32,
    pub algorithm_interval_days: f64,
    pub computed_interval_days: u32,
    pub fuzz_delta: f64,
}

impl From<OfflineTransitionV1> for TransitionSnapshot {
    fn from(value: OfflineTransitionV1) -> Self {
        Self {
            before: value.before.into(),
            after: value.after.into(),
            parameters: value.parameters.into(),
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

impl From<&TransitionSnapshot> for OfflineTransitionV1 {
    fn from(value: &TransitionSnapshot) -> Self {
        Self {
            before: (&value.before).into(),
            after: (&value.after).into(),
            parameters: (&value.parameters).into(),
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

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPackEnvelopeV2 {
    #[serde(deserialize_with = "deserialize_version_2")]
    #[schemars(extend("const" = 2))]
    pub version: u32,
    pub pack: ReviewPackV2,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveReviewPackEnvelopeV2 {
    #[serde(deserialize_with = "deserialize_version_2")]
    #[schemars(extend("const" = 2))]
    pub version: u32,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub pack: Option<ReviewPackV2>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPackV2 {
    #[schemars(length(min = 1, max = 128))]
    pub pack_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub session_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub device_id: String,
    #[schemars(length(min = 1, max = 128))]
    pub lease_id: String,
    pub status: ReviewPackStatusV2,
    #[schemars(length(min = 1, max = 512))]
    pub cursor: String,
    #[schemars(range(min = 1))]
    pub issued_at_us: u64,
    pub current_position: u32,
    pub timezone: String,
    #[schemars(range(max = 23))]
    pub day_boundary_hour: u8,
    #[schemars(length(min = 1, max = 500))]
    pub items: Vec<ReviewPackItemV2>,
    pub assets: Vec<ReviewPackAssetV2>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewPackStatusV2 {
    Active,
    Completed,
    Abandoned,
}

impl ReviewPackStatusV2 {
    fn parse(value: &str) -> Result<Self, serde_json::Error> {
        match value {
            "active" => Ok(Self::Active),
            "completed" => Ok(Self::Completed),
            "abandoned" => Ok(Self::Abandoned),
            _ => Err(invalid_response("review pack status is invalid")),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Abandoned => "abandoned",
        }
    }
}

impl TryFrom<&ReviewPackView> for ReviewPackV2 {
    type Error = serde_json::Error;

    fn try_from(value: &ReviewPackView) -> Result<Self, Self::Error> {
        Ok(Self {
            pack_id: value.pack_id.clone(),
            session_id: value.session_id.clone(),
            device_id: value.device_id.clone(),
            lease_id: value.lease_id.clone(),
            status: ReviewPackStatusV2::parse(&value.status)?,
            cursor: durable_cursor(&value.cursor_server_id, value.cursor, &value.cursor_token)?,
            issued_at_us: value.issued_at_us,
            current_position: value.current_position,
            timezone: value.timezone.clone(),
            day_boundary_hour: value.day_boundary_hour,
            items: value
                .items
                .iter()
                .map(ReviewPackItemV2::try_from)
                .collect::<Result<_, _>>()?,
            assets: value.assets.iter().map(ReviewPackAssetV2::from).collect(),
        })
    }
}

impl TryFrom<ReviewPackV2> for ReviewPackView {
    type Error = OfflineReviewWireError;

    fn try_from(value: ReviewPackV2) -> Result<Self, Self::Error> {
        validate_review_pack(&value)?;
        let (server_id, cursor, token) = decode_durable_cursor(&value.cursor)?;
        if value.pack_id.is_empty()
            || value.session_id.is_empty()
            || value.device_id.is_empty()
            || value.lease_id.is_empty()
            || value.issued_at_us == 0
            || value.day_boundary_hour > 23
        {
            return Err(OfflineReviewWireError::Operation);
        }
        let pack = Self {
            format_version: OFFLINE_REVIEW_VERSION,
            pack_id: value.pack_id,
            session_id: value.session_id,
            device_id: value.device_id,
            lease_id: value.lease_id,
            status: value.status.as_str().into(),
            cursor,
            cursor_server_id: server_id,
            cursor_token: token,
            issued_at_us: value.issued_at_us,
            current_position: value.current_position,
            timezone: value.timezone,
            day_boundary_hour: value.day_boundary_hour,
            items: value.items.into_iter().map(ReviewPackItem::from).collect(),
            assets: value
                .assets
                .into_iter()
                .map(ReviewPackAsset::from)
                .collect(),
        };
        validate_decoded_pack(&pack)?;
        Ok(pack)
    }
}

fn validate_review_pack(value: &ReviewPackV2) -> Result<(), OfflineReviewWireError> {
    let coherent_status = match value.status {
        ReviewPackStatusV2::Active => (value.current_position as usize) < value.items.len(),
        ReviewPackStatusV2::Completed => value.current_position as usize == value.items.len(),
        ReviewPackStatusV2::Abandoned => value.current_position as usize <= value.items.len(),
    };
    if value.items.is_empty() || !coherent_status {
        return Err(OfflineReviewWireError::Operation);
    }
    let assets = value
        .assets
        .iter()
        .map(|asset| asset.logical_path.as_str())
        .collect::<BTreeSet<_>>();
    if assets.len() != value.assets.len()
        || value.assets.iter().any(|asset| {
            !is_canonical_sha256(&asset.content_sha256)
                || asset.media_type.is_empty()
                || crate::application::validate_asset_path(&asset.logical_path).is_err()
                || asset.logical_path.split('/').nth(1) != Some(asset.content_sha256.as_str())
        })
    {
        return Err(OfflineReviewWireError::Operation);
    }
    for (position, item) in value.items.iter().enumerate() {
        if item.position as usize != position
            || item.thema_id.is_empty()
            || !is_canonical_sha256(&item.content_sha256)
            || item.asset_paths.iter().collect::<BTreeSet<_>>().len() != item.asset_paths.len()
            || item
                .asset_paths
                .iter()
                .any(|path| !assets.contains(path.as_str()))
        {
            return Err(OfflineReviewWireError::Operation);
        }
    }
    Ok(())
}

fn validate_decoded_pack(pack: &ReviewPackView) -> Result<(), OfflineReviewWireError> {
    review_day_at(pack.issued_at_us, &pack.timezone, pack.day_boundary_hour)
        .map_err(|_| OfflineReviewWireError::Operation)?;
    for item in &pack.items {
        let content = presentation_content(&item.presentation)
            .map_err(|_| OfflineReviewWireError::Operation)?;
        crate::application::prepare_thema(
            Some(item.thema_id.clone()),
            content,
            item.presentation.tags.clone(),
            Vec::new(),
            item.asset_paths.clone(),
            None,
        )
        .map_err(|_| OfflineReviewWireError::Operation)?;
        crate::domain::validate_parameters(&item.parameters.clone().into())
            .map_err(|_| OfflineReviewWireError::Operation)?;
        crate::domain::validate_state(&item.review_state.clone().into())
            .map_err(|_| OfflineReviewWireError::Operation)?;
        if item.matcher_version != crate::domain::MATCHER_VERSION {
            return Err(OfflineReviewWireError::Operation);
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPackItemV2 {
    pub position: u32,
    #[schemars(length(min = 1, max = 128))]
    pub thema_id: String,
    pub thema_revision: u64,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub content_sha256: String,
    pub presentation: OfflinePresentationV1,
    pub review_state: OfflineReviewStateV1,
    pub parameters: OfflineParametersV1,
    pub matcher_version: u32,
    pub asset_paths: Vec<String>,
}

impl TryFrom<&ReviewPackItem> for ReviewPackItemV2 {
    type Error = serde_json::Error;

    fn try_from(value: &ReviewPackItem) -> Result<Self, Self::Error> {
        Ok(Self {
            position: value.position,
            thema_id: value.thema_id.clone(),
            thema_revision: value.thema_revision,
            content_sha256: value.content_sha256.clone(),
            presentation: OfflinePresentationV1::try_from(&value.presentation)?,
            review_state: (&value.review_state).into(),
            parameters: (&value.parameters).into(),
            matcher_version: value.matcher_version,
            asset_paths: value.asset_paths.clone(),
        })
    }
}

impl From<ReviewPackItemV2> for ReviewPackItem {
    fn from(value: ReviewPackItemV2) -> Self {
        Self {
            position: value.position,
            thema_id: value.thema_id,
            thema_revision: value.thema_revision,
            content_sha256: value.content_sha256,
            presentation: value.presentation.into(),
            review_state: value.review_state.into(),
            parameters: value.parameters.into(),
            matcher_version: value.matcher_version,
            asset_paths: value.asset_paths,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewPackAssetV2 {
    #[schemars(length(min = 1))]
    pub logical_path: String,
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub content_sha256: String,
    pub size_bytes: u64,
    #[schemars(length(min = 1, max = 255))]
    pub media_type: String,
}

impl From<&ReviewPackAsset> for ReviewPackAssetV2 {
    fn from(value: &ReviewPackAsset) -> Self {
        Self {
            logical_path: value.logical_path.clone(),
            content_sha256: value.content_sha256.clone(),
            size_bytes: value.size_bytes,
            media_type: value.media_type.clone(),
        }
    }
}

impl From<ReviewPackAssetV2> for ReviewPackAsset {
    fn from(value: ReviewPackAssetV2) -> Self {
        Self {
            logical_path: value.logical_path,
            content_sha256: value.content_sha256,
            size_bytes: value.size_bytes,
            media_type: value.media_type,
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineReviewResultEnvelopeV2 {
    #[serde(deserialize_with = "deserialize_version_2")]
    #[schemars(extend("const" = 2))]
    pub version: u32,
    pub result: OfflineReviewResultV2,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum OfflineReviewResultV2 {
    Accepted {
        #[schemars(length(min = 1, max = 128))]
        attempt_id: String,
        conflict_reason: (),
        #[schemars(length(min = 1, max = 128))]
        event_id: String,
        authoritative_state: OfflineReviewStateV1,
        session: OfflineReviewSessionV2,
        #[schemars(length(min = 1, max = 512))]
        cursor: String,
    },
    Conflict {
        #[schemars(length(min = 1, max = 128))]
        attempt_id: String,
        conflict_reason: OfflineReviewConflictReasonV2,
        event_id: (),
        authoritative_state: OfflineReviewStateV1,
        session: OfflineReviewSessionV2,
        #[schemars(length(min = 1, max = 512))]
        cursor: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflineReviewConflictReasonV2 {
    LeaseEnded,
    SessionAdvanced,
    ContentChanged,
    SchedulingChanged,
    CaptureMismatch,
}

impl From<OfflineConflictReason> for OfflineReviewConflictReasonV2 {
    fn from(value: OfflineConflictReason) -> Self {
        match value {
            OfflineConflictReason::LeaseEnded => Self::LeaseEnded,
            OfflineConflictReason::SessionAdvanced => Self::SessionAdvanced,
            OfflineConflictReason::ContentChanged => Self::ContentChanged,
            OfflineConflictReason::SchedulingChanged => Self::SchedulingChanged,
            OfflineConflictReason::CaptureMismatch => Self::CaptureMismatch,
        }
    }
}

impl From<OfflineReviewConflictReasonV2> for OfflineConflictReason {
    fn from(value: OfflineReviewConflictReasonV2) -> Self {
        match value {
            OfflineReviewConflictReasonV2::LeaseEnded => Self::LeaseEnded,
            OfflineReviewConflictReasonV2::SessionAdvanced => Self::SessionAdvanced,
            OfflineReviewConflictReasonV2::ContentChanged => Self::ContentChanged,
            OfflineReviewConflictReasonV2::SchedulingChanged => Self::SchedulingChanged,
            OfflineReviewConflictReasonV2::CaptureMismatch => Self::CaptureMismatch,
        }
    }
}

impl TryFrom<&OfflineReviewResult> for OfflineReviewResultV2 {
    type Error = serde_json::Error;

    fn try_from(value: &OfflineReviewResult) -> Result<Self, Self::Error> {
        let authoritative_state = (&value.authoritative_state).into();
        let session = OfflineReviewSessionV2::try_from(&value.session)?;
        let cursor = durable_cursor(&value.cursor_server_id, value.cursor, &value.cursor_token)?;
        match value.status {
            ReconciliationStatus::Accepted => Ok(Self::Accepted {
                attempt_id: value.attempt_id.clone(),
                conflict_reason: require_absent(value.conflict_reason, "accepted conflict")?,
                event_id: require_nonempty(value.event_id.clone(), "accepted event")?,
                authoritative_state,
                session,
                cursor,
            }),
            ReconciliationStatus::Conflict => Ok(Self::Conflict {
                attempt_id: value.attempt_id.clone(),
                conflict_reason: require_present(value.conflict_reason, "conflict reason")?.into(),
                event_id: require_absent(value.event_id.as_ref(), "conflict event")?,
                authoritative_state,
                session,
                cursor,
            }),
        }
    }
}

impl TryFrom<OfflineReviewResultV2> for OfflineReviewResult {
    type Error = OfflineReviewWireError;

    fn try_from(value: OfflineReviewResultV2) -> Result<Self, Self::Error> {
        let (attempt_id, status, conflict_reason, event_id, state, session, encoded_cursor) =
            match value {
                OfflineReviewResultV2::Accepted {
                    attempt_id,
                    event_id,
                    authoritative_state,
                    session,
                    cursor,
                    ..
                } => (
                    attempt_id,
                    ReconciliationStatus::Accepted,
                    None,
                    Some(event_id),
                    authoritative_state,
                    session,
                    cursor,
                ),
                OfflineReviewResultV2::Conflict {
                    attempt_id,
                    conflict_reason,
                    authoritative_state,
                    session,
                    cursor,
                    ..
                } => (
                    attempt_id,
                    ReconciliationStatus::Conflict,
                    Some(conflict_reason.into()),
                    None,
                    authoritative_state,
                    session,
                    cursor,
                ),
            };
        let (server_id, cursor, token) = decode_durable_cursor(&encoded_cursor)?;
        if attempt_id.is_empty() || event_id.as_ref().is_some_and(String::is_empty) {
            return Err(OfflineReviewWireError::Operation);
        }
        let authoritative_state: ReviewStateSnapshot = state.into();
        crate::domain::validate_state(&authoritative_state.clone().into())
            .map_err(|_| OfflineReviewWireError::Operation)?;
        let session = ReviewSessionView::try_from(session)?;
        if session.mode != ReviewSessionMode::Offline
            || session
                .owner_device_id
                .as_ref()
                .is_none_or(String::is_empty)
        {
            return Err(OfflineReviewWireError::Operation);
        }
        Ok(Self {
            format_version: OFFLINE_REVIEW_VERSION,
            attempt_id,
            status,
            conflict_reason,
            event_id,
            authoritative_state,
            session,
            cursor,
            cursor_server_id: server_id,
            cursor_token: token,
        })
    }
}

fn decode_durable_cursor(value: &str) -> Result<(String, u64, String), OfflineReviewWireError> {
    crate::protocol::replica::decode_cursor(value).map_err(|_| OfflineReviewWireError::Operation)
}

fn durable_cursor(
    server_id: &str,
    position: u64,
    token: &str,
) -> Result<String, serde_json::Error> {
    encode_cursor(server_id, position, token)
        .map_err(|error| serde_json::Error::io(io::Error::other(error)))
}

fn invalid_response(message: &'static str) -> serde_json::Error {
    serde_json::Error::io(io::Error::new(io::ErrorKind::InvalidData, message))
}

fn require_present<T>(value: Option<T>, message: &'static str) -> Result<T, serde_json::Error> {
    value.ok_or_else(|| invalid_response(message))
}

fn require_nonempty(
    value: Option<String>,
    message: &'static str,
) -> Result<String, serde_json::Error> {
    require_present(value, message).and_then(|value| {
        (!value.is_empty())
            .then_some(value)
            .ok_or_else(|| invalid_response(message))
    })
}

fn require_absent<T>(value: Option<T>, message: &'static str) -> Result<(), serde_json::Error> {
    if value.is_none() {
        Ok(())
    } else {
        Err(invalid_response(message))
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineReviewSessionV2 {
    #[schemars(length(min = 1, max = 128))]
    pub session_id: String,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub owner_device_id: Option<String>,
    pub mode: OfflineReviewSessionModeV2,
    pub source: OfflineReviewSourceV2,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub target_count: Option<u32>,
    pub status: OfflineReviewSessionStatusV2,
    pub current_position: u32,
    pub queue_length: u32,
    #[schemars(range(min = 1))]
    pub started_at_us: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflineReviewSessionModeV2 {
    Online,
    Offline,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflineReviewSessionStatusV2 {
    Active,
    Completed,
    Quit,
    Aborted,
}

impl OfflineReviewSessionStatusV2 {
    fn parse(value: &str) -> Result<Self, serde_json::Error> {
        match value {
            "active" => Ok(Self::Active),
            "completed" => Ok(Self::Completed),
            "quit" => Ok(Self::Quit),
            "aborted" => Ok(Self::Aborted),
            _ => Err(invalid_response("review session status is invalid")),
        }
    }

    const fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Completed => "completed",
            Self::Quit => "quit",
            Self::Aborted => "aborted",
        }
    }
}

impl From<ReviewSessionMode> for OfflineReviewSessionModeV2 {
    fn from(value: ReviewSessionMode) -> Self {
        match value {
            ReviewSessionMode::Online => Self::Online,
            ReviewSessionMode::Offline => Self::Offline,
        }
    }
}

impl From<OfflineReviewSessionModeV2> for ReviewSessionMode {
    fn from(value: OfflineReviewSessionModeV2) -> Self {
        match value {
            OfflineReviewSessionModeV2::Online => Self::Online,
            OfflineReviewSessionModeV2::Offline => Self::Offline,
        }
    }
}

impl TryFrom<&ReviewSessionView> for OfflineReviewSessionV2 {
    type Error = serde_json::Error;

    fn try_from(value: &ReviewSessionView) -> Result<Self, Self::Error> {
        Ok(Self {
            session_id: value.session_id.clone(),
            owner_device_id: value.owner_device_id.clone(),
            mode: value.mode.into(),
            source: (&value.source).into(),
            target_count: value.target_count,
            status: OfflineReviewSessionStatusV2::parse(&value.status)?,
            current_position: value.current_position,
            queue_length: value.queue_length,
            started_at_us: value.started_at_us,
        })
    }
}

impl TryFrom<OfflineReviewSessionV2> for ReviewSessionView {
    type Error = OfflineReviewWireError;

    fn try_from(value: OfflineReviewSessionV2) -> Result<Self, Self::Error> {
        let coherent_position = match value.status {
            OfflineReviewSessionStatusV2::Active => value.current_position < value.queue_length,
            OfflineReviewSessionStatusV2::Completed => value.current_position == value.queue_length,
            OfflineReviewSessionStatusV2::Aborted | OfflineReviewSessionStatusV2::Quit => {
                value.current_position <= value.queue_length
            }
        };
        if value.session_id.is_empty()
            || !coherent_position
            || value.started_at_us == 0
            || value.target_count == Some(0)
        {
            return Err(OfflineReviewWireError::Operation);
        }
        Ok(Self {
            session_id: value.session_id,
            owner_device_id: value.owner_device_id,
            mode: value.mode.into(),
            source: ReviewSource::try_from(value.source)?,
            target_count: value.target_count,
            status: value.status.as_str().into(),
            current_position: value.current_position,
            queue_length: value.queue_length,
            started_at_us: value.started_at_us,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OfflineReviewSourceV2 {
    pub kind: OfflineReviewSourceKindV2,
    pub tags: Vec<String>,
    pub exclude_tags: Vec<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub topic_node_id: Option<String>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub topic_forward_depth: Option<u32>,
    #[serde(deserialize_with = "deserialize_required_option")]
    #[schemars(required)]
    pub topic_backward_depth: Option<u32>,
    pub manual_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OfflineReviewSourceKindV2 {
    Due,
    Overdue,
    DueWithoutOverdue,
    Tag,
    DueTag,
    Topic,
    Manual,
}

impl From<&crate::application::ReviewSourceKind> for OfflineReviewSourceKindV2 {
    fn from(value: &crate::application::ReviewSourceKind) -> Self {
        use crate::application::ReviewSourceKind;
        match value {
            ReviewSourceKind::Due => Self::Due,
            ReviewSourceKind::Overdue => Self::Overdue,
            ReviewSourceKind::DueWithoutOverdue => Self::DueWithoutOverdue,
            ReviewSourceKind::Tag => Self::Tag,
            ReviewSourceKind::DueTag => Self::DueTag,
            ReviewSourceKind::Topic => Self::Topic,
            ReviewSourceKind::Manual => Self::Manual,
        }
    }
}

impl From<OfflineReviewSourceKindV2> for crate::application::ReviewSourceKind {
    fn from(value: OfflineReviewSourceKindV2) -> Self {
        use crate::application::ReviewSourceKind;
        match value {
            OfflineReviewSourceKindV2::Due => ReviewSourceKind::Due,
            OfflineReviewSourceKindV2::Overdue => ReviewSourceKind::Overdue,
            OfflineReviewSourceKindV2::DueWithoutOverdue => ReviewSourceKind::DueWithoutOverdue,
            OfflineReviewSourceKindV2::Tag => ReviewSourceKind::Tag,
            OfflineReviewSourceKindV2::DueTag => ReviewSourceKind::DueTag,
            OfflineReviewSourceKindV2::Topic => ReviewSourceKind::Topic,
            OfflineReviewSourceKindV2::Manual => ReviewSourceKind::Manual,
        }
    }
}

impl From<&ReviewSource> for OfflineReviewSourceV2 {
    fn from(value: &ReviewSource) -> Self {
        Self {
            kind: (&value.kind).into(),
            tags: value.tags.clone(),
            exclude_tags: value.exclude_tags.clone(),
            topic_node_id: value.topic_node_id.clone(),
            topic_forward_depth: value.topic_forward_depth,
            topic_backward_depth: value.topic_backward_depth,
            manual_ids: value.manual_ids.clone(),
        }
    }
}

impl TryFrom<OfflineReviewSourceV2> for ReviewSource {
    type Error = OfflineReviewWireError;

    fn try_from(value: OfflineReviewSourceV2) -> Result<Self, Self::Error> {
        ReviewSource {
            kind: value.kind.into(),
            tags: value.tags,
            exclude_tags: value.exclude_tags,
            topic_node_id: value.topic_node_id,
            topic_forward_depth: value.topic_forward_depth,
            topic_backward_depth: value.topic_backward_depth,
            manual_ids: value.manual_ids,
        }
        .validate()
        .map_err(|_| OfflineReviewWireError::Operation)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sha2::{Digest as _, Sha256};

    use super::{
        OfflineReviewSubmission, OfflineReviewWireError, decode_offline_result, decode_review_pack,
        offline_result_json, review_pack_json,
    };
    use crate::application::{OfflineReviewResult, ReviewPackView};

    #[test]
    fn rejects_unknown_versions_before_decoding_the_operation() {
        let submission: OfflineReviewSubmission = serde_json::from_value(json!({
            "version": 2,
            "operation": {}
        }))
        .unwrap();

        assert!(matches!(
            submission.decode(),
            Err(OfflineReviewWireError::Version)
        ));
    }

    #[test]
    fn rejects_unknown_envelope_fields() {
        assert!(
            serde_json::from_value::<OfflineReviewSubmission>(json!({
                "version": 1,
                "operation": {},
                "future": true
            }))
            .is_err()
        );
    }

    #[test]
    fn response_decoders_classify_unknown_versions() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/protocol/offline-review-v1.json"
        ))
        .unwrap();
        let mut pack = fixture["pack"].clone();
        pack["version"] = json!(1);
        assert!(matches!(
            decode_review_pack(pack),
            Err(OfflineReviewWireError::Version)
        ));
        let mut result = fixture["result"].clone();
        result["version"] = json!(1);
        assert!(matches!(
            decode_offline_result(result),
            Err(OfflineReviewWireError::Version)
        ));
    }

    #[test]
    fn v1_submission_and_v2_responses_match_the_golden_fixture() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/protocol/offline-review-v1.json"
        ))
        .unwrap();
        let submission: OfflineReviewSubmission =
            serde_json::from_value(fixture["submission"].clone()).unwrap();
        let decoded = submission.decode().unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&decoded.canonical_payload).unwrap(),
            fixture["submission"]
        );
        assert_eq!(
            format!("{:x}", Sha256::digest(decoded.canonical_payload.as_bytes())),
            "41eb0a1da2be6f8651957083f4df4d32ff7bd2fcee24536d274b59beaaf6da82"
        );

        let mut pack = fixture["pack"]["pack"].clone();
        pack["cursor"] = json!(4);
        pack["cursor_server_id"] = json!("0123456789abcdef0123456789abcdef");
        pack["cursor_token"] = json!("11111111111111111111111111111111");
        pack["format_version"] = json!(1);
        let pack: ReviewPackView = serde_json::from_value(pack).unwrap();
        assert_eq!(review_pack_json(&pack).unwrap(), fixture["pack"]);

        let mut result = fixture["result"]["result"].clone();
        result["cursor"] = json!(5);
        result["cursor_server_id"] = json!("0123456789abcdef0123456789abcdef");
        result["cursor_token"] = json!("22222222222222222222222222222222");
        result["format_version"] = json!(1);
        let result: OfflineReviewResult = serde_json::from_value(result).unwrap();
        assert_eq!(offline_result_json(&result).unwrap(), fixture["result"]);
    }
}
