use super::ThemaId;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LegacyDatabaseInfo {
    pub(crate) source_schema_version: u32,
    pub(crate) legacy_table_count: u64,
    pub(crate) legacy_row_count: u64,
    pub(crate) thema_count: u64,
    pub(crate) active_thema_count: u64,
    pub(crate) review_state_count: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LegacyThema {
    pub(crate) id: ThemaId,
    pub(crate) kind: String,
    pub(crate) keimenon: String,
    pub(crate) hypothesis: Vec<String>,
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DueThema {
    pub(crate) id: ThemaId,
    pub(crate) kind: String,
    pub(crate) keimenon: String,
    pub(crate) hypothesis: Vec<String>,
    pub(crate) answer: Vec<String>,
    pub(crate) next_review_day: u32,
    pub(crate) review_count: u64,
}
