use std::{
    collections::HashSet, error::Error, fs, os::unix::fs::PermissionsExt, path::Path,
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use thiserror::Error;

use crate::domain::{
    DocumentId, DocumentProjection, DocumentProjectionInput, LinkOccurrence, Node, NodeEdge,
    NodeId, SourceSpan, TitlePath,
};

const INITIAL_MIGRATION: &str = include_str!("migrations/0001_initial.sql");
const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub(crate) enum StoreError {
    #[error("document revision conflict: expected {expected}, current revision is {actual:?}")]
    Conflict { expected: u64, actual: Option<u64> },
    #[error("document projection is invalid: {reason}")]
    InvalidProjection { reason: &'static str },
    #[error("stored document contains invalid {field}")]
    CorruptData { field: &'static str },
    #[error("database schema version {found} is newer than supported version {supported}")]
    UnsupportedSchemaVersion { found: u32, supported: u32 },
    #[error("storage operation failed")]
    Storage(#[source] Box<dyn Error + Send + Sync>),
}

impl StoreError {
    fn storage(error: impl Error + Send + Sync + 'static) -> Self {
        Self::Storage(Box::new(error))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct StoredDocument {
    document_id: DocumentId,
    revision: u64,
    projection: DocumentProjection,
}

impl StoredDocument {
    pub(crate) fn document_id(&self) -> &DocumentId {
        &self.document_id
    }

    pub(crate) const fn revision(&self) -> u64 {
        self.revision
    }

    pub(crate) fn projection(&self) -> &DocumentProjection {
        &self.projection
    }
}

pub(crate) struct SqliteStore {
    connection: Connection,
}

impl SqliteStore {
    pub(crate) fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let connection = Connection::open(path).map_err(StoreError::storage)?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(StoreError::storage)?;
        Self::initialize(connection)
    }

    #[cfg(test)]
    fn open_in_memory() -> Result<Self, StoreError> {
        Self::initialize(Connection::open_in_memory().map_err(StoreError::storage)?)
    }

    fn initialize(connection: Connection) -> Result<Self, StoreError> {
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(StoreError::storage)?;
        connection
            .busy_timeout(Duration::from_secs(5))
            .map_err(StoreError::storage)?;
        migrate(&connection)?;
        Ok(Self { connection })
    }

    #[cfg(test)]
    fn schema_version(&self) -> Result<u32, StoreError> {
        schema_version(&self.connection)
    }

    pub(crate) fn get_document(
        &self,
        document_id: &DocumentId,
    ) -> Result<Option<StoredDocument>, StoreError> {
        load_document(&self.connection, document_id)
    }

    pub(crate) fn replace_document(
        &mut self,
        document_id: &DocumentId,
        projection: &DocumentProjection,
        expected_revision: Option<u64>,
    ) -> Result<StoredDocument, StoreError> {
        validate_projection(projection)?;
        self.replace(document_id, projection, expected_revision, FailPoint::None)
    }

    fn replace(
        &mut self,
        document_id: &DocumentId,
        projection: &DocumentProjection,
        expected_revision: Option<u64>,
        fail_point: FailPoint,
    ) -> Result<StoredDocument, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        let current = load_document(&transaction, document_id)?;
        check_revision(expected_revision, current.as_ref())?;
        if current
            .as_ref()
            .is_some_and(|stored| stored.projection() == projection)
        {
            transaction.commit().map_err(StoreError::storage)?;
            return current.ok_or(StoreError::CorruptData { field: "document" });
        }
        let revision = next_revision(current.as_ref())?;
        replace_rows(&transaction, document_id, projection, revision, fail_point)?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(StoredDocument {
            document_id: document_id.clone(),
            revision,
            projection: projection.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FailPoint {
    None,
    #[cfg(test)]
    AfterDocument,
}

fn migrate(connection: &Connection) -> Result<(), StoreError> {
    let found = schema_version(connection)?;
    if found > SCHEMA_VERSION {
        return Err(StoreError::UnsupportedSchemaVersion {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    if found == 0 {
        connection
            .execute_batch(INITIAL_MIGRATION)
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn schema_version(connection: &Connection) -> Result<u32, StoreError> {
    let version: i64 = connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(StoreError::storage)?;
    u32::try_from(version).map_err(|_| StoreError::CorruptData {
        field: "schema version",
    })
}

fn validate_projection(projection: &DocumentProjection) -> Result<(), StoreError> {
    let node_ids: HashSet<_> = projection.nodes().iter().map(Node::id).collect();
    if node_ids.len() != projection.nodes().len() {
        return invalid_projection("node IDs must be unique");
    }
    if projection
        .file_node_id()
        .is_some_and(|id| !node_ids.contains(id))
    {
        return invalid_projection("file node must belong to the document");
    }
    if projection
        .nodes()
        .iter()
        .filter_map(Node::parent_id)
        .any(|id| !node_ids.contains(id))
    {
        return invalid_projection("node parent must belong to the document");
    }
    if projection
        .link_occurrences()
        .iter()
        .any(|link| !node_ids.contains(link.owner()))
    {
        return invalid_projection("link owner must belong to the document");
    }
    Ok(())
}

fn invalid_projection<T>(reason: &'static str) -> Result<T, StoreError> {
    Err(StoreError::InvalidProjection { reason })
}

fn check_revision(
    expected: Option<u64>,
    current: Option<&StoredDocument>,
) -> Result<(), StoreError> {
    let actual = current.map(StoredDocument::revision);
    match expected {
        Some(expected) if actual != Some(expected) => {
            Err(StoreError::Conflict { expected, actual })
        }
        _ => Ok(()),
    }
}

fn next_revision(current: Option<&StoredDocument>) -> Result<u64, StoreError> {
    current
        .map_or(Some(1), |stored| stored.revision().checked_add(1))
        .ok_or(StoreError::CorruptData { field: "revision" })
}

fn replace_rows(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    projection: &DocumentProjection,
    revision: u64,
    fail_point: FailPoint,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "DELETE FROM document WHERE document_id = ?1",
            [document_id.as_str()],
        )
        .map_err(StoreError::storage)?;
    insert_document(transaction, document_id, projection, revision)?;
    #[cfg(test)]
    if fail_point == FailPoint::AfterDocument {
        return Err(StoreError::storage(std::io::Error::other(
            "injected write failure",
        )));
    }
    let _ = fail_point;
    insert_nodes(transaction, document_id, projection.nodes())?;
    insert_occurrences(transaction, document_id, projection.link_occurrences())?;
    insert_edges(transaction, document_id, projection.semantic_edges())
}

fn insert_document(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    projection: &DocumentProjection,
    revision: u64,
) -> Result<(), StoreError> {
    let revision =
        i64::try_from(revision).map_err(|_| StoreError::CorruptData { field: "revision" })?;
    let file_tags = serde_json::to_string(projection.file_tags()).map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO document (\
             document_id, revision, title, file_node_id, file_tags_json, \
             compatibility_sha1, content_sha256\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                document_id.as_str(),
                revision,
                projection.title(),
                projection.file_node_id().map(NodeId::as_str),
                file_tags,
                projection.compatibility_sha1(),
                projection.content_sha256(),
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_nodes(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    nodes: &[Node],
) -> Result<(), StoreError> {
    for (ordinal, node) in nodes.iter().enumerate() {
        insert_node(transaction, document_id, ordinal, node)?;
        insert_node_tags(transaction, document_id, node)?;
    }
    Ok(())
}

fn insert_node(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    ordinal: usize,
    node: &Node,
) -> Result<(), StoreError> {
    let title_path =
        serde_json::to_string(node.title_path().parts()).map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO node (\
             document_id, ordinal, node_id, title, title_path_json, parent_id, level\
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                document_id.as_str(),
                to_i64(ordinal, "node ordinal")?,
                node.id().as_str(),
                node.title(),
                title_path,
                node.parent_id().map(NodeId::as_str),
                i64::from(node.level()),
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_node_tags(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    node: &Node,
) -> Result<(), StoreError> {
    for (ordinal, tag) in node.tags().iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO node_tag (document_id, node_id, ordinal, tag) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    document_id.as_str(),
                    node.id().as_str(),
                    to_i64(ordinal, "tag ordinal")?,
                    tag,
                ],
            )
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn insert_occurrences(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    occurrences: &[LinkOccurrence],
) -> Result<(), StoreError> {
    for (ordinal, occurrence) in occurrences.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO link_occurrence (\
                 document_id, ordinal, owner_id, target_id, span_start, span_end\
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    document_id.as_str(),
                    to_i64(ordinal, "occurrence ordinal")?,
                    occurrence.owner().as_str(),
                    occurrence.target().as_str(),
                    to_i64(occurrence.span().start(), "span start")?,
                    to_i64(occurrence.span().end(), "span end")?,
                ],
            )
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn insert_edges(
    transaction: &Transaction<'_>,
    document_id: &DocumentId,
    edges: &[NodeEdge],
) -> Result<(), StoreError> {
    for (ordinal, edge) in edges.iter().enumerate() {
        transaction
            .execute(
                "INSERT INTO node_edge (document_id, ordinal, source_id, target_id) \
                 VALUES (?1, ?2, ?3, ?4)",
                params![
                    document_id.as_str(),
                    to_i64(ordinal, "edge ordinal")?,
                    edge.source().as_str(),
                    edge.target().as_str(),
                ],
            )
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn load_document(
    connection: &Connection,
    document_id: &DocumentId,
) -> Result<Option<StoredDocument>, StoreError> {
    let row = connection
        .query_row(
            "SELECT revision, title, file_node_id, file_tags_json, \
             compatibility_sha1, content_sha256 \
             FROM document WHERE document_id = ?1",
            [document_id.as_str()],
            |row| {
                Ok(DocumentRow {
                    revision: row.get(0)?,
                    title: row.get(1)?,
                    file_node_id: row.get(2)?,
                    file_tags_json: row.get(3)?,
                    compatibility_sha1: row.get(4)?,
                    content_sha256: row.get(5)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    row.map(|row| materialize_document(connection, document_id, row))
        .transpose()
}

struct DocumentRow {
    revision: i64,
    title: String,
    file_node_id: Option<String>,
    file_tags_json: String,
    compatibility_sha1: String,
    content_sha256: String,
}

fn materialize_document(
    connection: &Connection,
    document_id: &DocumentId,
    row: DocumentRow,
) -> Result<StoredDocument, StoreError> {
    let revision =
        u64::try_from(row.revision).map_err(|_| StoreError::CorruptData { field: "revision" })?;
    let nodes = load_nodes(connection, document_id)?;
    let link_occurrences = load_occurrences(connection, document_id)?;
    let stored_edges = load_edges(connection, document_id)?;
    let projection = DocumentProjection::new(DocumentProjectionInput {
        title: row.title,
        file_node_id: row.file_node_id.map(node_id).transpose()?,
        file_tags: from_json(&row.file_tags_json, "file tags")?,
        nodes,
        link_occurrences,
        compatibility_sha1: row.compatibility_sha1,
        content_sha256: row.content_sha256,
    })
    .map_err(|_| StoreError::CorruptData {
        field: "projection",
    })?;
    if projection.semantic_edges() != stored_edges {
        return Err(StoreError::CorruptData {
            field: "semantic edges",
        });
    }
    Ok(StoredDocument {
        document_id: document_id.clone(),
        revision,
        projection,
    })
}

fn load_nodes(connection: &Connection, document_id: &DocumentId) -> Result<Vec<Node>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT node_id, title, title_path_json, parent_id, level \
             FROM node WHERE document_id = ?1 ORDER BY ordinal",
        )
        .map_err(StoreError::storage)?;
    let rows = statement
        .query_map([document_id.as_str()], |row| {
            Ok(NodeRow {
                id: row.get(0)?,
                title: row.get(1)?,
                title_path_json: row.get(2)?,
                parent_id: row.get(3)?,
                level: row.get(4)?,
            })
        })
        .map_err(StoreError::storage)?;
    rows.map(|row| materialize_node(connection, document_id, row.map_err(StoreError::storage)?))
        .collect()
}

struct NodeRow {
    id: String,
    title: String,
    title_path_json: String,
    parent_id: Option<String>,
    level: i64,
}

fn materialize_node(
    connection: &Connection,
    document_id: &DocumentId,
    row: NodeRow,
) -> Result<Node, StoreError> {
    let id = node_id(row.id)?;
    let title_path =
        TitlePath::new(from_json(&row.title_path_json, "title path")?).map_err(|_| {
            StoreError::CorruptData {
                field: "title path",
            }
        })?;
    let tags = load_node_tags(connection, document_id, &id)?;
    let parent = row.parent_id.map(node_id).transpose()?;
    let level = u32::try_from(row.level).map_err(|_| StoreError::CorruptData { field: "level" })?;
    Node::new(id, row.title, title_path, tags, parent, level)
        .map_err(|_| StoreError::CorruptData { field: "node" })
}

fn load_node_tags(
    connection: &Connection,
    document_id: &DocumentId,
    node_id: &NodeId,
) -> Result<Vec<String>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT tag FROM node_tag \
             WHERE document_id = ?1 AND node_id = ?2 ORDER BY ordinal",
        )
        .map_err(StoreError::storage)?;
    let rows = statement
        .query_map(params![document_id.as_str(), node_id.as_str()], |row| {
            row.get(0)
        })
        .map_err(StoreError::storage)?;
    rows.map(|row| row.map_err(StoreError::storage)).collect()
}

fn load_occurrences(
    connection: &Connection,
    document_id: &DocumentId,
) -> Result<Vec<LinkOccurrence>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT owner_id, target_id, span_start, span_end \
             FROM link_occurrence WHERE document_id = ?1 ORDER BY ordinal",
        )
        .map_err(StoreError::storage)?;
    let rows = statement
        .query_map([document_id.as_str()], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .map_err(StoreError::storage)?;
    rows.map(|row| {
        let (owner, target, start, end): (String, String, i64, i64) =
            row.map_err(StoreError::storage)?;
        let span =
            SourceSpan::from_offsets(to_usize(start, "span start")?, to_usize(end, "span end")?)
                .map_err(|_| StoreError::CorruptData {
                    field: "source span",
                })?;
        Ok(LinkOccurrence::new(node_id(owner)?, node_id(target)?, span))
    })
    .collect()
}

fn load_edges(
    connection: &Connection,
    document_id: &DocumentId,
) -> Result<Vec<NodeEdge>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT source_id, target_id FROM node_edge \
             WHERE document_id = ?1 ORDER BY ordinal",
        )
        .map_err(StoreError::storage)?;
    let rows = statement
        .query_map([document_id.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))
        .map_err(StoreError::storage)?;
    rows.map(|row| {
        let (source, target): (String, String) = row.map_err(StoreError::storage)?;
        Ok(NodeEdge::new(node_id(source)?, node_id(target)?))
    })
    .collect()
}

fn node_id(value: String) -> Result<NodeId, StoreError> {
    NodeId::new(value).map_err(|_| StoreError::CorruptData { field: "node ID" })
}

fn from_json(value: &str, field: &'static str) -> Result<Vec<String>, StoreError> {
    serde_json::from_str(value).map_err(|_| StoreError::CorruptData { field })
}

fn to_i64(value: usize, field: &'static str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::CorruptData { field })
}

fn to_usize(value: i64, field: &'static str) -> Result<usize, StoreError> {
    usize::try_from(value).map_err(|_| StoreError::CorruptData { field })
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt};

    use tempfile::tempdir;

    use super::{FailPoint, SqliteStore, StoreError};
    use crate::domain::{
        DocumentId, DocumentProjection, DocumentProjectionInput, LinkOccurrence, Node, NodeId,
        SourceSpan, TitlePath,
    };

    fn projection(title: &str, digest: &str) -> DocumentProjection {
        let root = NodeId::new("root").unwrap();
        DocumentProjection::new(DocumentProjectionInput {
            title: title.into(),
            file_node_id: Some(root.clone()),
            file_tags: vec!["learning".into()],
            nodes: vec![
                Node::new(
                    root.clone(),
                    title.into(),
                    TitlePath::new(vec![title.into()]).unwrap(),
                    vec!["learning".into()],
                    None,
                    0,
                )
                .unwrap(),
            ],
            link_occurrences: vec![
                LinkOccurrence::new(
                    root.clone(),
                    NodeId::new("unresolved").unwrap(),
                    SourceSpan::from_offsets(1, 4).unwrap(),
                ),
                LinkOccurrence::new(
                    root,
                    NodeId::new("unresolved").unwrap(),
                    SourceSpan::from_offsets(8, 12).unwrap(),
                ),
            ],
            compatibility_sha1: "sha1".into(),
            content_sha256: digest.into(),
        })
        .unwrap()
    }

    #[test]
    fn foreign_keys_are_enabled() {
        let store = SqliteStore::open_in_memory().unwrap();
        let enabled: bool = store
            .connection
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert!(enabled);
    }

    #[test]
    fn foreign_keys_reject_orphaned_rows() {
        let store = SqliteStore::open_in_memory().unwrap();
        let error = store
            .connection
            .execute(
                "INSERT INTO node_tag (document_id, node_id, ordinal, tag) \
                 VALUES ('missing-document', 'missing-node', 0, 'tag')",
                [],
            )
            .unwrap_err();
        assert!(error.to_string().contains("FOREIGN KEY constraint failed"));
    }

    #[test]
    fn replacement_preserves_thema_associations() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let id = DocumentId::new("document").unwrap();
        store
            .replace_document(&id, &projection("Original", "digest-one"), None)
            .unwrap();
        store
            .connection
            .execute("INSERT INTO thema (thema_id) VALUES ('thema')", [])
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO thema_concept (thema_id, concept_id, relation) \
                 VALUES ('thema', 'root', 'primary')",
                [],
            )
            .unwrap();

        store
            .replace_document(&id, &projection("Replacement", "digest-two"), Some(1))
            .unwrap();

        let count: u32 = store
            .connection
            .query_row("SELECT COUNT(*) FROM thema_concept", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn forced_mid_write_failure_rolls_back_previous_projection() {
        let directory = tempdir().unwrap();
        let mut store = SqliteStore::open(directory.path().join("gnosis.db")).unwrap();
        let id = DocumentId::new("document").unwrap();
        let original = projection("Original", "digest-one");
        store.replace_document(&id, &original, None).unwrap();

        let error = store
            .replace(
                &id,
                &projection("Replacement", "digest-two"),
                Some(1),
                FailPoint::AfterDocument,
            )
            .unwrap_err();
        assert!(matches!(error, StoreError::Storage(_)));

        let stored = store.get_document(&id).unwrap().unwrap();
        assert_eq!(stored.revision(), 1);
        assert_eq!(stored.projection(), &original);
    }

    #[test]
    fn migrates_and_reopens_database() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gnosis.db");
        assert_eq!(
            SqliteStore::open(&path).unwrap().schema_version().unwrap(),
            1
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            SqliteStore::open(&path).unwrap().schema_version().unwrap(),
            1
        );
    }

    #[test]
    fn round_trips_duplicate_occurrences_and_unique_edges() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let id = DocumentId::new("document").unwrap();
        let inserted = store
            .replace_document(&id, &projection("Document", "digest"), None)
            .unwrap();
        let loaded = store.get_document(&id).unwrap().unwrap();
        assert_eq!(loaded, inserted);
        assert_eq!(loaded.projection().link_occurrences().len(), 2);
        assert_eq!(loaded.projection().semantic_edges().len(), 1);
    }

    #[test]
    fn identical_replacement_is_idempotent() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let id = DocumentId::new("document").unwrap();
        let projection = projection("Document", "digest");
        let first = store.replace_document(&id, &projection, None).unwrap();
        let second = store.replace_document(&id, &projection, None).unwrap();
        assert_eq!(first, second);
        assert_eq!(second.revision(), 1);
    }

    #[test]
    fn stale_revision_preserves_current_projection() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let id = DocumentId::new("document").unwrap();
        store
            .replace_document(&id, &projection("First", "digest-one"), None)
            .unwrap();
        let replacement = projection("Second", "digest-two");
        store.replace_document(&id, &replacement, Some(1)).unwrap();
        let error = store
            .replace_document(&id, &projection("Third", "digest-three"), Some(1))
            .unwrap_err();
        assert!(matches!(
            error,
            StoreError::Conflict {
                expected: 1,
                actual: Some(2)
            }
        ));
        assert_eq!(
            store.get_document(&id).unwrap().unwrap().projection(),
            &replacement
        );
    }
}
