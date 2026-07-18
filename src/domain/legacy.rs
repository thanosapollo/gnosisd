use super::ThemaId;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LegacyThema {
    pub(crate) id: ThemaId,
    pub(crate) kind: String,
    pub(crate) keimenon: String,
    pub(crate) hypothesis: Vec<Option<String>>,
    pub(crate) answer: Vec<String>,
    pub(crate) source_guid: Option<String>,
    pub(crate) archived_at_us: Option<u64>,
    pub(crate) parathema: Option<String>,
    pub(crate) review_image: Option<String>,
    pub(crate) tags: Vec<String>,
    pub(crate) links: Vec<ThemaId>,
    pub(crate) review_state: LegacyReviewState,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LegacyReviewState {
    pub(crate) state_revision: u64,
    pub(crate) review_count: u64,
    pub(crate) last_reviewed_at_us: Option<u64>,
    pub(crate) last_review_day: Option<u32>,
    pub(crate) next_review_day: u32,
    pub(crate) interval_days: Option<u32>,
    pub(crate) total_successes: u64,
    pub(crate) total_failures: u64,
    pub(crate) suspended: bool,
    pub(crate) stability: Option<f64>,
    pub(crate) difficulty: Option<f64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DueThema {
    pub(crate) id: ThemaId,
    pub(crate) kind: String,
    pub(crate) keimenon: String,
    pub(crate) hypothesis: Vec<Option<String>>,
    pub(crate) answer: Vec<String>,
    pub(crate) next_review_day: u32,
    pub(crate) review_count: u64,
}
