use chrono::{DateTime, Datelike, Duration, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{
    ReviewState, SchedulerParameters, StructuredResponse, ThemaContent, Transition,
};

const MAX_TOPIC_DEPTH: u32 = 32;
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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ReviewStateSnapshot {
    pub(crate) state_revision: u64,
    pub(crate) review_count: u64,
    pub(crate) legacy_review_count: u64,
    pub(crate) legacy_unclassified_count: u64,
    pub(crate) last_reviewed_at_us: Option<u64>,
    pub(crate) last_review_day: Option<u32>,
    pub(crate) next_review_day: u32,
    pub(crate) interval_days: Option<u32>,
    pub(crate) consecutive_successes: u64,
    pub(crate) total_successes: u64,
    pub(crate) consecutive_failures: u64,
    pub(crate) total_failures: u64,
    pub(crate) suspended: bool,
    pub(crate) gnosis_plus: f64,
    pub(crate) gnosis_minus: f64,
    pub(crate) gnosis_synolon: f64,
}

impl From<ReviewStateSnapshot> for ReviewState {
    fn from(value: ReviewStateSnapshot) -> Self {
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

impl From<&ReviewState> for ReviewStateSnapshot {
    fn from(value: &ReviewState) -> Self {
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
pub(crate) struct ParametersSnapshot {
    #[serde(default)]
    pub(crate) scheduler_version: u32,
    pub(crate) amnesia: f64,
    pub(crate) epignosis: f64,
    pub(crate) agnoia: f64,
    pub(crate) anagnosis: u64,
    pub(crate) lethe: u64,
    pub(crate) synolon_max: f64,
    pub(crate) interval_fuzz: f64,
    pub(crate) proto: Vec<u32>,
}

impl From<ParametersSnapshot> for SchedulerParameters {
    fn from(value: ParametersSnapshot) -> Self {
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

impl From<&SchedulerParameters> for ParametersSnapshot {
    fn from(value: &SchedulerParameters) -> Self {
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
    pub(crate) parameters: ParametersSnapshot,
    pub(crate) reviewed_at_us: u64,
    pub(crate) review_day: u32,
    pub(crate) success: bool,
    pub(crate) first_review: bool,
    pub(crate) elapsed_days: u32,
    pub(crate) algorithm_interval_days: f64,
    pub(crate) computed_interval_days: u32,
    pub(crate) fuzz_delta: f64,
}

impl From<&Transition> for TransitionSnapshot {
    fn from(value: &Transition) -> Self {
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

#[derive(Clone, Debug, PartialEq)]
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
    pub(crate) parameters: ParametersSnapshot,
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
    pub(crate) issued_at_us: u64,
    pub(crate) current_position: u32,
    pub(crate) timezone: String,
    pub(crate) day_boundary_hour: u8,
    pub(crate) items: Vec<ReviewPackItem>,
    pub(crate) assets: Vec<ReviewPackAsset>,
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
    pub(crate) parameters: ParametersSnapshot,
    pub(crate) matcher_version: u32,
    pub(crate) response: AnswerInput,
    pub(crate) detected_success: bool,
    pub(crate) accepted_success: bool,
    pub(crate) override_count: u32,
    pub(crate) proposed_transition: TransitionSnapshot,
    pub(crate) desired_suspended: bool,
    pub(crate) answered_at_us: u64,
    pub(crate) fuzz_sample: f64,
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
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::{ReviewSource, ReviewSourceKind, review_day_at};

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
                review_day_at(timestamp, "Europe/Berlin", boundary).unwrap(),
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
