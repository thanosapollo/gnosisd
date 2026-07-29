use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, Duration, Utc};
use chrono_tz::Tz;
use regex::RegexBuilder;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use unicode_script::{Script, UnicodeScript};

use crate::domain::fsrs::{
    FsrsAlgorithm, FsrsConfigSnapshot as DomainFsrsConfigSnapshot, FsrsImplementation, FsrsModel,
    RecallOutcome,
};
use crate::domain::{
    ReviewState, StructuredResponse, ThemaContent, ThemaKind, Transition, compute_transition, grade,
};
use crate::is_canonical_sha256;

use super::{
    MAX_OFFLINE_ASSET_BYTES, MAX_REVIEW_PACK_ID_CHARS, MAX_REVIEW_PACK_ITEMS,
    MAX_REVIEW_PACK_MEDIA_TYPE_CHARS, validate_asset_path,
};

const MAX_TOPIC_DEPTH: u32 = 32;
const MAX_REVIEW_OPERATION_ID_BYTES: usize = 128;
const MAX_PERSISTED_REVISION: u64 = i64::MAX as u64;
pub(crate) const MAX_REVIEW_PACK_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewSourceKind {
    Due,
    Overdue,
    DueWithoutOverdue,
    Tag,
    DueTag,
    Topic,
    Manual,
}

impl ReviewSourceKind {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Due => "due",
            Self::Overdue => "overdue",
            Self::DueWithoutOverdue => "due_without_overdue",
            Self::Tag => "tag",
            Self::DueTag => "due_tag",
            Self::Topic => "topic",
            Self::Manual => "manual",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewSource {
    pub(crate) kind: ReviewSourceKind,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default)]
    pub(crate) exclude_tags: Vec<String>,
    pub(crate) topic_node_id: Option<String>,
    pub(crate) topic_forward_depth: Option<u32>,
    pub(crate) topic_backward_depth: Option<u32>,
    #[serde(default)]
    pub(crate) manual_ids: Vec<String>,
}

impl ReviewSource {
    pub(crate) fn validate(self) -> Result<Self, ReviewError> {
        let tags_are_valid = self
            .tags
            .iter()
            .chain(&self.exclude_tags)
            .all(|tag| !tag.trim().is_empty());
        let has_tag_filter = !self.tags.is_empty() || !self.exclude_tags.is_empty();
        let valid = match self.kind {
            ReviewSourceKind::Due
            | ReviewSourceKind::Overdue
            | ReviewSourceKind::DueWithoutOverdue => {
                self.tags.is_empty()
                    && self.exclude_tags.is_empty()
                    && self.topic_node_id.is_none()
                    && self.topic_forward_depth.is_none()
                    && self.topic_backward_depth.is_none()
                    && self.manual_ids.is_empty()
            }
            ReviewSourceKind::Tag | ReviewSourceKind::DueTag => {
                tags_are_valid
                    && has_tag_filter
                    && self.topic_node_id.is_none()
                    && self.topic_forward_depth.is_none()
                    && self.topic_backward_depth.is_none()
                    && self.manual_ids.is_empty()
            }
            ReviewSourceKind::Topic => {
                self.tags.is_empty()
                    && self.exclude_tags.is_empty()
                    && self.topic_node_id.as_ref().is_some_and(|id| !id.is_empty())
                    && self
                        .topic_forward_depth
                        .is_some_and(|depth| depth <= MAX_TOPIC_DEPTH)
                    && self
                        .topic_backward_depth
                        .is_some_and(|depth| depth <= MAX_TOPIC_DEPTH)
                    && self.manual_ids.is_empty()
            }
            ReviewSourceKind::Manual => {
                self.tags.is_empty()
                    && self.exclude_tags.is_empty()
                    && self.topic_node_id.is_none()
                    && self.topic_forward_depth.is_none()
                    && self.topic_backward_depth.is_none()
                    && !self.manual_ids.is_empty()
                    && self.manual_ids.len() <= MAX_REVIEW_PACK_ITEMS
                    && self.manual_ids.iter().all(|id| !id.is_empty())
            }
        };
        if valid {
            Ok(self)
        } else {
            Err(ReviewError::Source)
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct ReviewSessionView {
    pub(crate) session_id: String,
    pub(crate) owner_device_id: Option<String>,
    pub(crate) mode: ReviewSessionMode,
    pub(crate) source: ReviewSource,
    pub(crate) target_count: Option<u32>,
    pub(crate) status: String,
    pub(crate) current_position: u32,
    pub(crate) queue_length: u32,
    pub(crate) started_at_us: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewSessionMode {
    Online,
    Offline,
}

impl ReviewSessionMode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Offline => "offline",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PresentationSnapshot {
    pub(crate) kind: String,
    pub(crate) keimenon: String,
    pub(crate) hypothesis: Vec<Option<String>>,
    pub(crate) answer: Vec<String>,
    pub(crate) source_guid: Option<String>,
    pub(crate) parathema: Option<String>,
    pub(crate) review_image: Option<String>,
    pub(crate) tags: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReviewQuestion {
    pub(crate) text: String,
    pub(crate) response: ReviewResponsePrompt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReviewClozeCheck {
    pub(crate) matches: Vec<bool>,
    pub(crate) all_entered_correct: bool,
    pub(crate) complete: bool,
    pub(crate) question: String,
    pub(crate) next_input_script: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReviewResponsePrompt {
    Text { input_script: Option<String> },
    Texts { input_scripts: Vec<Option<String>> },
    Choice { choices: Vec<String> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewStateSnapshot {
    pub(crate) state_revision: u64,
    pub(crate) review_count: u64,
    pub(crate) last_reviewed_at_us: Option<u64>,
    pub(crate) last_review_day: Option<u32>,
    pub(crate) next_review_day: u32,
    pub(crate) interval_days: Option<u32>,
    pub(crate) total_successes: u64,
    pub(crate) total_failures: u64,
    pub(crate) suspended: bool,
    pub(crate) stability: Option<f32>,
    pub(crate) difficulty: Option<f32>,
}

impl From<ReviewStateSnapshot> for ReviewState {
    fn from(value: ReviewStateSnapshot) -> Self {
        Self {
            state_revision: value.state_revision,
            review_count: value.review_count,
            last_reviewed_at_us: value.last_reviewed_at_us,
            last_review_day: value.last_review_day,
            next_review_day: value.next_review_day,
            interval_days: value.interval_days,
            total_successes: value.total_successes,
            total_failures: value.total_failures,
            suspended: value.suspended,
            stability: value.stability,
            difficulty: value.difficulty,
        }
    }
}

impl From<&ReviewState> for ReviewStateSnapshot {
    fn from(value: &ReviewState) -> Self {
        Self {
            state_revision: value.state_revision,
            review_count: value.review_count,
            last_reviewed_at_us: value.last_reviewed_at_us,
            last_review_day: value.last_review_day,
            next_review_day: value.next_review_day,
            interval_days: value.interval_days,
            total_successes: value.total_successes,
            total_failures: value.total_failures,
            suspended: value.suspended,
            stability: value.stability,
            difficulty: value.difficulty,
        }
    }
}

impl Default for ReviewStateSnapshot {
    fn default() -> Self {
        Self::from(&ReviewState::default())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FsrsConfigSnapshot {
    pub(crate) algorithm: FsrsAlgorithmSnapshot,
    pub(crate) model: FsrsModelSnapshot,
    pub(crate) implementation: FsrsImplementationSnapshot,
    pub(crate) desired_retention: f32,
    pub(crate) parameters: [f32; 21],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FsrsAlgorithmSnapshot {
    Fsrs,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FsrsModelSnapshot {
    #[serde(rename = "fsrs6")]
    Six,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum FsrsImplementationSnapshot {
    #[serde(rename = "fsrs-6.6.1")]
    Crate6Dot6Dot1,
}

impl From<FsrsConfigSnapshot> for DomainFsrsConfigSnapshot {
    fn from(value: FsrsConfigSnapshot) -> Self {
        Self {
            algorithm: FsrsAlgorithm::Fsrs,
            model: FsrsModel::Six,
            implementation: FsrsImplementation::Crate6Dot6Dot1,
            desired_retention: value.desired_retention,
            parameters: value.parameters,
        }
    }
}

impl From<&DomainFsrsConfigSnapshot> for FsrsConfigSnapshot {
    fn from(value: &DomainFsrsConfigSnapshot) -> Self {
        Self {
            algorithm: FsrsAlgorithmSnapshot::Fsrs,
            model: FsrsModelSnapshot::Six,
            implementation: FsrsImplementationSnapshot::Crate6Dot6Dot1,
            desired_retention: value.desired_retention,
            parameters: value.parameters,
        }
    }
}

impl FsrsConfigSnapshot {
    pub(crate) fn valid(&self) -> bool {
        crate::domain::fsrs::validate_config(&self.clone().into()).is_ok()
    }
}

impl Default for FsrsConfigSnapshot {
    fn default() -> Self {
        Self::from(&DomainFsrsConfigSnapshot::default())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub(crate) enum AnswerInput {
    Text(String),
    Texts(Vec<String>),
    Choice(String),
}

impl From<AnswerInput> for StructuredResponse {
    fn from(value: AnswerInput) -> Self {
        match value {
            AnswerInput::Text(value) => Self::Text(value),
            AnswerInput::Texts(value) => Self::Texts(value),
            AnswerInput::Choice(value) => Self::Choice(value),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TransitionSnapshot {
    pub(crate) before: ReviewStateSnapshot,
    pub(crate) after: ReviewStateSnapshot,
    pub(crate) config: FsrsConfigSnapshot,
    pub(crate) reviewed_at_us: u64,
    pub(crate) review_day: u32,
    pub(crate) outcome: RecallOutcomeSnapshot,
    pub(crate) first_review: bool,
    pub(crate) elapsed_days: u32,
    pub(crate) raw_interval_days: f32,
    pub(crate) interval_days: u32,
    pub(crate) due_day: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecallOutcomeSnapshot {
    Failure,
    Success,
}

impl From<&Transition> for TransitionSnapshot {
    fn from(value: &Transition) -> Self {
        Self {
            before: (&value.before).into(),
            after: (&value.after).into(),
            config: (&value.config).into(),
            reviewed_at_us: value.reviewed_at_us,
            review_day: value.review_day,
            outcome: match value.outcome {
                RecallOutcome::Failure => RecallOutcomeSnapshot::Failure,
                RecallOutcome::Success => RecallOutcomeSnapshot::Success,
            },
            first_review: value.first_review,
            elapsed_days: value.elapsed_days,
            raw_interval_days: value.raw_interval_days,
            interval_days: value.interval_days,
            due_day: value.due_day,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewAttemptView {
    pub(crate) attempt_id: String,
    pub(crate) session_id: String,
    pub(crate) thema_id: String,
    pub(crate) presentation: PresentationSnapshot,
    pub(crate) matcher_version: u32,
    pub(crate) response: Option<AnswerInput>,
    pub(crate) detected_success: Option<bool>,
    pub(crate) accepted_success: Option<bool>,
    pub(crate) override_count: u32,
    pub(crate) proposed_transition: Option<TransitionSnapshot>,
    pub(crate) desired_suspended: bool,
    pub(crate) status: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewAttemptMutation {
    AcceptedSuccess(bool),
    Suspension(bool),
}

impl ReviewAttemptMutation {
    pub(crate) const fn kind(self) -> &'static str {
        match self {
            Self::AcceptedSuccess(_) => "accepted_success_set",
            Self::Suspension(_) => "suspension_set",
        }
    }

    pub(crate) const fn desired_value(self) -> bool {
        match self {
            Self::AcceptedSuccess(value) | Self::Suspension(value) => value,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReviewAttemptOperation {
    pub(crate) format_version: u32,
    pub(crate) operation_id: String,
    pub(crate) attempt_id: String,
    pub(crate) expected_attempt_revision: u64,
    pub(crate) mutation: ReviewAttemptMutation,
}

impl ReviewAttemptOperation {
    pub(crate) fn new(
        format_version: u32,
        operation_id: String,
        attempt_id: String,
        expected_attempt_revision: u64,
        mutation: ReviewAttemptMutation,
    ) -> Result<Self, ReviewError> {
        if format_version != 1
            || !valid_operation_id(&operation_id)
            || !valid_review_id(&attempt_id)
            || expected_attempt_revision > MAX_PERSISTED_REVISION
        {
            return Err(ReviewError::Operation);
        }
        Ok(Self {
            format_version,
            operation_id,
            attempt_id,
            expected_attempt_revision,
            mutation,
        })
    }
}

pub(crate) fn valid_operation_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value.len() <= MAX_REVIEW_OPERATION_ID_BYTES
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || b"._:-".contains(&byte))
}

fn valid_review_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_REVIEW_OPERATION_ID_BYTES
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ReviewOperationAuthority {
    DeviceOwner(String),
    DeviceAdministrator(String),
}

impl ReviewOperationAuthority {
    pub(crate) fn device(device_id: String, administrative: bool) -> Self {
        if administrative {
            Self::DeviceAdministrator(device_id)
        } else {
            Self::DeviceOwner(device_id)
        }
    }

    pub(crate) fn device_id(&self) -> &str {
        match self {
            Self::DeviceOwner(device_id) | Self::DeviceAdministrator(device_id) => device_id,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReviewAttemptOperationStatus {
    Accepted,
    RevisionConflict,
    StaleAttempt,
    StateConflict,
}

impl ReviewAttemptOperationStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::RevisionConflict => "revision_conflict",
            Self::StaleAttempt => "stale_attempt",
            Self::StateConflict => "state_conflict",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewAttemptOperationResult {
    pub(crate) format_version: u32,
    pub(crate) operation_id: String,
    pub(crate) status: ReviewAttemptOperationStatus,
    pub(crate) expected_attempt_revision: u64,
    pub(crate) attempt_revision: u64,
    pub(crate) changed: bool,
    pub(crate) attempt: ReviewAttemptView,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReviewAttemptState {
    pub(crate) attempt_revision: u64,
    pub(crate) attempt: ReviewAttemptView,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewPackAsset {
    pub(crate) logical_path: String,
    pub(crate) content_sha256: String,
    pub(crate) size_bytes: u64,
    pub(crate) media_type: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewPackItem {
    pub(crate) position: u32,
    pub(crate) thema_id: String,
    pub(crate) thema_revision: u64,
    pub(crate) content_sha256: String,
    pub(crate) presentation: PresentationSnapshot,
    pub(crate) review_state: ReviewStateSnapshot,
    pub(crate) config: FsrsConfigSnapshot,
    pub(crate) matcher_version: u32,
    pub(crate) asset_paths: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewPackView {
    pub(crate) format_version: u32,
    pub(crate) pack_id: String,
    pub(crate) session_id: String,
    pub(crate) device_id: String,
    pub(crate) lease_id: String,
    pub(crate) status: String,
    pub(crate) cursor: u64,
    pub(crate) cursor_server_id: String,
    pub(crate) cursor_token: String,
    pub(crate) issued_at_us: u64,
    pub(crate) current_position: u32,
    pub(crate) timezone: String,
    pub(crate) day_boundary_hour: u8,
    pub(crate) items: Vec<ReviewPackItem>,
    pub(crate) assets: Vec<ReviewPackAsset>,
}

pub(crate) fn validate_review_pack_projection(pack: &ReviewPackView) -> Result<(), ReviewError> {
    if !valid_pack_header(pack) || !valid_pack_assets(pack) || !valid_pack_items(pack) {
        return Err(ReviewError::Pack);
    }
    Ok(())
}

fn valid_pack_header(pack: &ReviewPackView) -> bool {
    let ids = [
        &pack.pack_id,
        &pack.session_id,
        &pack.device_id,
        &pack.lease_id,
    ];
    let progress = match pack.status.as_str() {
        "active" => (pack.current_position as usize) < pack.items.len(),
        "completed" => pack.current_position as usize == pack.items.len(),
        "abandoned" => pack.current_position as usize <= pack.items.len(),
        _ => false,
    };
    pack.format_version == 1
        && ids
            .into_iter()
            .all(|id| bounded_chars(id, MAX_REVIEW_PACK_ID_CHARS))
        && !pack.cursor_server_id.is_empty()
        && !pack.cursor_token.is_empty()
        && pack.issued_at_us > 0
        && pack.day_boundary_hour <= 23
        && (1..=MAX_REVIEW_PACK_ITEMS).contains(&pack.items.len())
        && progress
}

fn valid_pack_assets(pack: &ReviewPackView) -> bool {
    let paths = pack
        .assets
        .iter()
        .map(|asset| asset.logical_path.as_str())
        .collect::<BTreeSet<_>>();
    paths.len() == pack.assets.len()
        && pack.assets.iter().all(|asset| {
            is_canonical_sha256(&asset.content_sha256)
                && asset.size_bytes <= MAX_OFFLINE_ASSET_BYTES
                && bounded_chars(&asset.media_type, MAX_REVIEW_PACK_MEDIA_TYPE_CHARS)
                && validate_asset_path(&asset.logical_path).is_ok()
                && asset.logical_path.split('/').nth(1) == Some(asset.content_sha256.as_str())
        })
}

fn valid_pack_items(pack: &ReviewPackView) -> bool {
    let paths = pack
        .assets
        .iter()
        .map(|asset| asset.logical_path.as_str())
        .collect::<BTreeSet<_>>();
    pack.items.iter().enumerate().all(|(position, item)| {
        item.position as usize == position
            && bounded_chars(&item.thema_id, MAX_REVIEW_PACK_ID_CHARS)
            && is_canonical_sha256(&item.content_sha256)
            && item.asset_paths.iter().collect::<BTreeSet<_>>().len() == item.asset_paths.len()
            && item
                .asset_paths
                .iter()
                .all(|path| paths.contains(path.as_str()))
    })
}

fn bounded_chars(value: &str, maximum: usize) -> bool {
    let length = value.chars().count();
    (1..=maximum).contains(&length)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OfflineReviewOperation {
    pub(crate) attempt_id: String,
    pub(crate) pack_id: String,
    pub(crate) session_id: String,
    pub(crate) lease_id: String,
    pub(crate) device_id: String,
    pub(crate) device_sequence: u64,
    pub(crate) session_position: u32,
    pub(crate) thema_id: String,
    pub(crate) thema_revision: u64,
    pub(crate) base_state_revision: u64,
    pub(crate) content_sha256: String,
    pub(crate) presentation: PresentationSnapshot,
    pub(crate) config: FsrsConfigSnapshot,
    pub(crate) matcher_version: u32,
    pub(crate) response: AnswerInput,
    pub(crate) detected_success: bool,
    pub(crate) accepted_success: bool,
    pub(crate) override_count: u32,
    pub(crate) proposed_transition: TransitionSnapshot,
    pub(crate) desired_suspended: bool,
    pub(crate) answered_at_us: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OfflineAnswerInput {
    pub(crate) attempt_id: String,
    pub(crate) session_position: u32,
    pub(crate) response: AnswerInput,
    pub(crate) accepted_success: Option<bool>,
    pub(crate) desired_suspended: bool,
    pub(crate) answered_at_us: u64,
}

pub(crate) fn prepare_offline_operation(
    pack: &ReviewPackView,
    device_sequence: u64,
    input: OfflineAnswerInput,
) -> Result<OfflineReviewOperation, OfflineAnswerError> {
    let item = pack_item(pack, input.session_position)?;
    validate_offline_answer(pack, &input)?;
    let detected = detected_success(item, &input.response)?;
    let accepted = input.accepted_success.unwrap_or(detected);
    let transition = offline_transition(pack, item, accepted, &input)?;
    Ok(build_offline_operation(
        pack,
        item,
        device_sequence,
        input,
        detected,
        accepted,
        transition,
    ))
}

fn pack_item(
    pack: &ReviewPackView,
    session_position: u32,
) -> Result<&ReviewPackItem, OfflineAnswerError> {
    if pack.status != "active" {
        return Err(OfflineAnswerError::Pack);
    }
    pack.items
        .iter()
        .find(|item| item.position == session_position)
        .ok_or(OfflineAnswerError::Pack)
}

fn validate_offline_answer(
    pack: &ReviewPackView,
    input: &OfflineAnswerInput,
) -> Result<(), OfflineAnswerError> {
    if input.attempt_id.is_empty()
        || input.attempt_id.len() > 128
        || input.answered_at_us < pack.issued_at_us
    {
        return Err(OfflineAnswerError::Input);
    }
    Ok(())
}

fn detected_success(
    item: &ReviewPackItem,
    response: &AnswerInput,
) -> Result<bool, OfflineAnswerError> {
    let content =
        presentation_content(&item.presentation).map_err(|_| OfflineAnswerError::Grade)?;
    grade(item.matcher_version, &content, &response.clone().into())
        .map(|result| result.success)
        .map_err(|_| OfflineAnswerError::Grade)
}

fn offline_transition(
    pack: &ReviewPackView,
    item: &ReviewPackItem,
    accepted: bool,
    input: &OfflineAnswerInput,
) -> Result<Transition, OfflineAnswerError> {
    let day = review_day_at(input.answered_at_us, &pack.timezone, pack.day_boundary_hour)
        .map_err(|_| OfflineAnswerError::Clock)?;
    let state = item.review_state.clone().into();
    let config = item.config.clone().into();
    compute_transition(&state, &config, accepted, day, input.answered_at_us)
        .map_err(|_| OfflineAnswerError::Schedule)
}

#[allow(clippy::too_many_arguments)]
fn build_offline_operation(
    pack: &ReviewPackView,
    item: &ReviewPackItem,
    device_sequence: u64,
    input: OfflineAnswerInput,
    detected_success: bool,
    accepted_success: bool,
    transition: Transition,
) -> OfflineReviewOperation {
    OfflineReviewOperation {
        attempt_id: input.attempt_id,
        pack_id: pack.pack_id.clone(),
        session_id: pack.session_id.clone(),
        lease_id: pack.lease_id.clone(),
        device_id: pack.device_id.clone(),
        device_sequence,
        session_position: item.position,
        thema_id: item.thema_id.clone(),
        thema_revision: item.thema_revision,
        base_state_revision: item.review_state.state_revision,
        content_sha256: item.content_sha256.clone(),
        presentation: item.presentation.clone(),
        config: item.config.clone(),
        matcher_version: item.matcher_version,
        response: input.response,
        detected_success,
        accepted_success,
        override_count: u32::from(detected_success != accepted_success),
        proposed_transition: (&transition).into(),
        desired_suspended: input.desired_suspended,
        answered_at_us: input.answered_at_us,
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum OfflineAnswerError {
    #[error("offline review pack cannot accept an answer")]
    Pack,
    #[error("offline review answer input is invalid")]
    Input,
    #[error("offline review response cannot be graded")]
    Grade,
    #[error("offline review clock is invalid")]
    Clock,
    #[error("offline review transition cannot be computed")]
    Schedule,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReconciliationStatus {
    Accepted,
    Conflict,
}

impl ReconciliationStatus {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Conflict => "conflict",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OfflineConflictReason {
    LeaseEnded,
    SessionAdvanced,
    ContentChanged,
    SchedulingChanged,
    CaptureMismatch,
}

impl OfflineConflictReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::LeaseEnded => "lease_ended",
            Self::SessionAdvanced => "session_advanced",
            Self::ContentChanged => "content_changed",
            Self::SchedulingChanged => "scheduling_changed",
            Self::CaptureMismatch => "capture_mismatch",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OfflineReviewResult {
    pub(crate) format_version: u32,
    pub(crate) attempt_id: String,
    pub(crate) status: ReconciliationStatus,
    pub(crate) conflict_reason: Option<OfflineConflictReason>,
    pub(crate) event_id: Option<String>,
    pub(crate) authoritative_state: ReviewStateSnapshot,
    pub(crate) session: ReviewSessionView,
    pub(crate) cursor: u64,
    pub(crate) cursor_server_id: String,
    pub(crate) cursor_token: String,
}

pub(crate) fn presentation_content(
    presentation: &PresentationSnapshot,
) -> Result<ThemaContent, ReviewError> {
    super::stored_thema_content(
        &presentation.kind,
        presentation.keimenon.clone(),
        presentation.hypothesis.clone(),
        presentation.answer.clone(),
        presentation.source_guid.clone(),
        presentation.parathema.clone(),
        presentation.review_image.clone(),
    )
    .map_err(|_| ReviewError::Content)
}

pub(crate) fn review_question(
    presentation: &PresentationSnapshot,
) -> Result<ReviewQuestion, ReviewError> {
    let content = presentation_content(presentation)?
        .validate()
        .map_err(|_| ReviewError::Content)?;
    let text = question_text(&content)?;
    ensure_question_is_safe(&content, &text)?;
    let response = response_prompt(&content);
    Ok(ReviewQuestion { text, response })
}

pub(crate) fn progressive_cloze_check(
    presentation: &PresentationSnapshot,
    check: crate::domain::ClozeCheck,
) -> Result<ReviewClozeCheck, ReviewError> {
    let content = presentation_content(presentation)?
        .validate()
        .map_err(|_| ReviewError::Content)?;
    if content.kind != ThemaKind::Cloze || check.revealed.len() != content.answer.len() {
        return Err(ReviewError::Content);
    }
    let question = progressive_cloze_question(&content, &check.revealed)?;
    let next_input_script = check
        .revealed
        .iter()
        .position(|revealed| !revealed)
        .and_then(|index| dominant_non_latin_script(&content.answer[index]));
    Ok(ReviewClozeCheck {
        matches: check.matches,
        all_entered_correct: check.all_entered_correct,
        complete: check.complete,
        question,
        next_input_script,
    })
}

fn question_text(content: &ThemaContent) -> Result<String, ReviewError> {
    match content.kind {
        ThemaKind::Basic => Ok(basic_question(content)),
        ThemaKind::Cloze => concealed_question(content, true),
        ThemaKind::Mcq => Ok(content.keimenon.clone()),
        ThemaKind::McCloze => concealed_question(content, false),
    }
}

fn basic_question(content: &ThemaContent) -> String {
    let Some(hint) = content.hypothesis.first().and_then(Option::as_deref) else {
        return content.keimenon.clone();
    };
    if hint.is_empty() {
        content.keimenon.clone()
    } else {
        format!("{}\n\nHint: {hint}", content.keimenon)
    }
}

fn concealed_question(content: &ThemaContent, add_hints: bool) -> Result<String, ReviewError> {
    let mut question = content.keimenon.clone();
    for answer in &content.answer {
        question = conceal_first(question, answer)?;
    }
    if add_hints {
        Ok(add_cloze_hints(question, &content.hypothesis))
    } else {
        Ok(question)
    }
}

fn progressive_cloze_question(
    content: &ThemaContent,
    revealed: &[bool],
) -> Result<String, ReviewError> {
    let mut question = concealed_question(content, false)?;
    let mut cursor = 0;
    for (index, answer) in content.answer.iter().enumerate() {
        let relative = question[cursor..]
            .find("(...)")
            .ok_or(ReviewError::Content)?;
        let start = cursor + relative;
        let hint = content.hypothesis.get(index).and_then(Option::as_deref);
        let replacement = if revealed[index] {
            trim_outer_quotes(answer).trim().to_owned()
        } else {
            hint.filter(|value| !matches!(*value, "" | "nil" | "\"\""))
                .map_or_else(|| "(...)".to_owned(), |value| format!("({value})"))
        };
        question.replace_range(start..start + "(...)".len(), &replacement);
        cursor = start + replacement.len();
    }
    Ok(question)
}

fn ensure_question_is_safe(content: &ThemaContent, question: &str) -> Result<(), ReviewError> {
    if content.kind == ThemaKind::Mcq {
        return Ok(());
    }
    for answer in &content.answer {
        let answer = trim_outer_quotes(answer).trim();
        if answer.is_empty() || case_folded_literal(answer)?.is_match(question) {
            return Err(ReviewError::Content);
        }
    }
    Ok(())
}

fn conceal_first(question: String, captured_answer: &str) -> Result<String, ReviewError> {
    let answer = trim_outer_quotes(captured_answer);
    if answer.is_empty() {
        return Err(ReviewError::Content);
    }
    let replacement = format!(
        "{}(...){}",
        leading_whitespace(answer),
        trailing_whitespace(answer)
    );
    let expression = case_folded_literal(answer)?;
    let range = match expression.find(&question) {
        Some(found) => found.range(),
        None => return Ok(question),
    };
    Ok(replace_range(question, range, &replacement))
}

fn case_folded_literal(value: &str) -> Result<regex::Regex, ReviewError> {
    RegexBuilder::new(&regex::escape(value))
        .case_insensitive(true)
        .build()
        .map_err(|_| ReviewError::Content)
}

fn replace_range(mut value: String, range: std::ops::Range<usize>, replacement: &str) -> String {
    value.replace_range(range, replacement);
    value
}

fn trim_outer_quotes(value: &str) -> &str {
    let value = value.strip_suffix('"').unwrap_or(value);
    value.strip_prefix('"').unwrap_or(value)
}

fn leading_whitespace(value: &str) -> &str {
    let end = value
        .find(|character: char| !character.is_whitespace())
        .unwrap_or(value.len());
    &value[..end]
}

fn trailing_whitespace(value: &str) -> &str {
    let start = value
        .char_indices()
        .rfind(|(_, character)| !character.is_whitespace())
        .map_or(0, |(index, character)| index + character.len_utf8());
    &value[start..]
}

fn add_cloze_hints(mut question: String, hints: &[Option<String>]) -> String {
    let mut cursor = 0;
    for hint in hints {
        let Some(relative) = question[cursor..].find("(...)") else {
            break;
        };
        let start = cursor + relative;
        cursor = replace_hint(&mut question, start, hint.as_deref());
    }
    question
}

fn replace_hint(question: &mut String, start: usize, hint: Option<&str>) -> usize {
    let Some(hint) = hint.filter(|value| !matches!(*value, "" | "nil" | "\"\"")) else {
        return start + "(...)".len();
    };
    let replacement = format!("({hint})");
    question.replace_range(start..start + "(...)".len(), &replacement);
    start + replacement.len()
}

fn response_prompt(content: &ThemaContent) -> ReviewResponsePrompt {
    match content.kind {
        ThemaKind::Basic => ReviewResponsePrompt::Text {
            input_script: dominant_non_latin_script(&content.answer[0]),
        },
        ThemaKind::Cloze => ReviewResponsePrompt::Texts {
            input_scripts: content
                .answer
                .iter()
                .map(|answer| dominant_non_latin_script(answer))
                .collect(),
        },
        ThemaKind::Mcq | ThemaKind::McCloze => ReviewResponsePrompt::Choice {
            choices: content.hypothesis.iter().flatten().cloned().collect(),
        },
    }
}

fn dominant_non_latin_script(value: &str) -> Option<String> {
    let mut counts: Vec<(Script, usize)> = Vec::new();
    for script in value.chars().map(|character| character.script()) {
        if matches!(
            script,
            Script::Latin | Script::Common | Script::Inherited | Script::Unknown
        ) {
            continue;
        }
        match counts
            .iter_mut()
            .find(|(candidate, _)| *candidate == script)
        {
            Some((_, count)) => *count += 1,
            None => counts.push((script, 1)),
        }
    }
    let script = counts
        .into_iter()
        .reduce(|best, candidate| {
            if candidate.1 > best.1 {
                candidate
            } else {
                best
            }
        })?
        .0;
    Some(script_name(script))
}

fn script_name(script: Script) -> String {
    script
        .full_name()
        .chars()
        .map(|character| match character {
            ' ' | '_' => '-',
            value => value.to_ascii_lowercase(),
        })
        .collect()
}

pub(crate) fn review_day_at(
    timestamp_us: u64,
    timezone: &str,
    day_boundary_hour: u8,
) -> Result<u32, ReviewError> {
    if day_boundary_hour > 23 {
        return Err(ReviewError::Clock);
    }
    let timezone = timezone.parse::<Tz>().map_err(|_| ReviewError::Clock)?;
    let seconds = i64::try_from(timestamp_us / 1_000_000).map_err(|_| ReviewError::Clock)?;
    let micros = u32::try_from(timestamp_us % 1_000_000).map_err(|_| ReviewError::Clock)?;
    let utc = DateTime::<Utc>::from_timestamp(seconds, micros * 1_000).ok_or(ReviewError::Clock)?;
    let local = utc.with_timezone(&timezone) - Duration::hours(i64::from(day_boundary_hour));
    Ok(
        u32::try_from(local.year()).map_err(|_| ReviewError::Clock)? * 10_000
            + local.month() * 100
            + local.day(),
    )
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum ReviewError {
    #[error("review source is invalid")]
    Source,
    #[error("review target count is invalid")]
    Target,
    #[error("captured thema content is invalid")]
    Content,
    #[error("review clock settings are invalid")]
    Clock,
    #[error("review attempt operation is invalid")]
    Operation,
    #[error("review pack projection is invalid")]
    Pack,
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use crate::application::MAX_REVIEW_PACK_ITEMS;

    use super::{
        PresentationSnapshot, ReviewAttemptMutation, ReviewAttemptOperation, ReviewResponsePrompt,
        ReviewSource, ReviewSourceKind, review_day_at, review_question,
    };

    fn presentation(kind: &str, keimenon: &str) -> PresentationSnapshot {
        PresentationSnapshot {
            kind: kind.into(),
            keimenon: keimenon.into(),
            hypothesis: vec![],
            answer: vec![],
            source_guid: None,
            parathema: None,
            review_image: None,
            tags: vec![],
        }
    }

    #[test]
    fn review_question_conceals_clozes_and_describes_text_inputs() {
        let mut captured = presentation("cloze", "Question with μυστικό and тайна");
        captured.hypothesis = vec![Some("Greek hint".into()), Some("Cyrillic hint".into())];
        captured.answer = vec!["μυστικό".into(), "тайна".into()];

        let question = review_question(&captured).unwrap();

        assert_eq!(
            question.text,
            "Question with (Greek hint) and (Cyrillic hint)"
        );
        assert_eq!(
            question.response,
            ReviewResponsePrompt::Texts {
                input_scripts: vec![Some("greek".into()), Some("cyrillic".into())],
            }
        );
    }

    #[test]
    fn progressive_cloze_question_reveals_matches_and_selects_next_script() {
        let mut captured = presentation("cloze", "Question with μυστικό and тайна");
        captured.hypothesis = vec![Some("Greek hint".into()), Some("Cyrillic hint".into())];
        captured.answer = vec!["μυστικό".into(), "тайна".into()];

        let check = super::progressive_cloze_check(
            &captured,
            crate::domain::check_clozes(&captured.answer, &["тайна".into()]),
        )
        .unwrap();

        assert_eq!(check.question, "Question with (Greek hint) and тайна");
        assert_eq!(check.next_input_script.as_deref(), Some("greek"));
        assert_eq!(check.matches, vec![true]);
        assert!(!check.complete);
    }

    #[test]
    fn review_question_materializes_basic_hint_without_exposing_answer() {
        let mut captured = presentation("basic", "Question");
        captured.hypothesis = vec![Some("Remember this".into())];
        captured.answer = vec!["ordinary Latin answer".into()];

        let question = review_question(&captured).unwrap();

        assert_eq!(question.text, "Question\n\nHint: Remember this");
        assert_eq!(
            question.response,
            ReviewResponsePrompt::Text { input_script: None }
        );
    }

    #[test]
    fn review_question_conceals_mc_cloze_and_exposes_choices() {
        let mut captured = presentation("mc-cloze", "A λόγος is concealed");
        captured.hypothesis = vec![Some("λόγος".into()), Some("βίος".into())];
        captured.answer = vec!["λόγος".into()];

        let question = review_question(&captured).unwrap();

        assert_eq!(question.text, "A (...) is concealed");
        assert_eq!(
            question.response,
            ReviewResponsePrompt::Choice {
                choices: vec!["λόγος".into(), "βίος".into()],
            }
        );
    }

    #[test]
    fn review_question_preserves_cloze_whitespace_and_skips_empty_hints() {
        let mut captured = presentation("cloze", "Before secret after second");
        captured.hypothesis = vec![Some("nil".into()), Some("\"\"".into())];
        captured.answer = vec!["\" secret \"".into(), "second".into()];

        let question = review_question(&captured).unwrap();

        assert_eq!(question.text, "Before (...) after (...)");
    }

    #[test]
    fn review_question_conceals_case_folded_answers_like_emacs() {
        let mut cloze = presentation("cloze", "Alpha and ΣΟΦΌΣ");
        cloze.answer = vec!["alpha".into(), "σοφός".into()];

        let question = review_question(&cloze).unwrap();

        assert_eq!(question.text, "(...) and (...)");
    }

    #[test]
    fn review_question_uses_first_script_when_counts_are_tied() {
        let mut captured = presentation("basic", "Question");
        captured.answer = vec!["αб".into()];

        let question = review_question(&captured).unwrap();

        assert_eq!(
            question.response,
            ReviewResponsePrompt::Text {
                input_script: Some("greek".into()),
            }
        );
    }

    #[test]
    fn review_question_rejects_text_that_still_contains_an_answer() {
        let mut basic = presentation("basic", "The answer is SECRET");
        basic.answer = vec!["secret".into()];
        let mut repeated_cloze = presentation("cloze", "secret and secret");
        repeated_cloze.answer = vec!["secret".into()];
        let mut unicode_cloze = presentation("cloze", "ΣΟΦΌΣ and σοφός");
        unicode_cloze.answer = vec!["σοφός".into()];

        assert!(review_question(&basic).is_err());
        assert!(review_question(&repeated_cloze).is_err());
        assert!(review_question(&unicode_cloze).is_err());
    }

    #[test]
    fn attempt_operations_reject_revisions_sqlite_cannot_store() {
        assert!(
            ReviewAttemptOperation::new(
                1,
                "operation".into(),
                "attempt".into(),
                u64::MAX,
                ReviewAttemptMutation::Suspension(true),
            )
            .is_err()
        );
    }

    #[test]
    fn attempt_operations_require_ascii_operation_ids() {
        for operation_id in ["\0", "π", "-leading", "contains space"] {
            assert!(
                ReviewAttemptOperation::new(
                    1,
                    operation_id.into(),
                    "attempt".into(),
                    0,
                    ReviewAttemptMutation::Suspension(true),
                )
                .is_err(),
                "accepted {operation_id:?}"
            );
        }
    }

    #[test]
    fn topic_sources_reject_unbounded_graph_depth() {
        let source = ReviewSource {
            kind: ReviewSourceKind::Topic,
            tags: vec![],
            exclude_tags: vec![],
            topic_node_id: Some("node".into()),
            topic_forward_depth: Some(1_000_000),
            topic_backward_depth: Some(1),
            manual_ids: vec![],
        };

        assert!(source.validate().is_err());
    }

    #[test]
    fn manual_sources_reject_more_than_the_session_maximum() {
        let source = ReviewSource {
            kind: ReviewSourceKind::Manual,
            tags: vec![],
            exclude_tags: vec![],
            topic_node_id: None,
            topic_forward_depth: None,
            topic_backward_depth: None,
            manual_ids: (0..=MAX_REVIEW_PACK_ITEMS)
                .map(|index| format!("thema-{index}"))
                .collect(),
        };

        assert!(source.validate().is_err());
    }

    #[test]
    fn review_day_respects_timezone_boundary_and_dst() {
        let fixture: LogicalDayFixture =
            serde_json::from_str(include_str!("../../tests/fixtures/review/parity-v1.json"))
                .unwrap();
        for case in fixture.logical_days {
            let (timestamp, boundary) = match case.name.as_str() {
                "before_boundary" => (1_772_319_000_000_000, 3),
                "dst_before" => (1_774_744_200_000_000, 0),
                "dst_after" => (1_774_747_800_000_000, 0),
                _ => panic!("unknown logical-day parity case {}", case.name),
            };
            assert_eq!(
                review_day_at(timestamp, "Europe/Athens", boundary).unwrap(),
                case.review_day,
                "{}",
                case.name
            );
        }
    }

    #[derive(Deserialize)]
    struct LogicalDayFixture {
        logical_days: Vec<LogicalDayCase>,
    }

    #[derive(Deserialize)]
    struct LogicalDayCase {
        name: String,
        review_day: u32,
    }
}
