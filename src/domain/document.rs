use std::collections::HashSet;

use thiserror::Error;

use super::{NodeEdge, NodeId, project_unique_edges};

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum DocumentError {
    #[error("document title must not be blank")]
    BlankDocumentTitle,
    #[error("node title must not be blank")]
    BlankNodeTitle,
    #[error("title path must contain at least one non-blank title")]
    InvalidTitlePath,
    #[error("source span start must be less than its end")]
    InvalidSourceSpan,
    #[error("source span must lie on UTF-8 byte boundaries")]
    InvalidUtf8SourceSpan,
    #[error("node title must equal the final title-path component")]
    MismatchedNodeTitlePath,
    #[error("document node IDs must be unique")]
    DuplicateNodeId,
    #[error("file node must belong to the document projection")]
    MissingFileNode,
    #[error("node parent must belong to the document projection")]
    MissingParentNode,
    #[error("link owner must belong to the document projection")]
    MissingLinkOwner,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct SourceSpan {
    start: usize,
    end: usize,
}

impl SourceSpan {
    pub fn for_source(source: &str, start: usize, end: usize) -> Result<Self, DocumentError> {
        let span = Self::from_offsets(start, end)?;
        if end > source.len() || !source.is_char_boundary(start) || !source.is_char_boundary(end) {
            Err(DocumentError::InvalidUtf8SourceSpan)
        } else {
            Ok(span)
        }
    }

    pub(crate) fn from_offsets(start: usize, end: usize) -> Result<Self, DocumentError> {
        (start < end)
            .then_some(Self { start, end })
            .ok_or(DocumentError::InvalidSourceSpan)
    }

    pub const fn start(self) -> usize {
        self.start
    }

    pub const fn end(self) -> usize {
        self.end
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TitlePath(Vec<String>);

impl TitlePath {
    pub fn new(parts: Vec<String>) -> Result<Self, DocumentError> {
        if parts.is_empty() || parts.iter().any(|part| part.trim().is_empty()) {
            Err(DocumentError::InvalidTitlePath)
        } else {
            Ok(Self(parts))
        }
    }

    pub fn parts(&self) -> &[String] {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Node {
    id: NodeId,
    title: String,
    title_path: TitlePath,
    tags: Vec<String>,
    parent_id: Option<NodeId>,
    level: u32,
}

impl Node {
    pub fn new(
        id: NodeId,
        title: String,
        title_path: TitlePath,
        tags: Vec<String>,
        parent_id: Option<NodeId>,
        level: u32,
    ) -> Result<Self, DocumentError> {
        if title.trim().is_empty() {
            return Err(DocumentError::BlankNodeTitle);
        }
        if title_path.parts().last().map(String::as_str) != Some(title.as_str()) {
            return Err(DocumentError::MismatchedNodeTitlePath);
        }
        Ok(Self {
            id,
            title,
            title_path,
            tags,
            parent_id,
            level,
        })
    }

    pub fn id(&self) -> &NodeId {
        &self.id
    }
    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn title_path(&self) -> &TitlePath {
        &self.title_path
    }
    pub fn tags(&self) -> &[String] {
        &self.tags
    }
    pub fn parent_id(&self) -> Option<&NodeId> {
        self.parent_id.as_ref()
    }
    pub const fn level(&self) -> u32 {
        self.level
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkOccurrence {
    owner: NodeId,
    target: NodeId,
    span: SourceSpan,
}

impl LinkOccurrence {
    pub fn new(owner: NodeId, target: NodeId, span: SourceSpan) -> Self {
        Self {
            owner,
            target,
            span,
        }
    }

    pub fn owner(&self) -> &NodeId {
        &self.owner
    }
    pub fn target(&self) -> &NodeId {
        &self.target
    }
    pub const fn span(&self) -> SourceSpan {
        self.span
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentProjection {
    title: String,
    file_node_id: Option<NodeId>,
    file_tags: Vec<String>,
    nodes: Vec<Node>,
    link_occurrences: Vec<LinkOccurrence>,
    semantic_edges: Vec<NodeEdge>,
    content_sha256: String,
}

impl DocumentProjection {
    pub fn new(input: DocumentProjectionInput) -> Result<Self, DocumentError> {
        if input.title.trim().is_empty() {
            return Err(DocumentError::BlankDocumentTitle);
        }
        validate_projection(&input)?;
        let semantic_edges = project_unique_edges(&input.link_occurrences);
        Ok(Self {
            title: input.title,
            file_node_id: input.file_node_id,
            file_tags: input.file_tags,
            nodes: input.nodes,
            link_occurrences: input.link_occurrences,
            semantic_edges,
            content_sha256: input.content_sha256,
        })
    }

    pub fn title(&self) -> &str {
        &self.title
    }
    pub fn file_node_id(&self) -> Option<&NodeId> {
        self.file_node_id.as_ref()
    }
    pub fn file_tags(&self) -> &[String] {
        &self.file_tags
    }
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }
    pub fn link_occurrences(&self) -> &[LinkOccurrence] {
        &self.link_occurrences
    }
    pub fn semantic_edges(&self) -> &[NodeEdge] {
        &self.semantic_edges
    }
    pub fn content_sha256(&self) -> &str {
        &self.content_sha256
    }
}

fn validate_projection(input: &DocumentProjectionInput) -> Result<(), DocumentError> {
    let node_ids: HashSet<_> = input.nodes.iter().map(Node::id).collect();
    if node_ids.len() != input.nodes.len() {
        return Err(DocumentError::DuplicateNodeId);
    }
    if input
        .file_node_id
        .as_ref()
        .is_some_and(|id| !node_ids.contains(id))
    {
        return Err(DocumentError::MissingFileNode);
    }
    if input
        .nodes
        .iter()
        .filter_map(Node::parent_id)
        .any(|id| !node_ids.contains(id))
    {
        return Err(DocumentError::MissingParentNode);
    }
    if input
        .link_occurrences
        .iter()
        .any(|link| !node_ids.contains(link.owner()))
    {
        return Err(DocumentError::MissingLinkOwner);
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocumentProjectionInput {
    pub title: String,
    pub file_node_id: Option<NodeId>,
    pub file_tags: Vec<String>,
    pub nodes: Vec<Node>,
    pub link_occurrences: Vec<LinkOccurrence>,
    pub content_sha256: String,
}

#[cfg(test)]
mod tests {
    use super::{
        DocumentError, DocumentProjection, DocumentProjectionInput, Node, SourceSpan, TitlePath,
    };
    use crate::domain::NodeId;

    #[test]
    fn rejects_blank_titles_and_paths() {
        assert_eq!(TitlePath::new(vec![]), Err(DocumentError::InvalidTitlePath));
        assert_eq!(
            TitlePath::new(vec![" ".into()]),
            Err(DocumentError::InvalidTitlePath)
        );
    }

    #[test]
    fn rejects_empty_or_reversed_spans() {
        assert_eq!(
            SourceSpan::for_source("text", 2, 2),
            Err(DocumentError::InvalidSourceSpan)
        );
        assert_eq!(
            SourceSpan::for_source("text", 3, 2),
            Err(DocumentError::InvalidSourceSpan)
        );
    }

    #[test]
    fn rejects_spans_that_split_utf8_or_leave_source() {
        assert!(SourceSpan::for_source("é", 0, 2).is_ok());
        assert_eq!(
            SourceSpan::for_source("é", 0, 1),
            Err(DocumentError::InvalidUtf8SourceSpan)
        );
        assert_eq!(
            SourceSpan::for_source("é", 0, 3),
            Err(DocumentError::InvalidUtf8SourceSpan)
        );
    }

    #[test]
    fn rejects_duplicate_node_ids() {
        let id = NodeId::new("duplicate").unwrap();
        let node = || {
            Node::new(
                id.clone(),
                "Title".into(),
                TitlePath::new(vec!["Title".into()]).unwrap(),
                vec![],
                None,
                1,
            )
            .unwrap()
        };
        let result = DocumentProjection::new(DocumentProjectionInput {
            title: "Document".into(),
            file_node_id: None,
            file_tags: vec![],
            nodes: vec![node(), node()],
            link_occurrences: vec![],
            content_sha256: "sha256".into(),
        });
        assert_eq!(result, Err(DocumentError::DuplicateNodeId));
    }

    #[test]
    fn rejects_parent_outside_projection() {
        let node = Node::new(
            NodeId::new("node").unwrap(),
            "Title".into(),
            TitlePath::new(vec!["Title".into()]).unwrap(),
            vec![],
            Some(NodeId::new("missing").unwrap()),
            1,
        )
        .unwrap();
        let result = DocumentProjection::new(DocumentProjectionInput {
            title: "Document".into(),
            file_node_id: None,
            file_tags: vec![],
            nodes: vec![node],
            link_occurrences: vec![],
            content_sha256: "sha256".into(),
        });
        assert_eq!(result, Err(DocumentError::MissingParentNode));
    }
}
