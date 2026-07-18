use super::ThemaId;

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
