use std::path::Path;

use serde_json::Value;
use thiserror::Error;

use crate::{
    application::{
        AnswerInput, OfflineAnswerInput, OfflineConflictReason, OfflineReviewOperation,
        ReconciliationStatus,
    },
    protocol::{
        OfflineReviewWireError, ReplicaWireError, canonical_offline_operation, decode_change_page,
        decode_offline_result, decode_replica_state, decode_review_pack, decode_snapshot_cursor,
        decode_snapshot_page,
    },
    store::{
        DeltaInstall, LocalReplicaError as StoreReplicaError, LocalReplicaItem, LocalReplicaStore,
        SnapshotInstall, StoredReviewPack, StoredReviewResult, StoredReviewResultPage,
        canonical_json_sha256,
    },
};

pub struct LocalReplica {
    store: LocalReplicaStore,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingUpload {
    pub attempt_id: String,
    pub device_sequence: u64,
    pub canonical_payload: String,
    pub submission: Value,
    operation: OfflineReviewOperation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum OfflineResponse {
    Text(String),
    Texts(Vec<String>),
    Choice(String),
}

#[derive(Clone, Debug, PartialEq)]
pub struct OfflineAnswer {
    pub attempt_id: String,
    pub session_position: u32,
    pub response: OfflineResponse,
    pub accepted_success: Option<bool>,
    pub desired_suspended: bool,
    pub answered_at_us: u64,
    pub fuzz_sample: f64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedAnswer {
    pub attempt_id: String,
    pub device_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicaStatus {
    pub server_id: Option<String>,
    pub observed_server_id: Option<String>,
    pub cursor: Option<String>,
    pub snapshot_next: Option<String>,
    pub next_review_sequence: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicaProjection {
    pub entity_kind: String,
    pub entity_id: String,
    pub revision: u64,
    pub tombstone: bool,
    pub blob_digests: Vec<String>,
    pub payload: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredOfflineReviewPack {
    pub pack_id: String,
    pub session_id: String,
    pub status: String,
    pub local_position: u32,
    pub issued_at_us: u64,
    pub timezone: String,
    pub day_boundary_hour: u8,
    pub items: Vec<StoredOfflineReviewItem>,
    pub assets: Vec<StoredOfflineReviewAsset>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredOfflineReviewItem {
    pub position: u32,
    pub thema_id: String,
    pub thema_revision: u64,
    pub content_sha256: String,
    pub presentation: CapturedPresentation,
    pub asset_paths: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedPresentation {
    pub kind: String,
    pub keimenon: String,
    pub hypothesis: Vec<Option<String>>,
    pub answer: Vec<String>,
    pub source_guid: Option<String>,
    pub parathema: Option<String>,
    pub review_image: Option<String>,
    pub tags: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredOfflineReviewAsset {
    pub logical_path: String,
    pub content_sha256: String,
    pub size_bytes: u64,
    pub media_type: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OfflineReviewOutcome {
    Accepted,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReviewConflictReason {
    LeaseEnded,
    SessionAdvanced,
    ContentChanged,
    SchedulingChanged,
    CaptureMismatch,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconciledOfflineReview {
    pub result_sequence: u64,
    pub server_id: String,
    pub attempt_id: String,
    pub device_sequence: u64,
    pub outcome: OfflineReviewOutcome,
    pub conflict_reason: Option<ReviewConflictReason>,
    pub event_id: Option<String>,
    pub operation: Value,
    pub authoritative_state: Value,
    pub session: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconciledOfflineReviewPage {
    pub items: Vec<ReconciledOfflineReview>,
    pub next_after: Option<u64>,
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("local replica storage failed")]
    Store,
    #[error("local replica belongs to another device")]
    Device,
    #[error("server identity changed while the outbox is pending")]
    ServerChanged,
    #[error("replica synchronization page is out of order")]
    SyncOrder,
    #[error("replica projection is stale or inconsistent")]
    Projection,
    #[error("replica snapshot changed across a retry")]
    SnapshotChanged,
    #[error("review pack assets are incomplete")]
    MissingAssets,
    #[error("offline answer no longer matches the current pack position")]
    StaleAnswer,
    #[error("offline answer is invalid")]
    Answer,
    #[error("offline operation result is out of order or inconsistent")]
    Result,
    #[error("replica synchronization response is invalid")]
    ReplicaWire,
    #[error("offline review response is invalid")]
    ReviewWire,
    #[error("local replica response is invalid")]
    Response,
}

impl LocalReplica {
    pub fn open(path: impl AsRef<Path>, device_id: &str) -> Result<Self, Error> {
        Ok(Self {
            store: LocalReplicaStore::open(path, device_id)?,
        })
    }

    pub fn observe_state(&mut self, value: Value) -> Result<(), Error> {
        let state = decode_replica_state(value)?;
        self.store
            .observe_server(&state.server_id, state.next_review_sequence)?;
        Ok(())
    }

    pub fn install_snapshot_page(
        &mut self,
        requested: Option<&str>,
        value: Value,
    ) -> Result<(), Error> {
        let response_sha256 = canonical_json_sha256(&value)?;
        let page = decode_snapshot_page(value)?;
        let after = requested_after(requested, &page)?;
        self.store.install_snapshot(SnapshotInstall {
            snapshot_id: page.snapshot_id,
            server_id: page.server_id,
            cut_cursor: page.cut_cursor,
            cut_token: page.cut_token,
            requested: requested.map(str::to_owned),
            after,
            next: page.next,
            next_after: page.next_after,
            cursor: page.cursor,
            response_sha256,
            items: page.items.into_iter().map(LocalReplicaItem::from).collect(),
        })?;
        Ok(())
    }

    pub fn install_change_page(&mut self, value: Value) -> Result<bool, Error> {
        let page = decode_change_page(value)?;
        let has_more = page.has_more;
        self.store.install_delta(DeltaInstall {
            server_id: page.server_id,
            from: page.from,
            next: page.next,
            items: page.items.into_iter().map(LocalReplicaItem::from).collect(),
        })?;
        Ok(has_more)
    }

    pub fn install_review_pack(&mut self, value: Value) -> Result<String, Error> {
        let cursor = string_field(&value, &["pack", "cursor"])?;
        let pack = decode_review_pack(value)?;
        let id = pack.pack_id.clone();
        self.store.install_review_pack(&pack, &cursor)?;
        Ok(id)
    }

    pub fn install_blob(&mut self, digest: &str, bytes: &[u8]) -> Result<(), Error> {
        self.store.install_blob(digest, bytes)?;
        Ok(())
    }

    pub fn missing_blobs(&self) -> Result<Vec<String>, Error> {
        Ok(self.store.missing_blobs()?)
    }

    pub fn blob(&self, digest: &str) -> Result<Option<Vec<u8>>, Error> {
        Ok(self.store.blob(digest)?)
    }

    pub fn projection(
        &self,
        entity_kind: &str,
        entity_id: &str,
    ) -> Result<Option<ReplicaProjection>, Error> {
        self.store
            .projection(entity_kind, entity_id)?
            .map(ReplicaProjection::try_from)
            .transpose()
    }

    pub fn review_pack(&self, pack_id: &str) -> Result<Option<StoredOfflineReviewPack>, Error> {
        Ok(self
            .store
            .review_pack(pack_id)?
            .map(StoredOfflineReviewPack::from))
    }

    pub fn active_review_pack(&self) -> Result<Option<StoredOfflineReviewPack>, Error> {
        Ok(self
            .store
            .active_review_pack()?
            .map(StoredOfflineReviewPack::from))
    }

    pub fn answer(&mut self, pack_id: &str, input: OfflineAnswer) -> Result<RecordedAnswer, Error> {
        let operation = self.store.record_answer(pack_id, input.into())?;
        Ok(RecordedAnswer {
            attempt_id: operation.attempt_id,
            device_sequence: operation.device_sequence,
        })
    }

    pub fn next_upload(&self) -> Result<Option<PendingUpload>, Error> {
        let Some(pending) = self.store.next_pending_operation()? else {
            return Ok(None);
        };
        let operation = pending.operation;
        let canonical_payload =
            canonical_offline_operation(crate::protocol::OFFLINE_REVIEW_VERSION, &operation)?;
        let submission = serde_json::from_str(&canonical_payload).map_err(|_| Error::Response)?;
        Ok(Some(PendingUpload {
            attempt_id: operation.attempt_id.clone(),
            device_sequence: operation.device_sequence,
            operation,
            canonical_payload,
            submission,
        }))
    }

    pub fn record_result(&mut self, value: Value) -> Result<(), Error> {
        let cursor = string_field(&value, &["result", "cursor"])?;
        let result_json = serde_json::to_string(&value).map_err(|_| Error::Response)?;
        let result = decode_offline_result(value)?;
        self.store.record_result(&result, &cursor, &result_json)?;
        Ok(())
    }

    pub fn review_results(
        &self,
        after: Option<u64>,
        limit: u16,
    ) -> Result<ReconciledOfflineReviewPage, Error> {
        self.result_page(after, limit, false)
    }

    pub fn conflicts(
        &self,
        after: Option<u64>,
        limit: u16,
    ) -> Result<ReconciledOfflineReviewPage, Error> {
        self.result_page(after, limit, true)
    }

    fn result_page(
        &self,
        after: Option<u64>,
        limit: u16,
        conflicts_only: bool,
    ) -> Result<ReconciledOfflineReviewPage, Error> {
        let page = self
            .store
            .review_results(after.unwrap_or(0), limit, conflicts_only)?;
        ReconciledOfflineReviewPage::try_from(page)
    }

    pub fn status(&self) -> Result<ReplicaStatus, Error> {
        let status = self.store.status()?;
        Ok(ReplicaStatus {
            server_id: status.server_id,
            observed_server_id: status.observed_server_id,
            cursor: status.cursor,
            snapshot_next: status.snapshot_next,
            next_review_sequence: status.next_review_sequence,
        })
    }

    #[cfg(test)]
    fn counts(&self) -> Result<(u64, u64, u64), Error> {
        Ok(self.store.counts()?)
    }

    #[cfg(test)]
    fn review_progress(&self, pack_id: &str) -> Result<u32, Error> {
        Ok(self.store.review_progress(pack_id)?)
    }
}

impl TryFrom<LocalReplicaItem> for ReplicaProjection {
    type Error = Error;

    fn try_from(value: LocalReplicaItem) -> Result<Self, Self::Error> {
        Ok(Self {
            entity_kind: value.entity_kind,
            entity_id: value.entity_id,
            revision: value.revision,
            tombstone: value.tombstone,
            blob_digests: value.blob_digests,
            payload: serde_json::from_str(&value.payload_json).map_err(|_| Error::Response)?,
        })
    }
}

impl From<StoredReviewPack> for StoredOfflineReviewPack {
    fn from(value: StoredReviewPack) -> Self {
        let pack = value.pack;
        Self {
            pack_id: pack.pack_id,
            session_id: pack.session_id,
            status: value.status,
            local_position: value.local_position,
            issued_at_us: pack.issued_at_us,
            timezone: pack.timezone,
            day_boundary_hour: pack.day_boundary_hour,
            items: pack
                .items
                .into_iter()
                .map(StoredOfflineReviewItem::from)
                .collect(),
            assets: pack
                .assets
                .into_iter()
                .map(StoredOfflineReviewAsset::from)
                .collect(),
        }
    }
}

impl From<crate::application::ReviewPackItem> for StoredOfflineReviewItem {
    fn from(value: crate::application::ReviewPackItem) -> Self {
        Self {
            position: value.position,
            thema_id: value.thema_id,
            thema_revision: value.thema_revision,
            content_sha256: value.content_sha256,
            presentation: value.presentation.into(),
            asset_paths: value.asset_paths,
        }
    }
}

impl From<crate::application::PresentationSnapshot> for CapturedPresentation {
    fn from(value: crate::application::PresentationSnapshot) -> Self {
        Self {
            kind: value.kind,
            keimenon: value.keimenon,
            hypothesis: value.hypothesis,
            answer: value.answer,
            source_guid: value.source_guid,
            parathema: value.parathema,
            review_image: value.review_image,
            tags: value.tags,
        }
    }
}

impl From<crate::application::ReviewPackAsset> for StoredOfflineReviewAsset {
    fn from(value: crate::application::ReviewPackAsset) -> Self {
        Self {
            logical_path: value.logical_path,
            content_sha256: value.content_sha256,
            size_bytes: value.size_bytes,
            media_type: value.media_type,
        }
    }
}

impl TryFrom<StoredReviewResult> for ReconciledOfflineReview {
    type Error = Error;

    fn try_from(value: StoredReviewResult) -> Result<Self, Self::Error> {
        let result_value: Value =
            serde_json::from_str(&value.result_json).map_err(|_| Error::Response)?;
        let authoritative_state = result_field(&result_value, "authoritative_state")?;
        let session = result_field(&result_value, "session")?;
        let result = decode_offline_result(result_value)?;
        let canonical =
            canonical_offline_operation(crate::protocol::OFFLINE_REVIEW_VERSION, &value.operation)?;
        Ok(Self {
            result_sequence: value.sequence,
            server_id: value.server_id,
            attempt_id: value.operation.attempt_id,
            device_sequence: value.operation.device_sequence,
            outcome: result.status.into(),
            conflict_reason: result.conflict_reason.map(ReviewConflictReason::from),
            event_id: result.event_id,
            operation: serde_json::from_str(&canonical).map_err(|_| Error::Response)?,
            authoritative_state,
            session,
        })
    }
}

impl TryFrom<StoredReviewResultPage> for ReconciledOfflineReviewPage {
    type Error = Error;

    fn try_from(value: StoredReviewResultPage) -> Result<Self, Self::Error> {
        Ok(Self {
            items: value
                .items
                .into_iter()
                .map(ReconciledOfflineReview::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            next_after: value.next_after,
        })
    }
}

impl From<ReconciliationStatus> for OfflineReviewOutcome {
    fn from(value: ReconciliationStatus) -> Self {
        match value {
            ReconciliationStatus::Accepted => Self::Accepted,
            ReconciliationStatus::Conflict => Self::Conflict,
        }
    }
}

impl From<OfflineConflictReason> for ReviewConflictReason {
    fn from(value: OfflineConflictReason) -> Self {
        match value {
            OfflineConflictReason::LeaseEnded => Self::LeaseEnded,
            OfflineConflictReason::SessionAdvanced => Self::SessionAdvanced,
            OfflineConflictReason::ContentChanged => Self::ContentChanged,
            OfflineConflictReason::SchedulingChanged => Self::SchedulingChanged,
            OfflineConflictReason::CaptureMismatch => Self::CaptureMismatch,
        }
    }
}

fn result_field(value: &Value, field: &str) -> Result<Value, Error> {
    value
        .get("result")
        .and_then(|result| result.get(field))
        .cloned()
        .ok_or(Error::Response)
}

impl From<crate::protocol::DecodedReplicaItem> for LocalReplicaItem {
    fn from(value: crate::protocol::DecodedReplicaItem) -> Self {
        Self {
            entity_kind: value.entity_kind,
            entity_id: value.entity_id,
            revision: value.revision,
            tombstone: value.tombstone,
            blob_digests: value.blob_digests,
            payload_json: value.payload_json,
        }
    }
}

fn requested_after(
    requested: Option<&str>,
    page: &crate::protocol::DecodedSnapshotPage,
) -> Result<u64, Error> {
    let Some(requested) = requested else {
        return Ok(0);
    };
    let position = decode_snapshot_cursor(requested)?;
    if position.snapshot_id != page.snapshot_id
        || position.server_id != page.server_id
        || position.cut_cursor != page.cut_cursor
        || position.cut_token != page.cut_token
    {
        return Err(Error::Response);
    }
    Ok(position.after)
}

fn string_field(value: &Value, path: &[&str]) -> Result<String, Error> {
    let mut current = value;
    for key in path {
        current = current.get(*key).ok_or(Error::Response)?;
    }
    current.as_str().map(str::to_owned).ok_or(Error::Response)
}

impl From<OfflineAnswer> for OfflineAnswerInput {
    fn from(value: OfflineAnswer) -> Self {
        Self {
            attempt_id: value.attempt_id,
            session_position: value.session_position,
            response: value.response.into(),
            accepted_success: value.accepted_success,
            desired_suspended: value.desired_suspended,
            answered_at_us: value.answered_at_us,
            fuzz_sample: value.fuzz_sample,
        }
    }
}

impl From<OfflineResponse> for AnswerInput {
    fn from(value: OfflineResponse) -> Self {
        match value {
            OfflineResponse::Text(value) => Self::Text(value),
            OfflineResponse::Texts(value) => Self::Texts(value),
            OfflineResponse::Choice(value) => Self::Choice(value),
        }
    }
}

impl From<StoreReplicaError> for Error {
    fn from(value: StoreReplicaError) -> Self {
        match value {
            StoreReplicaError::Device => Self::Device,
            StoreReplicaError::ServerChanged => Self::ServerChanged,
            StoreReplicaError::SyncOrder => Self::SyncOrder,
            StoreReplicaError::Projection => Self::Projection,
            StoreReplicaError::SnapshotChanged => Self::SnapshotChanged,
            StoreReplicaError::MissingAssets => Self::MissingAssets,
            StoreReplicaError::StaleAnswer => Self::StaleAnswer,
            StoreReplicaError::Answer(_) => Self::Answer,
            StoreReplicaError::Result => Self::Result,
            StoreReplicaError::Storage(_)
            | StoreReplicaError::Io(_)
            | StoreReplicaError::Corrupt
            | StoreReplicaError::Schema => Self::Store,
        }
    }
}

impl From<ReplicaWireError> for Error {
    fn from(_: ReplicaWireError) -> Self {
        Self::ReplicaWire
    }
}

impl From<OfflineReviewWireError> for Error {
    fn from(_: OfflineReviewWireError) -> Self {
        Self::ReviewWire
    }
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::{Arc, Barrier},
        thread,
    };

    use serde_json::{Value, json};

    use sha2::{Digest as _, Sha256};

    use super::{Error, LocalReplica, OfflineAnswer, OfflineResponse, ReviewConflictReason};
    use crate::{
        application::{
            NewDevice, ReviewSource, ReviewSourceKind, ServiceHandle, ServiceRuntime,
            canonical_asset_path, generate_device, prepare_asset, prepare_thema, thema_content,
        },
        protocol::{
            OFFLINE_REVIEW_VERSION, decode_cursor, decode_snapshot_cursor, encode_cursor,
            encode_snapshot_cursor, offline_result_json, replica_changes_json,
            replica_snapshot_json, replica_state_json, review_pack_json,
        },
    };

    #[test]
    fn snapshot_resume_is_atomic_and_delta_failure_rolls_back() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/protocol/replica-sync-v1.json"
        ))
        .unwrap();
        let fixture = valid_replica_fixture(fixture);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replica.sqlite");
        let mut replica = LocalReplica::open(&path, "device").unwrap();
        replica.observe_state(fixture["state"].clone()).unwrap();
        replica
            .install_snapshot_page(None, fixture["snapshot_partial"].clone())
            .unwrap();
        assert_eq!(replica.counts().unwrap().0, 0);
        let next = replica.status().unwrap().snapshot_next.unwrap();
        drop(replica);

        let mut replica = LocalReplica::open(&path, "device").unwrap();
        assert_eq!(replica.status().unwrap().snapshot_next, Some(next.clone()));
        replica
            .install_snapshot_page(None, fixture["snapshot_partial"].clone())
            .unwrap();
        replica
            .install_snapshot_page(Some(&next), fixture["snapshot_terminal"].clone())
            .unwrap();
        assert_eq!(replica.counts().unwrap().0, 2);
        assert!(
            replica
                .projection("asset", &asset_path())
                .unwrap()
                .unwrap()
                .tombstone
        );

        replica
            .install_change_page(fixture["changes"].clone())
            .unwrap();
        let cursor = replica.status().unwrap().cursor.unwrap();
        let before = replica.projection("thema", "thema").unwrap().unwrap();
        let malformed = malformed_delta(&fixture, &cursor);
        assert!(matches!(
            replica.install_change_page(malformed),
            Err(Error::ReplicaWire)
        ));
        assert_eq!(replica.status().unwrap().cursor, Some(cursor.clone()));
        assert_eq!(
            replica.projection("thema", "thema").unwrap().unwrap(),
            before
        );
        assert!(matches!(
            replica.install_change_page(noncanonical_asset_delta(&cursor)),
            Err(Error::ReplicaWire)
        ));
        assert_eq!(replica.status().unwrap().cursor, Some(cursor.clone()));
        assert!(matches!(
            replica.install_change_page(invalid_review_delta(&fixture, &cursor)),
            Err(Error::ReplicaWire)
        ));
        assert_eq!(replica.status().unwrap().cursor, Some(cursor.clone()));
        assert!(matches!(
            replica.install_change_page(older_asset_delta(&cursor)),
            Err(Error::Projection)
        ));
        assert!(
            replica
                .projection("asset", &asset_path())
                .unwrap()
                .unwrap()
                .tombstone
        );
    }

    #[test]
    fn a_superseded_partial_snapshot_restarts_without_touching_active_rows() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/protocol/replica-sync-v1.json"
        ))
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replica.sqlite");
        let mut replica = LocalReplica::open(&path, "device").unwrap();
        replica.observe_state(fixture["state"].clone()).unwrap();
        replica
            .install_snapshot_page(None, fixture["snapshot_partial"].clone())
            .unwrap();

        let mut replacement = fixture["snapshot_partial"].clone();
        replacement["snapshot_id"] = json!("snapshot-2");
        let mut position = decode_snapshot_cursor(replacement["next"].as_str().unwrap()).unwrap();
        position.snapshot_id = "snapshot-2".into();
        let replacement_next = encode_snapshot_cursor(&position).unwrap();
        replacement["next"] = json!(replacement_next.clone());
        replica.install_snapshot_page(None, replacement).unwrap();
        assert_eq!(replica.counts().unwrap().0, 0);

        let mut terminal = fixture["snapshot_terminal"].clone();
        terminal["snapshot_id"] = json!("snapshot-2");
        replica
            .install_snapshot_page(Some(&replacement_next), terminal)
            .unwrap();
        assert_eq!(replica.counts().unwrap().0, 2);
        assert_eq!(snapshot_page_count(&path), 2);
    }

    #[test]
    fn concurrent_first_open_claims_one_replica_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replica.sqlite");
        let barrier = Arc::new(Barrier::new(2));
        let handles = (0..2)
            .map(|_| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    barrier.wait();
                    LocalReplica::open(path, "device").is_ok()
                })
            })
            .collect::<Vec<_>>();
        assert!(handles.into_iter().all(|handle| handle.join().unwrap()));
        assert_eq!(
            LocalReplica::open(path, "device")
                .unwrap()
                .status()
                .unwrap()
                .next_review_sequence,
            1
        );
    }

    #[tokio::test]
    async fn offline_answers_survive_restarts_and_reconcile_in_order() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("server.sqlite");
        let blobs = directory.path().join("server-blobs");
        let replica_path = directory.path().join("replica.sqlite");
        let runtime = ServiceRuntime::start(database.clone(), blobs.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let device = generate_device("offline".into(), vec!["review".into()]).unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let asset = add_shared_asset(&service, &device_id).await;
        add_thema(&service, "first", "First", &asset, &device_id, 3).await;
        add_thema(&service, "second", "Second", &asset, &device_id, 4).await;

        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        let state = service.replica_state(device_id.clone()).await.unwrap();
        replica
            .observe_state(replica_state_json(&state).unwrap())
            .unwrap();
        install_server_snapshot(&mut replica, &service, &device_id).await;
        install_missing_blobs(&mut replica, &service).await;
        assert_eq!(replica.counts().unwrap().1, 1);

        let issued_at_us = 1_752_600_000_000_000;
        let pack = service
            .create_review_pack(
                OFFLINE_REVIEW_VERSION,
                manual_source(&["first", "second"]),
                Some(2),
                device_id.clone(),
                issued_at_us,
            )
            .await
            .unwrap();
        let pack_id = replica
            .install_review_pack(review_pack_json(&pack).unwrap())
            .unwrap();
        assert!(replica.missing_blobs().unwrap().is_empty());
        replica
            .answer(&pack_id, answer("attempt-1", 0, issued_at_us + 1_000))
            .unwrap();
        replica
            .answer(&pack_id, answer("attempt-1", 0, issued_at_us + 1_000))
            .unwrap();
        assert_eq!(replica.review_progress(&pack_id).unwrap(), 1);
        drop(replica);

        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        let resumed = replica.active_review_pack().unwrap().unwrap();
        assert_eq!(resumed.pack_id, pack_id);
        assert_eq!(resumed.local_position, 1);
        assert_eq!(resumed.items[1].presentation.keimenon, "Second");
        let asset_digest = &resumed.assets[0].content_sha256;
        assert_eq!(
            replica.blob(asset_digest).unwrap().unwrap(),
            b"shared offline asset"
        );
        replica
            .answer(&pack_id, answer("attempt-2", 1, issued_at_us + 2_000))
            .unwrap();
        assert_eq!(replica.review_progress(&pack_id).unwrap(), 2);
        assert!(replica.active_review_pack().unwrap().is_none());
        assert_eq!(
            replica.review_pack(&pack_id).unwrap().unwrap().status,
            "completed"
        );
        let cursor_before_results = replica.status().unwrap().cursor;

        drop(service);
        runtime.finish().await.unwrap();
        let runtime = ServiceRuntime::start(database, blobs, 8).await.unwrap();
        let service = runtime.handle();
        reconcile_next_with_lost_response(&mut replica, &service, &device_id, issued_at_us + 3_000)
            .await;
        assert_eq!(replica.status().unwrap().cursor, cursor_before_results);
        reconcile_next(&mut replica, &service, &device_id, issued_at_us + 4_000).await;
        assert!(replica.next_upload().unwrap().is_none());
        assert_eq!(replica.counts().unwrap().2, 2);
        let first_page = replica.review_results(None, 1).unwrap();
        assert_eq!(first_page.items.len(), 1);
        let second_page = replica.review_results(first_page.next_after, 1).unwrap();
        assert_eq!(second_page.items.len(), 1);
        assert!(second_page.next_after.is_none());
        assert_eq!(
            first_page.items[0].server_id,
            second_page.items[0].server_id
        );
        assert!(replica.conflicts(None, 1).unwrap().items.is_empty());

        pull_server_changes(&mut replica, &service).await;
        let server_state = service.replica_state(device_id).await.unwrap();
        let server_cursor = replica_state_json(&server_state).unwrap()["cursor"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_eq!(replica.status().unwrap().cursor, Some(server_cursor));
        for thema_id in ["first", "second"] {
            let review = replica.projection("review", thema_id).unwrap().unwrap();
            assert_eq!(review.revision, 1);
            assert_eq!(review.payload["projection"]["state"]["review_count"], 1);
        }
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn expired_cursor_recovery_preserves_a_stale_attempt_as_a_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("server.sqlite"),
            directory.path().join("server-blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device("offline".into(), vec!["review".into()]).unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let asset = add_shared_asset(&service, &device_id).await;
        add_thema(&service, "first", "First", &asset, &device_id, 3).await;
        let replica_path = directory.path().join("replica.sqlite");
        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        let state = service.replica_state(device_id.clone()).await.unwrap();
        replica
            .observe_state(replica_state_json(&state).unwrap())
            .unwrap();
        install_server_snapshot(&mut replica, &service, &device_id).await;
        install_missing_blobs(&mut replica, &service).await;

        let issued_at_us = 1_752_600_000_000_000;
        let pack = service
            .create_review_pack(
                OFFLINE_REVIEW_VERSION,
                manual_source(&["first"]),
                Some(1),
                device_id.clone(),
                issued_at_us,
            )
            .await
            .unwrap();
        let pack_id = replica
            .install_review_pack(review_pack_json(&pack).unwrap())
            .unwrap();
        replica
            .answer(&pack_id, answer("stale-attempt", 0, issued_at_us + 1_000))
            .unwrap();

        let changed = thema_content(
            "basic",
            "Changed".into(),
            vec![None],
            vec!["answer".into()],
            None,
            None,
            Some(asset.clone()),
        )
        .unwrap();
        service
            .put_thema(
                prepare_thema(
                    Some("first".into()),
                    changed,
                    vec![],
                    vec![],
                    vec![asset],
                    Some(1),
                )
                .unwrap(),
                20260715,
                4,
                Some(device_id.clone()),
            )
            .await
            .unwrap();
        let changed_state = service.replica_state(device_id.clone()).await.unwrap();
        service
            .expire_replica_changes_through(changed_state.cursor)
            .await
            .unwrap();
        replica
            .observe_state(replica_state_json(&changed_state).unwrap())
            .unwrap();
        install_server_snapshot(&mut replica, &service, &device_id).await;
        assert_eq!(
            replica.next_upload().unwrap().unwrap().attempt_id,
            "stale-attempt"
        );

        let cursor_before_result = replica.status().unwrap().cursor;
        let upload = replica.next_upload().unwrap().unwrap();
        let result = reconcile_upload(&service, &upload, &device_id, issued_at_us + 2_000).await;
        assert_eq!(result["result"]["status"], "conflict");
        assert_eq!(result["result"]["conflict_reason"], "content_changed");
        replica.record_result(result).unwrap();
        drop(replica);
        let replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        let conflicts = replica.conflicts(None, 10).unwrap();
        assert_eq!(conflicts.items.len(), 1);
        assert_eq!(conflicts.items[0].attempt_id, "stale-attempt");
        assert_eq!(
            conflicts.items[0].conflict_reason,
            Some(ReviewConflictReason::ContentChanged)
        );
        assert_eq!(
            conflicts.items[0].operation["operation"]["content_sha256"],
            pack.items[0].content_sha256
        );
        assert_eq!(replica.status().unwrap().cursor, cursor_before_result);
        assert!(replica.next_upload().unwrap().is_none());
        assert_eq!(replica.counts().unwrap().2, 1);
        assert_eq!(
            replica
                .projection("review", "first")
                .unwrap()
                .unwrap()
                .revision,
            0
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn a_new_server_can_restart_review_sequence_after_old_results() {
        let directory = tempfile::tempdir().unwrap();
        let replica_path = directory.path().join("replica.sqlite");
        let device = generate_device("offline".into(), vec!["review".into()]).unwrap();
        let device_id = device.device_id.clone();
        let (runtime, service) =
            start_review_server(directory.path(), "first", device.clone()).await;
        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        sync_server(&mut replica, &service, &device_id).await;
        let issued_at_us = 1_752_600_000_000_000;
        let pack_id = install_single_pack(&mut replica, &service, &device_id, issued_at_us).await;
        let recorded = replica
            .answer(&pack_id, answer("old-attempt", 0, issued_at_us + 1_000))
            .unwrap();
        assert_eq!(recorded.device_sequence, 1);
        reconcile_next(&mut replica, &service, &device_id, issued_at_us + 2_000).await;
        drop(service);
        runtime.finish().await.unwrap();

        let (runtime, service) = start_review_server(directory.path(), "second", device).await;
        sync_server(&mut replica, &service, &device_id).await;
        let pack_id = install_single_pack(&mut replica, &service, &device_id, issued_at_us).await;
        let recorded = replica
            .answer(&pack_id, answer("new-attempt", 0, issued_at_us + 1_000))
            .unwrap();
        assert_eq!(recorded.device_sequence, 1);
        assert_eq!(
            replica.next_upload().unwrap().unwrap().attempt_id,
            "new-attempt"
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn a_stale_handle_cannot_answer_the_next_pack_item() {
        let directory = tempfile::tempdir().unwrap();
        let replica_path = directory.path().join("replica.sqlite");
        let device = generate_device("offline".into(), vec!["review".into()]).unwrap();
        let device_id = device.device_id.clone();
        let (runtime, service) = start_review_server(directory.path(), "server", device).await;
        let digest = format!("{:x}", Sha256::digest(b"shared offline asset"));
        let asset = canonical_asset_path(&digest, "shared.png").unwrap();
        add_thema(&service, "second", "Second", &asset, &device_id, 4).await;
        let mut first = LocalReplica::open(&replica_path, &device_id).unwrap();
        sync_server(&mut first, &service, &device_id).await;
        let issued_at_us = 1_752_600_000_000_000;
        let pack = service
            .create_review_pack(
                OFFLINE_REVIEW_VERSION,
                manual_source(&["first", "second"]),
                Some(2),
                device_id.clone(),
                issued_at_us,
            )
            .await
            .unwrap();
        let pack_id = first
            .install_review_pack(review_pack_json(&pack).unwrap())
            .unwrap();
        let mut stale = LocalReplica::open(&replica_path, &device_id).unwrap();
        assert_eq!(
            stale.active_review_pack().unwrap().unwrap().local_position,
            0
        );
        first
            .answer(&pack_id, answer("first-attempt", 0, issued_at_us + 1_000))
            .unwrap();
        assert!(matches!(
            stale.answer(&pack_id, answer("stale-attempt", 0, issued_at_us + 2_000)),
            Err(Error::StaleAnswer)
        ));
        assert_eq!(
            stale.review_pack(&pack_id).unwrap().unwrap().local_position,
            1
        );
        assert_eq!(stale.status().unwrap().next_review_sequence, 2);
        assert_eq!(
            stale.next_upload().unwrap().unwrap().attempt_id,
            "first-attempt"
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn an_authoritative_replacement_pack_supersedes_local_progress() {
        let directory = tempfile::tempdir().unwrap();
        let replica_path = directory.path().join("replica.sqlite");
        let device = generate_device("offline".into(), vec!["review".into()]).unwrap();
        let device_id = device.device_id.clone();
        let (runtime, service) = start_review_server(directory.path(), "server", device).await;
        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        sync_server(&mut replica, &service, &device_id).await;
        let issued_at_us = 1_752_600_000_000_000;
        let old_pack = install_single_pack(&mut replica, &service, &device_id, issued_at_us).await;
        service
            .abandon_review_pack(
                old_pack.clone(),
                Some(device_id.clone()),
                issued_at_us + 1_000,
            )
            .await
            .unwrap();
        let replacement =
            install_single_pack(&mut replica, &service, &device_id, issued_at_us + 2_000).await;
        assert_eq!(
            replica.review_pack(&old_pack).unwrap().unwrap().status,
            "abandoned"
        );
        assert_eq!(
            replica.active_review_pack().unwrap().unwrap().pack_id,
            replacement
        );
        assert!(matches!(
            replica.answer(
                &old_pack,
                answer("old-pack-attempt", 0, issued_at_us + 3_000)
            ),
            Err(Error::Answer)
        ));
        replica
            .answer(
                &replacement,
                answer("replacement-attempt", 0, issued_at_us + 3_000),
            )
            .unwrap();
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn an_inactive_historical_pack_does_not_supersede_the_active_pack() {
        let directory = tempfile::tempdir().unwrap();
        let replica_path = directory.path().join("replica.sqlite");
        let device = generate_device("offline".into(), vec!["review".into()]).unwrap();
        let device_id = device.device_id.clone();
        let (runtime, service) = start_review_server(directory.path(), "server", device).await;
        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        sync_server(&mut replica, &service, &device_id).await;
        let issued_at_us = 1_752_600_000_000_000;
        let historical = service
            .create_review_pack(
                OFFLINE_REVIEW_VERSION,
                manual_source(&["first"]),
                Some(1),
                device_id.clone(),
                issued_at_us,
            )
            .await
            .unwrap();
        let historical = service
            .abandon_review_pack(
                historical.pack_id,
                Some(device_id.clone()),
                issued_at_us + 1_000,
            )
            .await
            .unwrap();
        let active = service
            .create_review_pack(
                OFFLINE_REVIEW_VERSION,
                manual_source(&["first"]),
                Some(1),
                device_id,
                issued_at_us + 2_000,
            )
            .await
            .unwrap();
        let active_id = replica
            .install_review_pack(review_pack_json(&active).unwrap())
            .unwrap();
        replica
            .install_review_pack(review_pack_json(&historical).unwrap())
            .unwrap();
        assert_eq!(
            replica.active_review_pack().unwrap().unwrap().pack_id,
            active_id
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn malformed_pack_and_result_responses_never_advance_local_state() {
        let directory = tempfile::tempdir().unwrap();
        let replica_path = directory.path().join("replica.sqlite");
        let device = generate_device("offline".into(), vec!["review".into()]).unwrap();
        let device_id = device.device_id.clone();
        let (runtime, service) = start_review_server(directory.path(), "server", device).await;
        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        sync_server(&mut replica, &service, &device_id).await;
        let issued_at_us = 1_752_600_000_000_000;
        let pack = service
            .create_review_pack(
                OFFLINE_REVIEW_VERSION,
                manual_source(&["first"]),
                Some(1),
                device_id.clone(),
                issued_at_us,
            )
            .await
            .unwrap();
        let wire = review_pack_json(&pack).unwrap();
        let mut exhausted = wire.clone();
        exhausted["pack"]["current_position"] = json!(1);
        assert!(matches!(
            replica.install_review_pack(exhausted),
            Err(Error::ReviewWire)
        ));
        let mut invalid_state = wire.clone();
        invalid_state["pack"]["items"][0]["review_state"]["review_count"] = json!(1);
        assert!(matches!(
            replica.install_review_pack(invalid_state),
            Err(Error::ReviewWire)
        ));
        let mut invalid_asset = wire.clone();
        invalid_asset["pack"]["assets"][0]["content_sha256"] = json!("b".repeat(64));
        assert!(matches!(
            replica.install_review_pack(invalid_asset),
            Err(Error::ReviewWire)
        ));
        let mut missing_presentation_asset = wire.clone();
        missing_presentation_asset["pack"]["items"][0]["asset_paths"] = json!([]);
        assert!(matches!(
            replica.install_review_pack(missing_presentation_asset),
            Err(Error::ReviewWire)
        ));
        let pack_id = replica.install_review_pack(wire).unwrap();
        replica
            .answer(&pack_id, answer("attempt", 0, issued_at_us + 1_000))
            .unwrap();
        let upload = replica.next_upload().unwrap().unwrap();
        let valid = reconcile_upload(&service, &upload, &device_id, issued_at_us + 2_000).await;

        let mut invalid_session = valid.clone();
        invalid_session["result"]["session"]["status"] = json!("bogus");
        assert!(matches!(
            replica.record_result(invalid_session),
            Err(Error::ReviewWire)
        ));
        let mut invalid_result_state = valid.clone();
        invalid_result_state["result"]["authoritative_state"]["review_count"] = json!(2);
        assert!(matches!(
            replica.record_result(invalid_result_state),
            Err(Error::ReviewWire)
        ));
        let mut wrong_transition = valid.clone();
        let suspended = wrong_transition["result"]["authoritative_state"]["suspended"]
            .as_bool()
            .unwrap();
        wrong_transition["result"]["authoritative_state"]["suspended"] = json!(!suspended);
        assert!(matches!(
            replica.record_result(wrong_transition),
            Err(Error::Result)
        ));
        let mut impossible_session = valid.clone();
        impossible_session["result"]["session"]["status"] = json!("quit");
        assert!(matches!(
            replica.record_result(impossible_session),
            Err(Error::Result)
        ));
        assert_eq!(
            replica.next_upload().unwrap().unwrap().attempt_id,
            "attempt"
        );
        replica.record_result(valid).unwrap();
        assert!(replica.next_upload().unwrap().is_none());
        drop(service);
        runtime.finish().await.unwrap();
    }

    async fn start_review_server(
        directory: &Path,
        name: &str,
        device: NewDevice,
    ) -> (ServiceRuntime, ServiceHandle) {
        let runtime = ServiceRuntime::start(
            directory.join(format!("{name}.sqlite")),
            directory.join(format!("{name}-blobs")),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let asset = add_shared_asset(&service, &device_id).await;
        add_thema(&service, "first", "First", &asset, &device_id, 3).await;
        (runtime, service)
    }

    async fn sync_server(replica: &mut LocalReplica, service: &ServiceHandle, device_id: &str) {
        let state = service.replica_state(device_id.into()).await.unwrap();
        replica
            .observe_state(replica_state_json(&state).unwrap())
            .unwrap();
        install_server_snapshot(replica, service, device_id).await;
        install_missing_blobs(replica, service).await;
    }

    async fn install_single_pack(
        replica: &mut LocalReplica,
        service: &ServiceHandle,
        device_id: &str,
        issued_at_us: u64,
    ) -> String {
        let pack = service
            .create_review_pack(
                OFFLINE_REVIEW_VERSION,
                manual_source(&["first"]),
                Some(1),
                device_id.into(),
                issued_at_us,
            )
            .await
            .unwrap();
        replica
            .install_review_pack(review_pack_json(&pack).unwrap())
            .unwrap()
    }

    async fn add_shared_asset(service: &ServiceHandle, device_id: &str) -> String {
        let bytes = b"shared offline asset".to_vec();
        let digest = format!("{:x}", Sha256::digest(&bytes));
        let path = canonical_asset_path(&digest, "shared.png").unwrap();
        service
            .put_asset(
                prepare_asset(path.clone(), "shared.png".into(), None).unwrap(),
                bytes,
                "image/png".into(),
                2,
                Some(device_id.into()),
            )
            .await
            .unwrap();
        path
    }

    async fn add_thema(
        service: &ServiceHandle,
        id: &str,
        question: &str,
        asset: &str,
        device_id: &str,
        updated_at_us: i64,
    ) {
        let content = thema_content(
            "basic",
            question.into(),
            vec![None],
            vec!["answer".into()],
            None,
            None,
            Some(asset.into()),
        )
        .unwrap();
        service
            .put_thema(
                prepare_thema(
                    Some(id.into()),
                    content,
                    vec![],
                    vec![],
                    vec![asset.into()],
                    None,
                )
                .unwrap(),
                20260715,
                updated_at_us,
                Some(device_id.into()),
            )
            .await
            .unwrap();
    }

    fn manual_source(ids: &[&str]) -> ReviewSource {
        ReviewSource {
            kind: ReviewSourceKind::Manual,
            tags: vec![],
            exclude_tags: vec![],
            topic_node_id: None,
            topic_forward_depth: None,
            topic_backward_depth: None,
            manual_ids: ids.iter().map(|id| (*id).into()).collect(),
        }
    }

    fn answer(attempt_id: &str, session_position: u32, answered_at_us: u64) -> OfflineAnswer {
        OfflineAnswer {
            attempt_id: attempt_id.into(),
            session_position,
            response: OfflineResponse::Text("answer".into()),
            accepted_success: None,
            desired_suspended: false,
            answered_at_us,
            fuzz_sample: -0.25,
        }
    }

    async fn install_server_snapshot(
        replica: &mut LocalReplica,
        service: &ServiceHandle,
        device_id: &str,
    ) {
        let mut requested = None;
        let mut page = service
            .create_replica_snapshot(device_id.into(), 5, 2)
            .await
            .unwrap();
        loop {
            let wire = replica_snapshot_json(&page).unwrap();
            replica
                .install_snapshot_page(requested.as_deref(), wire)
                .unwrap();
            let Some(next) = replica.status().unwrap().snapshot_next else {
                break;
            };
            page = service
                .replica_snapshot_page(device_id.into(), decode_snapshot_cursor(&next).unwrap(), 2)
                .await
                .unwrap();
            requested = Some(next);
        }
    }

    async fn install_missing_blobs(replica: &mut LocalReplica, service: &ServiceHandle) {
        for digest in replica.missing_blobs().unwrap() {
            let (_, bytes) = service.blob(digest.clone()).await.unwrap().unwrap();
            replica.install_blob(&digest, &bytes).unwrap();
        }
    }

    async fn reconcile_next_with_lost_response(
        replica: &mut LocalReplica,
        service: &ServiceHandle,
        device_id: &str,
        received_at_us: u64,
    ) {
        let upload = replica.next_upload().unwrap().unwrap();
        let first = reconcile_upload(service, &upload, device_id, received_at_us).await;
        let retry = reconcile_upload(service, &upload, device_id, received_at_us + 1).await;
        assert_eq!(first, retry);
        replica.record_result(retry).unwrap();
    }

    async fn reconcile_next(
        replica: &mut LocalReplica,
        service: &ServiceHandle,
        device_id: &str,
        received_at_us: u64,
    ) {
        let upload = replica.next_upload().unwrap().unwrap();
        let result = reconcile_upload(service, &upload, device_id, received_at_us).await;
        replica.record_result(result).unwrap();
    }

    async fn reconcile_upload(
        service: &ServiceHandle,
        upload: &super::PendingUpload,
        device_id: &str,
        received_at_us: u64,
    ) -> Value {
        let result = service
            .reconcile_offline_operation(
                OFFLINE_REVIEW_VERSION,
                upload.operation.clone(),
                upload.canonical_payload.clone(),
                Some(device_id.into()),
                received_at_us,
            )
            .await
            .unwrap();
        offline_result_json(&result).unwrap()
    }

    async fn pull_server_changes(replica: &mut LocalReplica, service: &ServiceHandle) {
        loop {
            let cursor = replica.status().unwrap().cursor.unwrap();
            let (server_id, after, token) = decode_cursor(&cursor).unwrap();
            let (changes, latest, _) = service
                .replica_changes(server_id.clone(), after, token.clone(), 2)
                .await
                .unwrap();
            let page = replica_changes_json(&server_id, after, &token, latest, &changes).unwrap();
            replica.install_change_page(page).unwrap();
            if decode_cursor(replica.status().unwrap().cursor.as_deref().unwrap())
                .unwrap()
                .1
                >= latest
            {
                break;
            }
        }
    }

    fn malformed_delta(fixture: &Value, from: &str) -> Value {
        let server = "0123456789abcdef0123456789abcdef";
        let cursor_11 = encode_cursor(server, 11, &"5".repeat(32)).unwrap();
        let cursor_12 = encode_cursor(server, 12, &"6".repeat(32)).unwrap();
        let mut thema = fixture["changes"]["items"][0].clone();
        thema["cursor"] = json!(cursor_11);
        thema["projection"]["revision"] = json!(4);
        thema["projection"]["keimenon"] = json!("Changed");
        let mut stale_review = fixture["changes"]["items"][2].clone();
        stale_review["cursor"] = json!(cursor_12.clone());
        stale_review["projection"]["state"]["state_revision"] = json!(0);
        json!({
            "version": 1,
            "from": from,
            "next": cursor_12,
            "has_more": false,
            "items": [thema, stale_review]
        })
    }

    fn valid_replica_fixture(mut fixture: Value) -> Value {
        let content = thema_content(
            "basic",
            "Question".into(),
            vec![None],
            vec!["Answer".into()],
            Some("source".into()),
            None,
            None,
        )
        .unwrap();
        let prepared = prepare_thema(
            Some("thema".into()),
            content,
            vec!["tag".into()],
            vec!["node".into()],
            vec![],
            None,
        )
        .unwrap();
        fixture["changes"]["items"][0]["projection"]["content_sha256"] =
            json!(prepared.content_sha256);
        fixture
    }

    fn older_asset_delta(from: &str) -> Value {
        let server = "0123456789abcdef0123456789abcdef";
        let next = encode_cursor(server, 11, &"7".repeat(32)).unwrap();
        let digest = "c".repeat(64);
        json!({
            "version": 1,
            "from": from,
            "next": next.clone(),
            "has_more": false,
            "items": [{
                "cursor": next,
                "entity_kind": "asset",
                "projection": {
                    "logical_path": asset_path(),
                    "revision": 1,
                    "content_sha256": digest,
                    "size_bytes": 1,
                    "media_type": "image/png",
                    "blob_url": format!("/v1/blobs/{digest}"),
                    "operation": "create",
                    "created_at_us": 25,
                    "author_device_id": "device"
                }
            }]
        })
    }

    fn noncanonical_asset_delta(from: &str) -> Value {
        let server = "0123456789abcdef0123456789abcdef";
        let next = encode_cursor(server, 11, &"8".repeat(32)).unwrap();
        let digest = "c".repeat(64);
        json!({
            "version": 1,
            "from": from,
            "next": next.clone(),
            "has_more": false,
            "items": [{
                "cursor": next,
                "entity_kind": "asset",
                "projection": {
                    "logical_path": "note.org",
                    "revision": 3,
                    "content_sha256": digest,
                    "size_bytes": 1,
                    "media_type": "image/png",
                    "blob_url": format!("/v1/blobs/{digest}"),
                    "operation": "create",
                    "created_at_us": 25,
                    "author_device_id": "device"
                }
            }]
        })
    }

    fn invalid_review_delta(fixture: &Value, from: &str) -> Value {
        let server = "0123456789abcdef0123456789abcdef";
        let next = encode_cursor(server, 11, &"9".repeat(32)).unwrap();
        let mut review = fixture["changes"]["items"][2].clone();
        review["cursor"] = json!(next.clone());
        review["projection"]["state"]["review_count"] = json!(2);
        json!({
            "version": 1,
            "from": from,
            "next": next,
            "has_more": false,
            "items": [review]
        })
    }

    fn asset_path() -> String {
        format!("assets/{}/image.png", "c".repeat(64))
    }

    fn snapshot_page_count(path: &Path) -> u64 {
        rusqlite::Connection::open(path)
            .unwrap()
            .query_row("SELECT count(*) FROM replica_snapshot_page", [], |row| {
                row.get(0)
            })
            .unwrap()
    }
}
