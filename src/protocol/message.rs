use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::domain::{DocumentId, DocumentProjection};

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
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
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
            outcome: ResponseOutcome::Success { result },
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResponseOutcome {
    Success { result: ResponseResult },
    Failure { error: ProtocolError },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ResponseResult {
    SystemInfo(SystemInfo),
    DocumentImported(ImportResult),
    Document(DocumentView),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SystemInfo {
    pub protocol_version: u16,
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
