mod document;
mod graph;
mod ids;
mod legacy;
#[allow(
    dead_code,
    reason = "thema associations are stored by this slice and used by later scheduling slices"
)]
mod thema;

pub use document::{
    DocumentProjection, DocumentProjectionInput, LinkOccurrence, Node, SourceSpan, TitlePath,
};
pub use graph::{NodeEdge, project_unique_edges};
pub use ids::{DocumentId, IdError, NodeId, ThemaId};
pub(crate) use legacy::{DueThema, LegacyDatabaseInfo, LegacyReviewState, LegacyThema};
