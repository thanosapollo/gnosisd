mod document;
#[allow(dead_code, reason = "FSRS kernel lands before call-site migration")]
pub(crate) mod fsrs;
mod grading;
mod graph;
mod ids;
mod legacy;
mod scheduling;
mod thema;

pub use document::{
    DocumentProjection, DocumentProjectionInput, LinkOccurrence, Node, SourceSpan, TitlePath,
};
pub(crate) use grading::{
    ClozeCheck, GradeError, MATCHER_VERSION, StructuredResponse, check_clozes, grade,
};
pub use graph::{NodeEdge, project_unique_edges};
pub use ids::{DocumentId, IdError, NodeId, ThemaId};
pub(crate) use legacy::{DueThema, LegacyDatabaseInfo, LegacyReviewState, LegacyThema};
pub(crate) use scheduling::{
    ReviewState, SchedulerParameters, Transition, compute_transition, fuzz_delta,
    validate_parameters, validate_state,
};
pub use thema::{ThemaConcept, ThemaConceptRelation};
pub(crate) use thema::{ThemaContent, ThemaContentError, ThemaKind};
