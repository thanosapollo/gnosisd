use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{
    ReviewSessionV1, ReviewSourceInputV1, ReviewSourceV1, ReviewStateV1, ReviewTransitionV1,
};
use crate::application::{
    ContentCounts, DashboardSummary, ReviewHistoryEvent, ReviewQueueCounts, ReviewQueueSummary,
    ReviewSessionSummary, ReviewTotals, ReviewWindow, ThemaReviewState,
};
use crate::domain::ClozeCheck;

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DashboardV1 {
    pub generated_at_us: u64,
    pub review_day: u32,
    pub today: ReviewTotalsV1,
    pub window: ReviewWindowV1,
    pub queue: ReviewQueueCountsV1,
    pub content: ContentCountsV1,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(required)]
    pub active_session: Option<ReviewSessionV1>,
}

impl DashboardV1 {
    pub(crate) fn from_application(
        value: &DashboardSummary,
    ) -> Result<Self, super::ReviewAttemptWireError> {
        Ok(Self {
            generated_at_us: value.generated_at_us,
            review_day: value.review_day,
            today: ReviewTotalsV1::from(&value.today),
            window: ReviewWindowV1::from(&value.window),
            queue: ReviewQueueCountsV1::from(&value.queue),
            content: ContentCountsV1::from(&value.content),
            active_session: value
                .active_session
                .as_ref()
                .map(ReviewSessionV1::from_application)
                .transpose()?,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewTotalsV1 {
    pub review_count: u64,
    pub new_count: u64,
    pub success_count: u64,
    pub failure_count: u64,
    pub override_count: u64,
}

impl From<&ReviewTotals> for ReviewTotalsV1 {
    fn from(value: &ReviewTotals) -> Self {
        Self {
            review_count: value.review_count,
            new_count: value.new_count,
            success_count: value.success_count,
            failure_count: value.failure_count,
            override_count: value.override_count,
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewWindowV1 {
    pub from_day: u32,
    pub through_day: u32,
    pub days: u16,
    pub active_days: u64,
    pub totals: ReviewTotalsV1,
    pub average_per_calendar_day: f64,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(required)]
    pub average_per_active_day: Option<f64>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(required)]
    pub success_rate: Option<f64>,
}

impl From<&ReviewWindow> for ReviewWindowV1 {
    fn from(value: &ReviewWindow) -> Self {
        let graded = value.totals.success_count + value.totals.failure_count;
        Self {
            from_day: value.from_day,
            through_day: value.through_day,
            days: value.days,
            active_days: value.active_days,
            totals: ReviewTotalsV1::from(&value.totals),
            average_per_calendar_day: value.totals.review_count as f64 / f64::from(value.days),
            average_per_active_day: (value.active_days > 0)
                .then(|| value.totals.review_count as f64 / value.active_days as f64),
            success_rate: (graded > 0).then(|| value.totals.success_count as f64 / graded as f64),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewQueueCountsV1 {
    pub new_count: u64,
    pub due_count: u64,
    pub overdue_count: u64,
    pub suspended_count: u64,
}

impl From<&ReviewQueueCounts> for ReviewQueueCountsV1 {
    fn from(value: &ReviewQueueCounts) -> Self {
        Self {
            new_count: value.new_count,
            due_count: value.due_count,
            overdue_count: value.overdue_count,
            suspended_count: value.suspended_count,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContentCountsV1 {
    pub active_themata: u64,
    pub archived_themata: u64,
    pub active_documents: u64,
    pub active_nodes: u64,
}

impl From<&ContentCounts> for ContentCountsV1 {
    fn from(value: &ContentCounts) -> Self {
        Self {
            active_themata: value.active_themata,
            archived_themata: value.archived_themata,
            active_documents: value.active_documents,
            active_nodes: value.active_nodes,
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSessionSummaryV1 {
    pub session: ReviewSessionV1,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(required)]
    pub ended_at_us: Option<u64>,
    pub review_count: u64,
    pub new_count: u64,
    pub success_count: u64,
    pub failure_count: u64,
    pub override_count: u64,
}

impl ReviewSessionSummaryV1 {
    pub(crate) fn from_application(
        value: &ReviewSessionSummary,
    ) -> Result<Self, super::ReviewAttemptWireError> {
        Ok(Self {
            session: ReviewSessionV1::from_application(&value.session)?,
            ended_at_us: value.ended_at_us,
            review_count: value.review_count,
            new_count: value.new_count,
            success_count: value.success_count,
            failure_count: value.failure_count,
            override_count: value.override_count,
        })
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSessionPageV1 {
    #[schemars(length(max = 500))]
    pub items: Vec<ReviewSessionSummaryV1>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(required)]
    pub next: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReviewSessionListRequestV1 {
    pub after: Option<String>,
    pub limit: Option<u16>,
    pub status: Option<super::ReviewSessionStatusV1>,
    pub mode: Option<super::ReviewSessionModeV1>,
}

impl ReviewSessionPageV1 {
    pub(crate) fn from_application(
        items: &[ReviewSessionSummary],
    ) -> Result<Self, super::ReviewAttemptWireError> {
        Ok(Self {
            items: items
                .iter()
                .map(ReviewSessionSummaryV1::from_application)
                .collect::<Result<_, _>>()?,
            next: items.last().map(|item| item.session.session_id.clone()),
        })
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewHistoryEventV1 {
    pub event_id: String,
    pub thema_id: String,
    pub session_id: String,
    pub reviewed_at_us: u64,
    pub review_day: u32,
    pub detected_success: bool,
    pub accepted_success: bool,
    pub override_count: u64,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(required)]
    pub transition: Option<ReviewTransitionV1>,
}

impl From<&ReviewHistoryEvent> for ReviewHistoryEventV1 {
    fn from(value: &ReviewHistoryEvent) -> Self {
        Self {
            event_id: value.event_id.clone(),
            thema_id: value.thema_id.clone(),
            session_id: value.session_id.clone(),
            reviewed_at_us: value.reviewed_at_us,
            review_day: value.review_day,
            detected_success: value.detected_success,
            accepted_success: value.accepted_success,
            override_count: value.override_count,
            transition: value.transition.as_ref().map(ReviewTransitionV1::from),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewHistoryPageV1 {
    #[schemars(length(max = 500))]
    pub items: Vec<ReviewHistoryEventV1>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(required)]
    pub next: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReviewHistoryRequestV1 {
    pub after: Option<String>,
    pub limit: Option<u16>,
    pub thema_id: Option<String>,
    pub session_id: Option<String>,
    pub from_day: Option<u32>,
    pub through_day: Option<u32>,
    pub accepted_success: Option<bool>,
}

impl ReviewHistoryPageV1 {
    pub(crate) fn from_application(items: &[ReviewHistoryEvent]) -> Self {
        Self {
            items: items.iter().map(ReviewHistoryEventV1::from).collect(),
            next: items.last().map(|item| item.event_id.clone()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewQueueSummaryRequestV1 {
    pub source: ReviewSourceInputV1,
}

impl ReviewQueueSummaryRequestV1 {
    pub(crate) fn into_application(self) -> crate::application::ReviewSource {
        self.source.into_application()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewQueueSummaryV1 {
    pub source: ReviewSourceV1,
    pub matched_count: u64,
}

impl From<&ReviewQueueSummary> for ReviewQueueSummaryV1 {
    fn from(value: &ReviewQueueSummary) -> Self {
        Self {
            source: ReviewSourceV1::from(&value.source),
            matched_count: value.matched_count,
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemaReviewStateV1 {
    pub thema_id: String,
    pub state: ReviewStateV1,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewClozeCheckRequestV1 {
    #[schemars(length(min = 1, max = 500))]
    pub inputs: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewClozeCheckV1 {
    #[schemars(length(max = 500))]
    pub matches: Vec<bool>,
    pub all_entered_correct: bool,
    pub complete: bool,
}

impl From<ClozeCheck> for ReviewClozeCheckV1 {
    fn from(value: ClozeCheck) -> Self {
        Self {
            matches: value.matches,
            all_entered_correct: value.all_entered_correct,
            complete: value.complete,
        }
    }
}

impl From<&ThemaReviewState> for ThemaReviewStateV1 {
    fn from(value: &ThemaReviewState) -> Self {
        Self {
            thema_id: value.thema_id.clone(),
            state: ReviewStateV1::from(&value.state),
        }
    }
}
