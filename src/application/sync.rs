use super::{
    AssetRevision, DocumentReplicaRevision, NodeCatalogRecord, ReviewStateSnapshot, SettingsRecord,
    ThemaCatalogRecord, ThemaRevision,
};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReviewStateRevision {
    pub(crate) thema_id: String,
    pub(crate) state: ReviewStateSnapshot,
    pub(crate) event_id: Option<String>,
    pub(crate) created_at_us: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SettingsRevision {
    pub(crate) settings: SettingsRecord,
    pub(crate) operation: String,
    pub(crate) created_at_us: u64,
    pub(crate) author_device_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum ReplicaProjection {
    Document(DocumentReplicaRevision),
    Asset(AssetRevision),
    Thema(ThemaRevision),
    Settings(SettingsRevision),
    ReviewState(ReviewStateRevision),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReplicaChange {
    pub(crate) cursor: u64,
    pub(crate) cursor_token: String,
    pub(crate) projection: ReplicaProjection,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ReplicaState {
    pub(crate) server_id: String,
    pub(crate) cursor: u64,
    pub(crate) cursor_token: String,
    pub(crate) next_device_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NodeCatalogPage {
    pub(crate) state: ReplicaState,
    pub(crate) items: Vec<NodeCatalogRecord>,
    pub(crate) has_more: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ThemaCatalogPage {
    pub(crate) state: ReplicaState,
    pub(crate) items: Vec<ThemaCatalogRecord>,
    pub(crate) has_more: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct SnapshotPosition {
    pub(crate) snapshot_id: String,
    pub(crate) server_id: String,
    pub(crate) cut_cursor: u64,
    pub(crate) cut_token: String,
    pub(crate) after: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SnapshotPage {
    pub(crate) position: SnapshotPosition,
    pub(crate) items: Vec<ReplicaProjection>,
    pub(crate) next: Option<SnapshotPosition>,
}
