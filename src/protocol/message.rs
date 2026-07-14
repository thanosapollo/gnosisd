use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{
    DocumentId, DocumentProjection, DueThema, LegacyDatabaseInfo, LegacyReviewState, LegacyThema,
};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_REQUEST_ID_LEN: usize = 128;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub version: u16,
    pub id: String,
    #[serde(flatten)]
    pub request: Request,
}

impl RequestEnvelope {
    pub(crate) fn validate(&self) -> Result<(), MessageValidationError> {
        if self.version != PROTOCOL_VERSION {
            return Err(MessageValidationError::UnsupportedVersion(self.version));
        }
        if self.id.is_empty() || self.id.len() > MAX_REQUEST_ID_LEN {
            return Err(MessageValidationError::InvalidRequestId);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    #[serde(rename = "system.info")]
    SystemInfo {},
    #[serde(rename = "document.import")]
    DocumentImport {
        document_id: String,
        org: String,
        expected_revision: Option<u64>,
    },
    #[serde(rename = "document.get")]
    DocumentGet { document_id: String },
    #[serde(rename = "legacy.info")]
    LegacyInfo {},
    #[serde(rename = "thema.get")]
    ThemaGet { thema_id: String },
    #[serde(rename = "review.due")]
    ReviewDue { day: u32, limit: u16 },
    #[serde(rename = "review.due_page")]
    ReviewDuePage {
        day: u32,
        limit: u16,
        after: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub version: u16,
    pub id: String,
    #[serde(flatten)]
    pub outcome: ResponseOutcome,
}

impl ResponseEnvelope {
    pub fn success(id: String, result: ResponseResult) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            outcome: ResponseOutcome::Success {
                result: Box::new(result),
            },
        }
    }

    pub fn error(id: String, error: ProtocolError) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            outcome: ResponseOutcome::Failure { error },
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResponseOutcome {
    Success { result: Box<ResponseResult> },
    Failure { error: ProtocolError },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ResponseResult {
    SystemInfo(SystemInfo),
    DocumentImported(ImportResult),
    Document(DocumentView),
    LegacyInfo(LegacyInfoView),
    Thema(ThemaView),
    DueThemata(Vec<DueThemaView>),
    DuePage(DuePageView),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SystemInfo {
    pub protocol_version: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LegacyInfoView {
    pub imported: bool,
    pub source_schema_version: Option<u32>,
    pub legacy_table_count: u64,
    pub legacy_row_count: u64,
    pub thema_count: u64,
    pub active_thema_count: u64,
    pub review_state_count: u64,
}

impl From<Option<LegacyDatabaseInfo>> for LegacyInfoView {
    fn from(info: Option<LegacyDatabaseInfo>) -> Self {
        let Some(info) = info else {
            return Self {
                imported: false,
                source_schema_version: None,
                legacy_table_count: 0,
                legacy_row_count: 0,
                thema_count: 0,
                active_thema_count: 0,
                review_state_count: 0,
            };
        };
        Self {
            imported: true,
            source_schema_version: Some(info.source_schema_version),
            legacy_table_count: info.legacy_table_count,
            legacy_row_count: info.legacy_row_count,
            thema_count: info.thema_count,
            active_thema_count: info.active_thema_count,
            review_state_count: info.review_state_count,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ThemaView {
    pub id: String,
    pub kind: String,
    pub keimenon: String,
    pub hypothesis: Vec<Option<String>>,
    pub answer: Vec<String>,
    pub source_guid: Option<String>,
    pub archived_at_us: Option<u64>,
    pub parathema: Option<String>,
    pub review_image: Option<String>,
    pub tags: Vec<String>,
    pub links: Vec<String>,
    pub review_state: ReviewStateView,
}

impl From<LegacyThema> for ThemaView {
    fn from(thema: LegacyThema) -> Self {
        Self {
            id: thema.id.to_string(),
            kind: thema.kind,
            keimenon: thema.keimenon,
            hypothesis: thema.hypothesis,
            answer: thema.answer,
            source_guid: thema.source_guid,
            archived_at_us: thema.archived_at_us,
            parathema: thema.parathema,
            review_image: thema.review_image,
            tags: thema.tags,
            links: thema.links.into_iter().map(|id| id.to_string()).collect(),
            review_state: thema.review_state.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ReviewStateView {
    pub state_revision: u64,
    pub review_count: u64,
    pub legacy_review_count: u64,
    pub legacy_unclassified_count: u64,
    pub last_reviewed_at_us: Option<u64>,
    pub last_review_day: Option<u32>,
    pub next_review_day: u32,
    pub interval_days: Option<u32>,
    pub consecutive_successes: u64,
    pub total_successes: u64,
    pub consecutive_failures: u64,
    pub total_failures: u64,
    pub suspended: bool,
    pub gnosis_plus: f64,
    pub gnosis_minus: f64,
    pub gnosis_synolon: f64,
}

impl From<LegacyReviewState> for ReviewStateView {
    fn from(state: LegacyReviewState) -> Self {
        Self {
            state_revision: state.state_revision,
            review_count: state.review_count,
            legacy_review_count: state.legacy_review_count,
            legacy_unclassified_count: state.legacy_unclassified_count,
            last_reviewed_at_us: state.last_reviewed_at_us,
            last_review_day: state.last_review_day,
            next_review_day: state.next_review_day,
            interval_days: state.interval_days,
            consecutive_successes: state.consecutive_successes,
            total_successes: state.total_successes,
            consecutive_failures: state.consecutive_failures,
            total_failures: state.total_failures,
            suspended: state.suspended,
            gnosis_plus: state.gnosis_plus,
            gnosis_minus: state.gnosis_minus,
            gnosis_synolon: state.gnosis_synolon,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DueThemaView {
    pub id: String,
    pub kind: String,
    pub keimenon: String,
    pub hypothesis: Vec<Option<String>>,
    pub answer: Vec<String>,
    pub next_review_day: u32,
    pub review_count: u64,
}

impl From<DueThema> for DueThemaView {
    fn from(thema: DueThema) -> Self {
        Self {
            id: thema.id.to_string(),
            kind: thema.kind,
            keimenon: thema.keimenon,
            hypothesis: thema.hypothesis,
            answer: thema.answer,
            next_review_day: thema.next_review_day,
            review_count: thema.review_count,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DuePageView {
    pub items: Vec<DueThemaView>,
    pub next: Option<String>,
}

impl DuePageView {
    pub(crate) fn new(items: Vec<DueThema>, next: Option<String>) -> Self {
        Self {
            items: items.into_iter().map(DueThemaView::from).collect(),
            next,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ImportResult {
    pub document_id: String,
    pub revision: u64,
    pub content_sha256: String,
    pub node_count: usize,
    pub occurrence_count: usize,
    pub semantic_edge_count: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentView {
    pub document_id: String,
    pub revision: u64,
    pub title: String,
    pub file_node_id: Option<String>,
    pub file_tags: Vec<String>,
    pub nodes: Vec<NodeView>,
    pub link_occurrences: Vec<LinkOccurrenceView>,
    pub semantic_edges: Vec<NodeEdgeView>,
    pub compatibility_sha1: String,
    pub content_sha256: String,
}

impl DocumentView {
    pub(crate) fn from_projection(
        document_id: &DocumentId,
        revision: u64,
        projection: &DocumentProjection,
    ) -> Self {
        Self {
            document_id: document_id.to_string(),
            revision,
            title: projection.title().to_owned(),
            file_node_id: projection.file_node_id().map(ToString::to_string),
            file_tags: projection.file_tags().to_vec(),
            nodes: projection.nodes().iter().map(NodeView::from).collect(),
            link_occurrences: projection
                .link_occurrences()
                .iter()
                .map(LinkOccurrenceView::from)
                .collect(),
            semantic_edges: projection
                .semantic_edges()
                .iter()
                .map(NodeEdgeView::from)
                .collect(),
            compatibility_sha1: projection.compatibility_sha1().to_owned(),
            content_sha256: projection.content_sha256().to_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeView {
    pub id: String,
    pub title: String,
    pub title_path: Vec<String>,
    pub tags: Vec<String>,
    pub parent_id: Option<String>,
    pub level: u32,
}

impl From<&crate::domain::Node> for NodeView {
    fn from(node: &crate::domain::Node) -> Self {
        Self {
            id: node.id().to_string(),
            title: node.title().to_owned(),
            title_path: node.title_path().parts().to_vec(),
            tags: node.tags().to_vec(),
            parent_id: node.parent_id().map(ToString::to_string),
            level: node.level(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LinkOccurrenceView {
    pub owner_id: String,
    pub target_id: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

impl From<&crate::domain::LinkOccurrence> for LinkOccurrenceView {
    fn from(occurrence: &crate::domain::LinkOccurrence) -> Self {
        Self {
            owner_id: occurrence.owner().to_string(),
            target_id: occurrence.target().to_string(),
            start_byte: occurrence.span().start(),
            end_byte: occurrence.span().end(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeEdgeView {
    pub source_id: String,
    pub target_id: String,
}

impl From<&crate::domain::NodeEdge> for NodeEdgeView {
    fn from(edge: &crate::domain::NodeEdge) -> Self {
        Self {
            source_id: edge.source().to_string(),
            target_id: edge.target().to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    MalformedRequest,
    UnsupportedVersion,
    InvalidDocument,
    NotFound,
    Conflict,
    Busy,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
}

impl ProtocolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum MessageValidationError {
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("request ID must contain 1 to {MAX_REQUEST_ID_LEN} bytes")]
    InvalidRequestId,
}

#[cfg(test)]
mod tests {
    use super::{
        ErrorCode, MAX_REQUEST_ID_LEN, MessageValidationError, ProtocolError, Request,
        RequestEnvelope, ResponseEnvelope, ResponseOutcome, ResponseResult, SystemInfo,
    };

    #[test]
    fn request_round_trips_with_versioned_method() {
        let request = RequestEnvelope {
            version: 1,
            id: "request-1".into(),
            request: Request::DocumentGet {
                document_id: "doc-1".into(),
            },
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&json).unwrap(),
            request
        );
    }

    #[test]
    fn due_page_request_has_stable_wire_shape() {
        let request = RequestEnvelope {
            version: 1,
            id: "due-2".into(),
            request: Request::ReviewDuePage {
                day: 20260714,
                limit: 25,
                after: Some("0:20260713:8".into()),
            },
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["method"], "review.due_page");
        assert_eq!(value["params"]["day"], 20260714);
        assert_eq!(value["params"]["limit"], 25);
        assert_eq!(value["params"]["after"], "0:20260713:8");
    }

    #[test]
    fn rejects_unknown_version_and_unbounded_request_id() {
        let mut request = RequestEnvelope {
            version: 2,
            id: "request-1".into(),
            request: Request::SystemInfo {},
        };
        assert_eq!(
            request.validate(),
            Err(MessageValidationError::UnsupportedVersion(2))
        );
        request.version = 1;
        request.id = "x".repeat(MAX_REQUEST_ID_LEN + 1);
        assert_eq!(
            request.validate(),
            Err(MessageValidationError::InvalidRequestId)
        );

        let unknown_method = r#"{
            "version":1,
            "id":"request-2",
            "method":"database.execute",
            "params":{"sql":"SELECT 1"}
        }"#;
        assert!(serde_json::from_str::<RequestEnvelope>(unknown_method).is_err());
    }

    #[test]
    fn response_cannot_contain_success_and_error() {
        let response = ResponseEnvelope::success(
            "request-1".into(),
            ResponseResult::SystemInfo(SystemInfo {
                protocol_version: 1,
            }),
        );
        assert!(matches!(response.outcome, ResponseOutcome::Success { .. }));

        let response = ResponseEnvelope::error(
            "request-2".into(),
            ProtocolError::new(ErrorCode::Busy, "daemon is busy"),
        );
        assert!(matches!(response.outcome, ResponseOutcome::Failure { .. }));

        let both = r#"{
            "version":1,
            "id":"request-3",
            "status":"success",
            "result":{"type":"system_info","value":{"protocol_version":1}},
            "error":{"code":"internal","message":"should be rejected"}
        }"#;
        assert!(serde_json::from_str::<ResponseEnvelope>(both).is_err());
    }
}
