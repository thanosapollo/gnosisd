use super::{ReviewSessionView, ReviewSource, ReviewStateSnapshot, TransitionSnapshot};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DashboardSummary {
    pub(crate) generated_at_us: u64,
    pub(crate) review_day: u32,
    pub(crate) today: ReviewTotals,
    pub(crate) window: ReviewWindow,
    pub(crate) queue: ReviewQueueCounts,
    pub(crate) content: ContentCounts,
    pub(crate) active_session: Option<ReviewSessionView>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReviewTotals {
    pub(crate) review_count: u64,
    pub(crate) new_count: u64,
    pub(crate) success_count: u64,
    pub(crate) failure_count: u64,
    pub(crate) override_count: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReviewWindow {
    pub(crate) from_day: u32,
    pub(crate) through_day: u32,
    pub(crate) days: u16,
    pub(crate) active_days: u64,
    pub(crate) totals: ReviewTotals,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ReviewQueueCounts {
    pub(crate) new_count: u64,
    pub(crate) due_count: u64,
    pub(crate) overdue_count: u64,
    pub(crate) suspended_count: u64,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ContentCounts {
    pub(crate) active_themata: u64,
    pub(crate) archived_themata: u64,
    pub(crate) active_documents: u64,
    pub(crate) active_nodes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReviewSessionSummary {
    pub(crate) session: ReviewSessionView,
    pub(crate) ended_at_us: Option<u64>,
    pub(crate) review_count: u64,
    pub(crate) new_count: u64,
    pub(crate) success_count: u64,
    pub(crate) failure_count: u64,
    pub(crate) override_count: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReviewHistoryEvent {
    pub(crate) event_id: String,
    pub(crate) thema_id: String,
    pub(crate) session_id: String,
    pub(crate) reviewed_at_us: u64,
    pub(crate) review_day: u32,
    pub(crate) detected_success: bool,
    pub(crate) accepted_success: bool,
    pub(crate) override_count: u64,
    pub(crate) transition: Option<TransitionSnapshot>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReviewHistoryQuery {
    pub(crate) after: Option<String>,
    pub(crate) limit: u16,
    pub(crate) thema_id: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) from_day: Option<u32>,
    pub(crate) through_day: Option<u32>,
    pub(crate) accepted_success: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReviewSessionQuery {
    pub(crate) after: Option<String>,
    pub(crate) limit: u16,
    pub(crate) status: Option<String>,
    pub(crate) mode: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReviewQueueSummary {
    pub(crate) source: ReviewSource,
    pub(crate) matched_count: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ThemaReviewState {
    pub(crate) thema_id: String,
    pub(crate) state: ReviewStateSnapshot,
}
