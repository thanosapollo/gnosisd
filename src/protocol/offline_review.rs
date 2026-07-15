use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::application::{
    AnswerInput, OfflineReviewOperation, OfflineReviewResult, ParametersSnapshot,
    PresentationSnapshot, ReviewPackAsset, ReviewPackItem, ReviewPackView, ReviewSessionMode,
    ReviewSessionView, ReviewSource, ReviewStateSnapshot, TransitionSnapshot,
};
use crate::protocol::replica::encode_cursor;

pub(crate) const OFFLINE_REVIEW_VERSION: u32 = 1;
const OFFLINE_REVIEW_RESPONSE_VERSION: u32 = 2;

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

#[cfg(test)]
pub(crate) fn canonical_offline_operation(
    version: u32,
    operation: &OfflineReviewOperation,
) -> Result<String, OfflineReviewWireError> {
    if version != OFFLINE_REVIEW_VERSION {
        return Err(OfflineReviewWireError::Version);
    }
    encode_v1_operation(&OfflineReviewOperationV1::from(operation))
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

pub(crate) fn review_pack_json(pack: &ReviewPackView) -> Result<Value, serde_json::Error> {
    serde_json::to_value(ReviewPackEnvelopeV1 {
        version: OFFLINE_REVIEW_RESPONSE_VERSION,
        pack: ReviewPackV1::try_from(pack)?,
    })
}

pub(crate) fn active_review_pack_json(
    _version: u32,
    pack: Option<&ReviewPackView>,
) -> Result<Value, serde_json::Error> {
    serde_json::to_value(ActiveReviewPackEnvelopeV1 {
        version: OFFLINE_REVIEW_RESPONSE_VERSION,
        pack: pack.map(ReviewPackV1::try_from).transpose()?,
    })
}

pub(crate) fn offline_result_json(
    result: &OfflineReviewResult,
) -> Result<Value, serde_json::Error> {
    serde_json::to_value(OfflineResultEnvelopeV1 {
        version: OFFLINE_REVIEW_RESPONSE_VERSION,
        result: OfflineResultV1::try_from(result)?,
    })
}

#[derive(Serialize)]
struct VersionedOperationV1<'a> {
    version: u32,
    operation: &'a OfflineReviewOperationV1,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OfflineReviewOperationV1 {
    attempt_id: String,
    pack_id: String,
    session_id: String,
    lease_id: String,
    device_id: String,
    device_sequence: u64,
    session_position: u32,
    thema_id: String,
    thema_revision: u64,
    base_state_revision: u64,
    content_sha256: String,
    presentation: PresentationV1,
    parameters: ParametersV1,
    matcher_version: u32,
    response: AnswerV1,
    detected_success: bool,
    accepted_success: bool,
    override_count: u32,
    proposed_transition: TransitionV1,
    desired_suspended: bool,
    answered_at_us: u64,
    fuzz_sample: f64,
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

impl From<&OfflineReviewOperation> for OfflineReviewOperationV1 {
    fn from(value: &OfflineReviewOperation) -> Self {
        Self {
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
            presentation: (&value.presentation).into(),
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
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PresentationV1 {
    kind: String,
    keimenon: String,
    hypothesis: Vec<Option<String>>,
    answer: Vec<String>,
    source_guid: Option<String>,
    parathema: Option<String>,
    review_image: Option<String>,
    tags: Vec<String>,
}

impl From<PresentationV1> for PresentationSnapshot {
    fn from(value: PresentationV1) -> Self {
        Self {
            kind: value.kind,
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

impl From<&PresentationSnapshot> for PresentationV1 {
    fn from(value: &PresentationSnapshot) -> Self {
        Self {
            kind: value.kind.clone(),
            keimenon: value.keimenon.clone(),
            hypothesis: value.hypothesis.clone(),
            answer: value.answer.clone(),
            source_guid: value.source_guid.clone(),
            parathema: value.parathema.clone(),
            review_image: value.review_image.clone(),
            tags: value.tags.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewStateV1 {
    state_revision: u64,
    review_count: u64,
    legacy_review_count: u64,
    legacy_unclassified_count: u64,
    last_reviewed_at_us: Option<u64>,
    last_review_day: Option<u32>,
    next_review_day: u32,
    interval_days: Option<u32>,
    consecutive_successes: u64,
    total_successes: u64,
    consecutive_failures: u64,
    total_failures: u64,
    suspended: bool,
    gnosis_plus: f64,
    gnosis_minus: f64,
    gnosis_synolon: f64,
}

impl From<ReviewStateV1> for ReviewStateSnapshot {
    fn from(value: ReviewStateV1) -> Self {
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

impl From<&ReviewStateSnapshot> for ReviewStateV1 {
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParametersV1 {
    scheduler_version: u32,
    amnesia: f64,
    epignosis: f64,
    agnoia: f64,
    anagnosis: u64,
    lethe: u64,
    synolon_max: f64,
    interval_fuzz: f64,
    proto: Vec<u32>,
}

impl From<ParametersV1> for ParametersSnapshot {
    fn from(value: ParametersV1) -> Self {
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

impl From<&ParametersSnapshot> for ParametersV1 {
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum AnswerV1 {
    Text(String),
    Texts(Vec<String>),
    Choice(String),
}

impl From<AnswerV1> for AnswerInput {
    fn from(value: AnswerV1) -> Self {
        match value {
            AnswerV1::Text(value) => Self::Text(value),
            AnswerV1::Texts(value) => Self::Texts(value),
            AnswerV1::Choice(value) => Self::Choice(value),
        }
    }
}

impl From<&AnswerInput> for AnswerV1 {
    fn from(value: &AnswerInput) -> Self {
        match value {
            AnswerInput::Text(value) => Self::Text(value.clone()),
            AnswerInput::Texts(value) => Self::Texts(value.clone()),
            AnswerInput::Choice(value) => Self::Choice(value.clone()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransitionV1 {
    before: ReviewStateV1,
    after: ReviewStateV1,
    parameters: ParametersV1,
    reviewed_at_us: u64,
    review_day: u32,
    success: bool,
    first_review: bool,
    elapsed_days: u32,
    algorithm_interval_days: f64,
    computed_interval_days: u32,
    fuzz_delta: f64,
}

impl From<TransitionV1> for TransitionSnapshot {
    fn from(value: TransitionV1) -> Self {
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

impl From<&TransitionSnapshot> for TransitionV1 {
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

#[derive(Serialize)]
struct ReviewPackEnvelopeV1 {
    version: u32,
    pack: ReviewPackV1,
}

#[derive(Serialize)]
struct ActiveReviewPackEnvelopeV1 {
    version: u32,
    pack: Option<ReviewPackV1>,
}

#[derive(Serialize)]
struct ReviewPackV1 {
    pack_id: String,
    session_id: String,
    device_id: String,
    lease_id: String,
    status: String,
    cursor: String,
    issued_at_us: u64,
    current_position: u32,
    timezone: String,
    day_boundary_hour: u8,
    items: Vec<ReviewPackItemV1>,
    assets: Vec<ReviewPackAssetV1>,
}

impl TryFrom<&ReviewPackView> for ReviewPackV1 {
    type Error = serde_json::Error;

    fn try_from(value: &ReviewPackView) -> Result<Self, Self::Error> {
        Ok(Self {
            pack_id: value.pack_id.clone(),
            session_id: value.session_id.clone(),
            device_id: value.device_id.clone(),
            lease_id: value.lease_id.clone(),
            status: value.status.clone(),
            cursor: durable_cursor(&value.cursor_server_id, value.cursor, &value.cursor_token)?,
            issued_at_us: value.issued_at_us,
            current_position: value.current_position,
            timezone: value.timezone.clone(),
            day_boundary_hour: value.day_boundary_hour,
            items: value.items.iter().map(ReviewPackItemV1::from).collect(),
            assets: value.assets.iter().map(ReviewPackAssetV1::from).collect(),
        })
    }
}

#[derive(Serialize)]
struct ReviewPackItemV1 {
    position: u32,
    thema_id: String,
    thema_revision: u64,
    content_sha256: String,
    presentation: PresentationV1,
    review_state: ReviewStateV1,
    parameters: ParametersV1,
    matcher_version: u32,
    asset_paths: Vec<String>,
}

impl From<&ReviewPackItem> for ReviewPackItemV1 {
    fn from(value: &ReviewPackItem) -> Self {
        Self {
            position: value.position,
            thema_id: value.thema_id.clone(),
            thema_revision: value.thema_revision,
            content_sha256: value.content_sha256.clone(),
            presentation: (&value.presentation).into(),
            review_state: (&value.review_state).into(),
            parameters: (&value.parameters).into(),
            matcher_version: value.matcher_version,
            asset_paths: value.asset_paths.clone(),
        }
    }
}

#[derive(Serialize)]
struct ReviewPackAssetV1 {
    logical_path: String,
    content_sha256: String,
    size_bytes: u64,
    media_type: String,
}

impl From<&ReviewPackAsset> for ReviewPackAssetV1 {
    fn from(value: &ReviewPackAsset) -> Self {
        Self {
            logical_path: value.logical_path.clone(),
            content_sha256: value.content_sha256.clone(),
            size_bytes: value.size_bytes,
            media_type: value.media_type.clone(),
        }
    }
}

#[derive(Serialize)]
struct OfflineResultEnvelopeV1 {
    version: u32,
    result: OfflineResultV1,
}

#[derive(Serialize)]
struct OfflineResultV1 {
    attempt_id: String,
    status: String,
    conflict_reason: Option<String>,
    event_id: Option<String>,
    authoritative_state: ReviewStateV1,
    session: ReviewSessionV1,
    cursor: String,
}

impl TryFrom<&OfflineReviewResult> for OfflineResultV1 {
    type Error = serde_json::Error;

    fn try_from(value: &OfflineReviewResult) -> Result<Self, Self::Error> {
        Ok(Self {
            attempt_id: value.attempt_id.clone(),
            status: value.status.as_str().into(),
            conflict_reason: value.conflict_reason.map(|reason| reason.as_str().into()),
            event_id: value.event_id.clone(),
            authoritative_state: (&value.authoritative_state).into(),
            session: (&value.session).into(),
            cursor: durable_cursor(&value.cursor_server_id, value.cursor, &value.cursor_token)?,
        })
    }
}

fn durable_cursor(
    server_id: &str,
    position: u64,
    token: &str,
) -> Result<String, serde_json::Error> {
    encode_cursor(server_id, position, token)
        .map_err(|error| serde_json::Error::io(io::Error::other(error)))
}

#[derive(Serialize)]
struct ReviewSessionV1 {
    session_id: String,
    owner_device_id: Option<String>,
    mode: String,
    source: ReviewSourceV1,
    target_count: Option<u32>,
    status: String,
    current_position: u32,
    queue_length: u32,
    started_at_us: u64,
}

impl From<&ReviewSessionView> for ReviewSessionV1 {
    fn from(value: &ReviewSessionView) -> Self {
        Self {
            session_id: value.session_id.clone(),
            owner_device_id: value.owner_device_id.clone(),
            mode: match value.mode {
                ReviewSessionMode::Online => "online",
                ReviewSessionMode::Offline => "offline",
            }
            .into(),
            source: (&value.source).into(),
            target_count: value.target_count,
            status: value.status.clone(),
            current_position: value.current_position,
            queue_length: value.queue_length,
            started_at_us: value.started_at_us,
        }
    }
}

#[derive(Serialize)]
struct ReviewSourceV1 {
    kind: String,
    tags: Vec<String>,
    exclude_tags: Vec<String>,
    topic_node_id: Option<String>,
    topic_forward_depth: Option<u32>,
    topic_backward_depth: Option<u32>,
    manual_ids: Vec<String>,
}

impl From<&ReviewSource> for ReviewSourceV1 {
    fn from(value: &ReviewSource) -> Self {
        Self {
            kind: value.kind.as_str().into(),
            tags: value.tags.clone(),
            exclude_tags: value.exclude_tags.clone(),
            topic_node_id: value.topic_node_id.clone(),
            topic_forward_depth: value.topic_forward_depth,
            topic_backward_depth: value.topic_backward_depth,
            manual_ids: value.manual_ids.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use sha2::{Digest as _, Sha256};

    use super::{
        OfflineReviewSubmission, OfflineReviewWireError, offline_result_json, review_pack_json,
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
