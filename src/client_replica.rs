use std::{fmt, path::Path};

use serde_json::Value;
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::{
    application::{
        AnswerInput, AssetReconciliationStatus, AuthoritativeAsset, AuthoritativeDocument,
        ContentBase, ContentReconciliationStatus, OfflineAnswerInput, OfflineAssetConflictReason,
        OfflineAssetOperation, OfflineConflictReason, OfflineContentConflictReason,
        OfflineContentMutation, OfflineContentOperation, OfflineReviewResult, ReconciliationStatus,
        asset_paths_in_org, canonical_asset_path, validate_asset_path,
    },
    protocol::{
        DecodedOfflineAssetResult, DecodedOfflineContentResult, OfflineAssetSubmission,
        OfflineAssetWireError, OfflineContentSubmission, OfflineContentWireError,
        OfflineReviewWireError, ReplicaWireError, canonical_asset_operation,
        canonical_content_operation, canonical_offline_operation, decode_change_page,
        decode_cursor, decode_offline_asset_capabilities, decode_offline_asset_result,
        decode_offline_content_capabilities, decode_offline_content_result, decode_offline_result,
        decode_replica_state, decode_review_pack, decode_snapshot_cursor, decode_snapshot_page,
        http_v1::{
            AssetCapabilitiesV1, ContentCapabilitiesV1, ReplicaChangePageV1, ReplicaSnapshotPageV1,
            ReplicaStateV1, ReviewPackEnvelopeV2,
        },
    },
    store::{
        AssetResultInstall, ContentResultInstall, DeltaInstall,
        LocalReplicaError as StoreReplicaError, LocalReplicaItem, LocalReplicaStore,
        SnapshotInstall, StoredAssetResult, StoredAssetResultPage, StoredContentResult,
        StoredContentResultPage, StoredReviewPack, StoredReviewResult, StoredReviewResultPage,
        canonical_json_sha256,
    },
};

pub struct LocalReplica {
    store: LocalReplicaStore,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingUpload {
    attempt_id: String,
    device_sequence: u64,
    canonical_payload: String,
}

impl PendingUpload {
    pub fn attempt_id(&self) -> &str {
        &self.attempt_id
    }

    pub const fn device_sequence(&self) -> u64 {
        self.device_sequence
    }

    pub fn canonical_payload(&self) -> &str {
        &self.canonical_payload
    }

    pub fn submission(&self) -> Result<Value, Error> {
        submission_value(&self.canonical_payload)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingContentUpload {
    operation_id: String,
    device_sequence: u64,
    document_id: String,
    canonical_payload: String,
}

impl PendingContentUpload {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub const fn device_sequence(&self) -> u64 {
        self.device_sequence
    }

    pub fn document_id(&self) -> &str {
        &self.document_id
    }

    pub fn canonical_payload(&self) -> &str {
        &self.canonical_payload
    }

    pub fn submission(&self) -> Result<Value, Error> {
        submission_value(&self.canonical_payload)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum PendingDeviceUpload {
    Review(Box<PendingUpload>),
    Content(PendingContentUpload),
    Asset(PendingAssetUpload),
}

impl PendingDeviceUpload {
    pub const fn kind(&self) -> DeviceUploadKind {
        match self {
            Self::Review(_) => DeviceUploadKind::Review,
            Self::Content(_) => DeviceUploadKind::Content,
            Self::Asset(_) => DeviceUploadKind::Asset,
        }
    }

    pub fn operation_id(&self) -> &str {
        match self {
            Self::Review(upload) => &upload.attempt_id,
            Self::Content(upload) => &upload.operation_id,
            Self::Asset(upload) => &upload.operation_id,
        }
    }

    pub const fn device_sequence(&self) -> u64 {
        match self {
            Self::Review(upload) => upload.device_sequence,
            Self::Content(upload) => upload.device_sequence,
            Self::Asset(upload) => upload.device_sequence,
        }
    }

    pub(crate) fn canonical_payload(&self) -> &str {
        match self {
            Self::Review(upload) => &upload.canonical_payload,
            Self::Content(upload) => &upload.canonical_payload,
            Self::Asset(upload) => &upload.canonical_payload,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceUploadKind {
    Review,
    Content,
    Asset,
}

pub struct DeviceUploadResult {
    result_json: String,
    result: DecodedDeviceUploadResult,
}

impl DeviceUploadResult {
    pub(crate) fn decode(kind: DeviceUploadKind, value: Value) -> Result<Self, Error> {
        let result_json = serde_json::to_string(&value).map_err(|_| Error::Response)?;
        let result = match kind {
            DeviceUploadKind::Review => DecodedDeviceUploadResult::Review {
                cursor: string_field(&value, &["result", "cursor"])?,
                result: Box::new(decode_offline_result(value)?),
            },
            DeviceUploadKind::Content => {
                DecodedDeviceUploadResult::Content(decode_offline_content_result(value)?)
            }
            DeviceUploadKind::Asset => {
                DecodedDeviceUploadResult::Asset(decode_offline_asset_result(value)?)
            }
        };
        Ok(Self {
            result_json,
            result,
        })
    }

    pub const fn kind(&self) -> DeviceUploadKind {
        self.result.kind()
    }

    pub fn operation_id(&self) -> &str {
        self.result.operation_id()
    }

    fn into_parts(self) -> (String, DecodedDeviceUploadResult) {
        (self.result_json, self.result)
    }
}

enum DecodedDeviceUploadResult {
    Review {
        cursor: String,
        result: Box<OfflineReviewResult>,
    },
    Content(DecodedOfflineContentResult),
    Asset(DecodedOfflineAssetResult),
}

impl DecodedDeviceUploadResult {
    const fn kind(&self) -> DeviceUploadKind {
        match self {
            Self::Review { .. } => DeviceUploadKind::Review,
            Self::Content(_) => DeviceUploadKind::Content,
            Self::Asset(_) => DeviceUploadKind::Asset,
        }
    }

    fn operation_id(&self) -> &str {
        match self {
            Self::Review { result, .. } => &result.attempt_id,
            Self::Content(result) => &result.operation_id,
            Self::Asset(result) => &result.operation_id,
        }
    }
}

impl fmt::Debug for DeviceUploadResult {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeviceUploadResult")
            .field("kind", &self.kind())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct PendingAssetUpload {
    operation_id: String,
    device_sequence: u64,
    logical_path: String,
    content_sha256: String,
    size_bytes: u64,
    media_type: String,
    bytes: Vec<u8>,
    canonical_payload: String,
}

impl PendingAssetUpload {
    pub fn operation_id(&self) -> &str {
        &self.operation_id
    }

    pub const fn device_sequence(&self) -> u64 {
        self.device_sequence
    }

    pub fn logical_path(&self) -> &str {
        &self.logical_path
    }

    pub fn content_sha256(&self) -> &str {
        &self.content_sha256
    }

    pub const fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    pub fn media_type(&self) -> &str {
        &self.media_type
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn canonical_payload(&self) -> &str {
        &self.canonical_payload
    }

    pub fn submission(&self) -> Result<Value, Error> {
        submission_value(&self.canonical_payload)
    }
}

fn submission_value(payload: &str) -> Result<Value, Error> {
    serde_json::from_str(payload).map_err(|_| Error::Response)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfflineAssetCreate {
    pub operation_id: String,
    pub original_name: String,
    pub media_type: String,
    pub bytes: Vec<u8>,
    pub authored_at_us: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedAssetOperation {
    pub operation_id: String,
    pub device_sequence: u64,
    pub logical_path: String,
    pub content_sha256: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfflineDocumentEdit {
    pub operation_id: String,
    pub document_id: String,
    pub logical_path: String,
    pub org: String,
    pub authored_at_us: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfflineDocumentRename {
    pub operation_id: String,
    pub document_id: String,
    pub logical_path: String,
    pub authored_at_us: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OfflineDocumentTombstone {
    pub operation_id: String,
    pub document_id: String,
    pub authored_at_us: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordedContentOperation {
    pub operation_id: String,
    pub device_sequence: u64,
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
    pub next_device_sequence: u64,
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
pub struct ReplicaAsset {
    pub logical_path: String,
    pub revision: u64,
    pub content_sha256: String,
    pub size_bytes: u64,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicaStagedAsset {
    pub logical_path: String,
    pub content_sha256: String,
    pub size_bytes: u64,
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReplicaAssetState {
    Missing,
    Tombstoned { revision: u64 },
    Staged(ReplicaStagedAsset),
    Available(ReplicaAsset),
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OfflineContentOutcome {
    Accepted,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ContentConflictReason {
    CursorChanged,
    BaseChanged,
    InvalidContent,
    PathConflict,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconciledOfflineContent {
    pub result_sequence: u64,
    pub server_id: String,
    pub operation_id: String,
    pub device_sequence: u64,
    pub document_id: String,
    pub outcome: OfflineContentOutcome,
    pub conflict_reason: Option<ContentConflictReason>,
    pub operation: Value,
    pub authoritative: Option<ReconciledDocumentHead>,
    pub cursor: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciledDocumentHead {
    pub document_id: String,
    pub logical_path: String,
    pub revision: u64,
    pub content_sha256: Option<String>,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconciledOfflineContentPage {
    pub items: Vec<ReconciledOfflineContent>,
    pub next_after: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OfflineAssetOutcome {
    Accepted,
    Conflict,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssetConflictReason {
    CursorChanged,
    BaseChanged,
    InvalidContent,
    PathConflict,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconciledOfflineAsset {
    pub result_sequence: u64,
    pub server_id: String,
    pub operation_id: String,
    pub device_sequence: u64,
    pub logical_path: String,
    pub outcome: OfflineAssetOutcome,
    pub conflict_reason: Option<AssetConflictReason>,
    pub operation: Value,
    pub authoritative: Option<ReplicaAssetHead>,
    pub cursor: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicaAssetHead {
    pub logical_path: String,
    pub revision: u64,
    pub content_sha256: String,
    pub size_bytes: u64,
    pub media_type: String,
    pub active: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ReconciledOfflineAssetPage {
    pub items: Vec<ReconciledOfflineAsset>,
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
    #[error("offline content response is invalid")]
    ContentWire,
    #[error("offline asset response is invalid")]
    AssetWire,
    #[error("offline content operation no longer matches the replica state")]
    Content,
    #[error("offline content path is already owned by another document")]
    PathConflict,
    #[error("local replica response is invalid")]
    Response,
}

impl LocalReplica {
    pub fn open(path: impl AsRef<Path>, device_id: &str) -> Result<Self, Error> {
        Ok(Self {
            store: LocalReplicaStore::open(path, device_id)?,
        })
    }

    pub fn device_id(&self) -> Result<String, Error> {
        Ok(self.store.device_id()?)
    }

    pub fn observe_state(&mut self, response: ReplicaStateV1) -> Result<(), Error> {
        let state = decode_replica_state(response)?;
        self.store
            .observe_server(&state.server_id, state.next_device_sequence)?;
        Ok(())
    }

    pub fn install_content_capabilities(
        &mut self,
        response: ContentCapabilitiesV1,
    ) -> Result<(), Error> {
        let capabilities = decode_offline_content_capabilities(response)?;
        self.store
            .install_content_capabilities(&capabilities.server_id)?;
        Ok(())
    }

    pub fn install_asset_capabilities(
        &mut self,
        response: AssetCapabilitiesV1,
    ) -> Result<(), Error> {
        let capabilities = decode_offline_asset_capabilities(response)?;
        self.store
            .install_asset_capabilities(&capabilities.server_id, capabilities.max_blob_bytes)?;
        Ok(())
    }

    pub fn install_snapshot_page(
        &mut self,
        requested: Option<&str>,
        response: ReplicaSnapshotPageV1,
    ) -> Result<(), Error> {
        let value = serde_json::to_value(&response).map_err(|_| Error::Response)?;
        let response_sha256 = canonical_json_sha256(&value)?;
        let page = decode_snapshot_page(response)?;
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

    pub fn install_change_page(&mut self, response: ReplicaChangePageV1) -> Result<bool, Error> {
        let page = decode_change_page(response)?;
        let has_more = page.has_more;
        self.store.install_delta(DeltaInstall {
            server_id: page.server_id,
            from: page.from,
            next: page.next,
            items: page.items.into_iter().map(LocalReplicaItem::from).collect(),
        })?;
        Ok(has_more)
    }

    pub fn install_review_pack(&mut self, response: ReviewPackEnvelopeV2) -> Result<String, Error> {
        let cursor = response.pack.cursor.clone();
        let pack = decode_review_pack(response)?;
        let id = pack.pack_id.clone();
        self.store.install_review_pack(&pack, &cursor)?;
        Ok(id)
    }

    pub fn install_blob(&mut self, digest: &str, bytes: &[u8]) -> Result<(), Error> {
        self.store.install_blob(digest, bytes)?;
        Ok(())
    }

    pub(crate) fn missing_projection_blobs(&self) -> Result<Vec<String>, Error> {
        Ok(self.store.missing_projection_blobs()?)
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

    pub fn document_id_for_path(&self, logical_path: &str) -> Result<Option<String>, Error> {
        Ok(self.store.document_id_for_path(logical_path)?)
    }

    pub fn asset(&self, logical_path: &str) -> Result<ReplicaAssetState, Error> {
        validate_asset_path(logical_path).map_err(|_| Error::Content)?;
        let _ = self.store.current_server_id()?;
        let Some(item) = self.store.projection("asset", logical_path)? else {
            return self.local_asset_overlay(logical_path);
        };
        if item.tombstone {
            return Ok(ReplicaAssetState::Tombstoned {
                revision: item.revision,
            });
        }
        let (path, revision, digest, size, media_type) = decode_replica_asset(&item, logical_path)?;
        let bytes = self.store.blob(&digest)?.ok_or(Error::MissingAssets)?;
        if bytes.len() != usize::try_from(size).map_err(|_| Error::Response)? {
            return Err(Error::Response);
        }
        Ok(ReplicaAssetState::Available(ReplicaAsset {
            logical_path: path,
            revision,
            content_sha256: digest,
            size_bytes: size,
            media_type,
            bytes,
        }))
    }

    fn local_asset_overlay(&self, logical_path: &str) -> Result<ReplicaAssetState, Error> {
        let Some(asset) = self.store.asset_overlay(logical_path)? else {
            return Ok(ReplicaAssetState::Missing);
        };
        let actual = format!("{:x}", Sha256::digest(&asset.bytes));
        let path_digest = logical_path.split('/').nth(1);
        let size = usize::try_from(asset.size_bytes).map_err(|_| Error::Response)?;
        if actual != asset.content_sha256
            || path_digest != Some(asset.content_sha256.as_str())
            || asset.bytes.len() != size
        {
            return Err(Error::Response);
        }
        Ok(ReplicaAssetState::Staged(ReplicaStagedAsset {
            logical_path: logical_path.to_owned(),
            content_sha256: asset.content_sha256,
            size_bytes: asset.size_bytes,
            media_type: asset.media_type,
            bytes: asset.bytes,
        }))
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
        Ok(Some(PendingUpload {
            attempt_id: operation.attempt_id.clone(),
            device_sequence: operation.device_sequence,
            canonical_payload,
        }))
    }

    pub fn record_document(
        &mut self,
        edit: OfflineDocumentEdit,
    ) -> Result<RecordedContentOperation, Error> {
        if let Some(json) = self.store.existing_content_operation(&edit.operation_id)? {
            return replayed_document_edit(&json, &edit);
        }
        let asset_paths = asset_paths_in_org(&edit.org).unwrap_or_default();
        self.store.require_document_assets(&asset_paths)?;
        let operation_id = edit.operation_id;
        let document_id = edit.document_id;
        let authored_at_us = edit.authored_at_us;
        self.record_content_mutation(operation_id, document_id.clone(), authored_at_us, |base| {
            OfflineContentMutation::Put {
                document_id,
                logical_path: edit.logical_path,
                content_sha256: format!("{:x}", Sha256::digest(edit.org.as_bytes())),
                org: edit.org,
                base,
            }
        })
    }

    pub fn rename_document(
        &mut self,
        rename: OfflineDocumentRename,
    ) -> Result<RecordedContentOperation, Error> {
        if let Some(json) = self
            .store
            .existing_content_operation(&rename.operation_id)?
        {
            return replayed_document_rename(&json, &rename);
        }
        let operation_id = rename.operation_id;
        let document_id = rename.document_id;
        let authored_at_us = rename.authored_at_us;
        self.record_content_mutation(operation_id, document_id.clone(), authored_at_us, |base| {
            OfflineContentMutation::Rename {
                document_id,
                logical_path: rename.logical_path,
                base,
            }
        })
    }

    pub fn tombstone_document(
        &mut self,
        tombstone: OfflineDocumentTombstone,
    ) -> Result<RecordedContentOperation, Error> {
        if let Some(json) = self
            .store
            .existing_content_operation(&tombstone.operation_id)?
        {
            return replayed_document_tombstone(&json, &tombstone);
        }
        let operation_id = tombstone.operation_id;
        let document_id = tombstone.document_id;
        let authored_at_us = tombstone.authored_at_us;
        self.record_content_mutation(operation_id, document_id.clone(), authored_at_us, |base| {
            OfflineContentMutation::Tombstone { document_id, base }
        })
    }

    pub fn record_asset(
        &mut self,
        create: OfflineAssetCreate,
    ) -> Result<RecordedAssetOperation, Error> {
        let digest = format!("{:x}", Sha256::digest(&create.bytes));
        let logical_path =
            canonical_asset_path(&digest, &create.original_name).map_err(|_| Error::Content)?;
        if let Some(json) = self.store.existing_asset_operation(&create.operation_id)? {
            return replayed_asset_create(&json, &create, &logical_path, &digest);
        }
        let context = self.store.asset_context(&logical_path)?;
        let size_bytes = u64::try_from(create.bytes.len()).map_err(|_| Error::Content)?;
        if size_bytes == 0 || size_bytes > context.max_blob_bytes {
            return Err(Error::Content);
        }
        let (server_id, cursor, token) = decode_cursor(&context.cursor)?;
        if server_id != context.server_id {
            return Err(Error::Content);
        }
        let operation = OfflineAssetOperation {
            operation_id: create.operation_id,
            device_id: self.device_id()?,
            device_sequence: context.next_device_sequence,
            cursor_server_id: server_id,
            cursor,
            cursor_token: token,
            authored_at_us: create.authored_at_us,
            logical_path: logical_path.clone(),
            original_name: create.original_name,
            content_sha256: digest.clone(),
            size_bytes,
            media_type: create.media_type,
        };
        let canonical_payload = canonical_asset_operation(&operation)?;
        self.store.record_asset_operation(
            &operation,
            &context.cursor,
            &canonical_payload,
            &create.bytes,
        )?;
        Ok(RecordedAssetOperation {
            operation_id: operation.operation_id,
            device_sequence: operation.device_sequence,
            logical_path,
            content_sha256: digest,
            size_bytes,
        })
    }

    fn record_content_mutation(
        &mut self,
        operation_id: String,
        document_id: String,
        authored_at_us: u64,
        mutation: impl FnOnce(ContentBase) -> OfflineContentMutation,
    ) -> Result<RecordedContentOperation, Error> {
        let context = self.store.content_context(&document_id)?;
        let (server_id, cursor, token) = decode_cursor(&context.cursor)?;
        if server_id != context.server_id {
            return Err(Error::Content);
        }
        let operation = OfflineContentOperation {
            operation_id,
            device_id: self.device_id()?,
            device_sequence: context.next_device_sequence,
            cursor_server_id: server_id,
            cursor,
            cursor_token: token,
            authored_at_us,
            mutation: mutation(context.base),
        };
        let canonical_payload = canonical_content_operation(&operation)?;
        self.store
            .record_content_operation(&operation, &context.cursor, &canonical_payload)?;
        Ok(RecordedContentOperation {
            operation_id: operation.operation_id,
            device_sequence: operation.device_sequence,
        })
    }

    pub fn next_content_upload(&self) -> Result<Option<PendingContentUpload>, Error> {
        let Some(pending) = self.store.next_pending_content_operation()? else {
            return Ok(None);
        };
        let submission: OfflineContentSubmission =
            serde_json::from_str(&pending.canonical_payload).map_err(|_| Error::Response)?;
        let decoded = submission.decode()?;
        if decoded.canonical_payload != pending.canonical_payload {
            return Err(Error::Response);
        }
        let operation = decoded.operation;
        let document_id = operation.document_id().to_owned();
        Ok(Some(PendingContentUpload {
            operation_id: operation.operation_id.clone(),
            device_sequence: operation.device_sequence,
            document_id,
            canonical_payload: pending.canonical_payload,
        }))
    }

    pub fn next_asset_upload(&self) -> Result<Option<PendingAssetUpload>, Error> {
        let Some(pending) = self.store.next_pending_asset_operation()? else {
            return Ok(None);
        };
        let submission: OfflineAssetSubmission =
            serde_json::from_str(&pending.canonical_payload).map_err(|_| Error::Response)?;
        let decoded = submission.decode()?;
        let operation = decoded.operation;
        let size_bytes = u64::try_from(pending.bytes.len()).map_err(|_| Error::Response)?;
        if decoded.canonical_payload != pending.canonical_payload
            || format!("{:x}", Sha256::digest(&pending.bytes)) != operation.content_sha256
            || size_bytes != operation.size_bytes
        {
            return Err(Error::Response);
        }
        Ok(Some(PendingAssetUpload {
            operation_id: operation.operation_id.clone(),
            device_sequence: operation.device_sequence,
            logical_path: operation.logical_path.clone(),
            content_sha256: operation.content_sha256.clone(),
            size_bytes,
            media_type: operation.media_type.clone(),
            bytes: pending.bytes,
            canonical_payload: pending.canonical_payload,
        }))
    }

    pub fn next_device_upload(&self) -> Result<Option<PendingDeviceUpload>, Error> {
        if let Some(review) = self.next_upload()? {
            return Ok(Some(PendingDeviceUpload::Review(Box::new(review))));
        }
        if let Some(content) = self.next_content_upload()? {
            return Ok(Some(PendingDeviceUpload::Content(content)));
        }
        Ok(self.next_asset_upload()?.map(PendingDeviceUpload::Asset))
    }

    pub fn record_device_result(&mut self, result: DeviceUploadResult) -> Result<(), Error> {
        let (result_json, result) = result.into_parts();
        match result {
            DecodedDeviceUploadResult::Review { cursor, result } => {
                self.store.record_result(&result, &cursor, &result_json)?;
                Ok(())
            }
            DecodedDeviceUploadResult::Content(result) => {
                self.record_decoded_content_result(result, result_json)
            }
            DecodedDeviceUploadResult::Asset(result) => {
                self.record_decoded_asset_result(result, result_json)
            }
        }
    }

    pub fn record_content_result(&mut self, value: Value) -> Result<(), Error> {
        let result_json = serde_json::to_string(&value).map_err(|_| Error::Response)?;
        let decoded = decode_offline_content_result(value)?;
        self.record_decoded_content_result(decoded, result_json)
    }

    fn record_decoded_content_result(
        &mut self,
        decoded: DecodedOfflineContentResult,
        result_json: String,
    ) -> Result<(), Error> {
        let operation_json = self
            .store
            .existing_content_operation(&decoded.operation_id)?
            .ok_or(Error::Content)?;
        let submission: OfflineContentSubmission =
            serde_json::from_str(&operation_json).map_err(|_| Error::Response)?;
        let operation = submission.decode()?.operation;
        let install = validated_content_result(&operation, &decoded)?;
        self.store
            .record_content_result(&install, &decoded.cursor, &result_json)?;
        Ok(())
    }

    pub fn record_asset_result(&mut self, value: Value) -> Result<(), Error> {
        let result_json = serde_json::to_string(&value).map_err(|_| Error::Response)?;
        let decoded = decode_offline_asset_result(value)?;
        self.record_decoded_asset_result(decoded, result_json)
    }

    fn record_decoded_asset_result(
        &mut self,
        decoded: DecodedOfflineAssetResult,
        result_json: String,
    ) -> Result<(), Error> {
        let operation_json = self
            .store
            .existing_asset_operation(&decoded.operation_id)?
            .ok_or(Error::Content)?;
        let submission: OfflineAssetSubmission =
            serde_json::from_str(&operation_json).map_err(|_| Error::Response)?;
        let operation = submission.decode()?.operation;
        let install = validated_asset_result(&operation, &decoded)?;
        self.store
            .record_asset_result(&install, &decoded.cursor, &result_json)?;
        Ok(())
    }

    pub fn asset_results(
        &self,
        after: Option<u64>,
        limit: u16,
    ) -> Result<ReconciledOfflineAssetPage, Error> {
        self.asset_result_page(after, limit, false)
    }

    pub fn asset_conflicts(
        &self,
        after: Option<u64>,
        limit: u16,
    ) -> Result<ReconciledOfflineAssetPage, Error> {
        self.asset_result_page(after, limit, true)
    }

    fn asset_result_page(
        &self,
        after: Option<u64>,
        limit: u16,
        conflicts_only: bool,
    ) -> Result<ReconciledOfflineAssetPage, Error> {
        let page = self
            .store
            .asset_results(after.unwrap_or(0), limit, conflicts_only)?;
        ReconciledOfflineAssetPage::try_from(page)
    }

    pub fn content_results(
        &self,
        after: Option<u64>,
        limit: u16,
    ) -> Result<ReconciledOfflineContentPage, Error> {
        self.content_result_page(after, limit, false)
    }

    pub fn content_conflicts(
        &self,
        after: Option<u64>,
        limit: u16,
    ) -> Result<ReconciledOfflineContentPage, Error> {
        self.content_result_page(after, limit, true)
    }

    fn content_result_page(
        &self,
        after: Option<u64>,
        limit: u16,
        conflicts_only: bool,
    ) -> Result<ReconciledOfflineContentPage, Error> {
        let page = self
            .store
            .content_results(after.unwrap_or(0), limit, conflicts_only)?;
        ReconciledOfflineContentPage::try_from(page)
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
            next_device_sequence: status.next_device_sequence,
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

fn validated_content_result(
    operation: &OfflineContentOperation,
    result: &crate::protocol::DecodedOfflineContentResult,
) -> Result<ContentResultInstall, Error> {
    let (server_id, cursor, _) = decode_cursor(&result.cursor)?;
    if server_id != operation.cursor_server_id || cursor < operation.cursor {
        return Err(Error::Result);
    }
    validate_content_authority(operation, result)?;
    Ok(ContentResultInstall {
        operation_id: result.operation_id.clone(),
        status: result.status,
        conflict_reason: result.conflict_reason,
        cursor_server_id: server_id,
        authoritative_document_id: result
            .authoritative
            .as_ref()
            .map(|document| document.document_id.clone()),
    })
}

fn replayed_asset_create(
    json: &str,
    create: &OfflineAssetCreate,
    logical_path: &str,
    digest: &str,
) -> Result<RecordedAssetOperation, Error> {
    let submission: OfflineAssetSubmission =
        serde_json::from_str(json).map_err(|_| Error::Response)?;
    let operation = submission.decode()?.operation;
    let size_bytes = u64::try_from(create.bytes.len()).map_err(|_| Error::Content)?;
    let matches = operation.operation_id == create.operation_id
        && operation.original_name == create.original_name
        && operation.media_type == create.media_type
        && operation.authored_at_us == create.authored_at_us
        && operation.logical_path == logical_path
        && operation.content_sha256 == digest
        && operation.size_bytes == size_bytes;
    matches
        .then_some(RecordedAssetOperation {
            operation_id: operation.operation_id,
            device_sequence: operation.device_sequence,
            logical_path: operation.logical_path,
            content_sha256: operation.content_sha256,
            size_bytes: operation.size_bytes,
        })
        .ok_or(Error::Content)
}

fn validated_asset_result(
    operation: &OfflineAssetOperation,
    result: &crate::protocol::DecodedOfflineAssetResult,
) -> Result<AssetResultInstall, Error> {
    let (server_id, cursor, _) = decode_cursor(&result.cursor)?;
    if server_id != operation.cursor_server_id || cursor < operation.cursor {
        return Err(Error::Result);
    }
    validate_asset_authority(operation, result)?;
    Ok(AssetResultInstall {
        operation_id: result.operation_id.clone(),
        status: result.status,
        conflict_reason: result.conflict_reason,
        cursor_server_id: server_id,
        authoritative_path: result
            .authoritative
            .as_ref()
            .map(|asset| asset.logical_path.clone()),
    })
}

fn validate_asset_authority(
    operation: &OfflineAssetOperation,
    result: &crate::protocol::DecodedOfflineAssetResult,
) -> Result<(), Error> {
    let Some(authoritative) = &result.authoritative else {
        return if result.status == AssetReconciliationStatus::Conflict {
            Ok(())
        } else {
            Err(Error::Result)
        };
    };
    if authoritative.logical_path != operation.logical_path {
        return Err(Error::Result);
    }
    if result.status == AssetReconciliationStatus::Accepted {
        validate_accepted_asset(operation, authoritative)?;
    }
    Ok(())
}

fn validate_accepted_asset(
    operation: &OfflineAssetOperation,
    authoritative: &AuthoritativeAsset,
) -> Result<(), Error> {
    (authoritative.active
        && authoritative.revision == 1
        && authoritative.content_sha256 == operation.content_sha256
        && authoritative.size_bytes == operation.size_bytes
        && authoritative.media_type == operation.media_type)
        .then_some(())
        .ok_or(Error::Result)
}

fn validate_content_authority(
    operation: &OfflineContentOperation,
    result: &crate::protocol::DecodedOfflineContentResult,
) -> Result<(), Error> {
    let Some(authoritative) = &result.authoritative else {
        return if result.status == ContentReconciliationStatus::Conflict {
            Ok(())
        } else {
            Err(Error::Result)
        };
    };
    if authoritative.document_id != operation.document_id() {
        return Err(Error::Result);
    }
    if result.status == ContentReconciliationStatus::Accepted {
        validate_accepted_document(operation, authoritative)?;
    }
    Ok(())
}

fn validate_accepted_document(
    operation: &OfflineContentOperation,
    authoritative: &AuthoritativeDocument,
) -> Result<(), Error> {
    let expected_revision = next_content_revision(operation.base())?;
    let valid = match &operation.mutation {
        OfflineContentMutation::Put {
            logical_path,
            content_sha256,
            ..
        } => {
            authoritative.active
                && authoritative.logical_path == *logical_path
                && authoritative.content_sha256.as_ref() == Some(content_sha256)
        }
        OfflineContentMutation::Rename {
            logical_path, base, ..
        } => {
            authoritative.active
                && authoritative.logical_path == *logical_path
                && authoritative.content_sha256.as_deref() == base.content_sha256()
        }
        OfflineContentMutation::Tombstone { .. } => {
            !authoritative.active && authoritative.content_sha256.is_none()
        }
    };
    (valid && authoritative.revision == expected_revision)
        .then_some(())
        .ok_or(Error::Result)
}

fn next_content_revision(base: &ContentBase) -> Result<u64, Error> {
    match base {
        ContentBase::Absent => Ok(1),
        ContentBase::Revision { revision, .. } => revision.checked_add(1).ok_or(Error::Result),
    }
}

fn replayed_document_edit(
    json: &str,
    edit: &OfflineDocumentEdit,
) -> Result<RecordedContentOperation, Error> {
    let submission: OfflineContentSubmission =
        serde_json::from_str(json).map_err(|_| Error::Response)?;
    let operation = submission.decode()?.operation;
    let OfflineContentMutation::Put {
        document_id,
        logical_path,
        org,
        ..
    } = &operation.mutation
    else {
        return Err(Error::Content);
    };
    if operation.operation_id != edit.operation_id
        || document_id != &edit.document_id
        || logical_path != &edit.logical_path
        || org != &edit.org
        || operation.authored_at_us != edit.authored_at_us
    {
        return Err(Error::Content);
    }
    Ok(RecordedContentOperation {
        operation_id: operation.operation_id,
        device_sequence: operation.device_sequence,
    })
}

fn replayed_document_rename(
    json: &str,
    rename: &OfflineDocumentRename,
) -> Result<RecordedContentOperation, Error> {
    let operation = decode_stored_content_operation(json)?;
    let matches = match &operation.mutation {
        OfflineContentMutation::Rename {
            document_id,
            logical_path,
            ..
        } => document_id == &rename.document_id && logical_path == &rename.logical_path,
        _ => false,
    };
    replayed_content_identity(
        operation,
        &rename.operation_id,
        rename.authored_at_us,
        matches,
    )
}

fn replayed_document_tombstone(
    json: &str,
    tombstone: &OfflineDocumentTombstone,
) -> Result<RecordedContentOperation, Error> {
    let operation = decode_stored_content_operation(json)?;
    let matches = match &operation.mutation {
        OfflineContentMutation::Tombstone { document_id, .. } => {
            document_id == &tombstone.document_id
        }
        _ => false,
    };
    replayed_content_identity(
        operation,
        &tombstone.operation_id,
        tombstone.authored_at_us,
        matches,
    )
}

fn decode_stored_content_operation(json: &str) -> Result<OfflineContentOperation, Error> {
    let submission: OfflineContentSubmission =
        serde_json::from_str(json).map_err(|_| Error::Response)?;
    Ok(submission.decode()?.operation)
}

fn replayed_content_identity(
    operation: OfflineContentOperation,
    operation_id: &str,
    authored_at_us: u64,
    mutation_matches: bool,
) -> Result<RecordedContentOperation, Error> {
    if operation.operation_id != operation_id
        || operation.authored_at_us != authored_at_us
        || !mutation_matches
    {
        return Err(Error::Content);
    }
    Ok(RecordedContentOperation {
        operation_id: operation.operation_id,
        device_sequence: operation.device_sequence,
    })
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

impl TryFrom<StoredContentResult> for ReconciledOfflineContent {
    type Error = Error;

    fn try_from(value: StoredContentResult) -> Result<Self, Self::Error> {
        let operation_value: Value =
            serde_json::from_str(&value.operation_json).map_err(|_| Error::Response)?;
        let submission: OfflineContentSubmission =
            serde_json::from_value(operation_value.clone()).map_err(|_| Error::Response)?;
        let operation = submission.decode()?.operation;
        let result_value: Value =
            serde_json::from_str(&value.result_json).map_err(|_| Error::Response)?;
        let result = decode_offline_content_result(result_value)?;
        let authoritative = result.authoritative.map(ReconciledDocumentHead::from);
        let document_id = operation.document_id().to_owned();
        Ok(Self {
            result_sequence: value.sequence,
            server_id: value.server_id,
            operation_id: operation.operation_id,
            device_sequence: operation.device_sequence,
            document_id,
            outcome: result.status.into(),
            conflict_reason: result.conflict_reason.map(ContentConflictReason::from),
            operation: operation_value,
            authoritative,
            cursor: result.cursor,
        })
    }
}

impl From<AuthoritativeDocument> for ReconciledDocumentHead {
    fn from(value: AuthoritativeDocument) -> Self {
        Self {
            document_id: value.document_id,
            logical_path: value.logical_path,
            revision: value.revision,
            content_sha256: value.content_sha256,
            active: value.active,
        }
    }
}

impl TryFrom<StoredContentResultPage> for ReconciledOfflineContentPage {
    type Error = Error;

    fn try_from(value: StoredContentResultPage) -> Result<Self, Self::Error> {
        Ok(Self {
            items: value
                .items
                .into_iter()
                .map(ReconciledOfflineContent::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            next_after: value.next_after,
        })
    }
}

impl TryFrom<StoredAssetResult> for ReconciledOfflineAsset {
    type Error = Error;

    fn try_from(value: StoredAssetResult) -> Result<Self, Self::Error> {
        let operation_value: Value =
            serde_json::from_str(&value.operation_json).map_err(|_| Error::Response)?;
        let submission: OfflineAssetSubmission =
            serde_json::from_value(operation_value.clone()).map_err(|_| Error::Response)?;
        let operation = submission.decode()?.operation;
        let result_value: Value =
            serde_json::from_str(&value.result_json).map_err(|_| Error::Response)?;
        let result = decode_offline_asset_result(result_value)?;
        Ok(Self {
            result_sequence: value.sequence,
            server_id: value.server_id,
            operation_id: operation.operation_id,
            device_sequence: operation.device_sequence,
            logical_path: operation.logical_path,
            outcome: result.status.into(),
            conflict_reason: result.conflict_reason.map(AssetConflictReason::from),
            operation: operation_value,
            authoritative: result.authoritative.map(ReplicaAssetHead::from),
            cursor: result.cursor,
        })
    }
}

impl TryFrom<StoredAssetResultPage> for ReconciledOfflineAssetPage {
    type Error = Error;

    fn try_from(value: StoredAssetResultPage) -> Result<Self, Self::Error> {
        Ok(Self {
            items: value
                .items
                .into_iter()
                .map(ReconciledOfflineAsset::try_from)
                .collect::<Result<Vec<_>, _>>()?,
            next_after: value.next_after,
        })
    }
}

impl From<AuthoritativeAsset> for ReplicaAssetHead {
    fn from(value: AuthoritativeAsset) -> Self {
        Self {
            logical_path: value.logical_path,
            revision: value.revision,
            content_sha256: value.content_sha256,
            size_bytes: value.size_bytes,
            media_type: value.media_type,
            active: value.active,
        }
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

impl From<ContentReconciliationStatus> for OfflineContentOutcome {
    fn from(value: ContentReconciliationStatus) -> Self {
        match value {
            ContentReconciliationStatus::Accepted => Self::Accepted,
            ContentReconciliationStatus::Conflict => Self::Conflict,
        }
    }
}

impl From<AssetReconciliationStatus> for OfflineAssetOutcome {
    fn from(value: AssetReconciliationStatus) -> Self {
        match value {
            AssetReconciliationStatus::Accepted => Self::Accepted,
            AssetReconciliationStatus::Conflict => Self::Conflict,
        }
    }
}

impl From<OfflineAssetConflictReason> for AssetConflictReason {
    fn from(value: OfflineAssetConflictReason) -> Self {
        match value {
            OfflineAssetConflictReason::CursorChanged => Self::CursorChanged,
            OfflineAssetConflictReason::BaseChanged => Self::BaseChanged,
            OfflineAssetConflictReason::InvalidContent => Self::InvalidContent,
            OfflineAssetConflictReason::PathConflict => Self::PathConflict,
        }
    }
}

impl From<OfflineContentConflictReason> for ContentConflictReason {
    fn from(value: OfflineContentConflictReason) -> Self {
        match value {
            OfflineContentConflictReason::CursorChanged => Self::CursorChanged,
            OfflineContentConflictReason::BaseChanged => Self::BaseChanged,
            OfflineContentConflictReason::InvalidContent => Self::InvalidContent,
            OfflineContentConflictReason::PathConflict => Self::PathConflict,
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

fn decode_replica_asset(
    item: &LocalReplicaItem,
    logical_path: &str,
) -> Result<(String, u64, String, u64, String), Error> {
    let payload: Value = serde_json::from_str(&item.payload_json).map_err(|_| Error::Response)?;
    let projection = payload.get("projection").ok_or(Error::Response)?;
    let path = projection
        .get("logical_path")
        .and_then(Value::as_str)
        .ok_or(Error::Response)?;
    let digest = projection
        .get("content_sha256")
        .and_then(Value::as_str)
        .ok_or(Error::Response)?;
    let size = projection
        .get("size_bytes")
        .and_then(Value::as_u64)
        .ok_or(Error::Response)?;
    let media_type = projection
        .get("media_type")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(Error::Response)?;
    if path != logical_path || logical_path.split('/').nth(1) != Some(digest) {
        return Err(Error::Response);
    }
    if item.blob_digests.as_slice() != [digest] {
        return Err(Error::Response);
    }
    Ok((
        path.to_owned(),
        item.revision,
        digest.to_owned(),
        size,
        media_type.to_owned(),
    ))
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
            StoreReplicaError::Content => Self::Content,
            StoreReplicaError::PathConflict => Self::PathConflict,
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

impl From<OfflineContentWireError> for Error {
    fn from(_: OfflineContentWireError) -> Self {
        Self::ContentWire
    }
}

impl From<OfflineAssetWireError> for Error {
    fn from(_: OfflineAssetWireError) -> Self {
        Self::AssetWire
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

    use super::{
        AssetConflictReason, ContentConflictReason, Error, LocalReplica, OfflineAnswer,
        OfflineAssetCreate, OfflineContentOutcome, OfflineDocumentEdit, OfflineDocumentRename,
        OfflineDocumentTombstone, OfflineResponse, PendingAssetUpload, PendingContentUpload,
        PendingDeviceUpload, ReplicaAssetState, ReviewConflictReason,
    };
    use crate::{
        application::{
            NewDevice, ReviewSource, ReviewSourceKind, canonical_asset_path, generate_device,
            prepare_asset, prepare_note, prepare_thema, thema_content,
        },
        protocol::{
            OFFLINE_REVIEW_VERSION, OfflineAssetSubmission, OfflineContentSubmission,
            OfflineReviewSubmission, decode_cursor, decode_snapshot_cursor, encode_cursor,
            encode_snapshot_cursor, offline_asset_capabilities, offline_asset_result_json,
            offline_content_capabilities, offline_content_result_json, offline_result_json,
            replica_change_page, replica_snapshot_page, replica_state_response, review_pack_json,
        },
        service::{ServiceHandle, ServiceRuntime},
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
        replica
            .observe_state(decode_response(fixture["state"].clone()))
            .unwrap();
        replica
            .install_snapshot_page(None, decode_response(fixture["snapshot_partial"].clone()))
            .unwrap();
        assert_eq!(replica.counts().unwrap().0, 0);
        let next = replica.status().unwrap().snapshot_next.unwrap();
        drop(replica);

        let mut replica = LocalReplica::open(&path, "device").unwrap();
        assert_eq!(replica.status().unwrap().snapshot_next, Some(next.clone()));
        replica
            .install_snapshot_page(None, decode_response(fixture["snapshot_partial"].clone()))
            .unwrap();
        replica
            .install_snapshot_page(
                Some(&next),
                decode_response(fixture["snapshot_terminal"].clone()),
            )
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
            .install_change_page(decode_response(fixture["changes"].clone()))
            .unwrap();
        let cursor = replica.status().unwrap().cursor.unwrap();
        let before = replica.projection("thema", "thema").unwrap().unwrap();
        let malformed = malformed_delta(&fixture, &cursor);
        assert!(matches!(
            replica.install_change_page(decode_response(malformed)),
            Err(Error::ReplicaWire)
        ));
        assert_eq!(replica.status().unwrap().cursor, Some(cursor.clone()));
        assert_eq!(
            replica.projection("thema", "thema").unwrap().unwrap(),
            before
        );
        assert!(matches!(
            replica.install_change_page(decode_response(noncanonical_asset_delta(&cursor))),
            Err(Error::ReplicaWire)
        ));
        assert_eq!(replica.status().unwrap().cursor, Some(cursor.clone()));
        assert!(matches!(
            replica.install_change_page(decode_response(invalid_review_delta(&fixture, &cursor))),
            Err(Error::ReplicaWire)
        ));
        assert_eq!(replica.status().unwrap().cursor, Some(cursor.clone()));
        assert!(matches!(
            replica.install_change_page(decode_response(older_asset_delta(&cursor))),
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
        replica
            .observe_state(decode_response(fixture["state"].clone()))
            .unwrap();
        replica
            .install_snapshot_page(None, decode_response(fixture["snapshot_partial"].clone()))
            .unwrap();

        let mut replacement = fixture["snapshot_partial"].clone();
        replacement["snapshot_id"] = json!("snapshot-2");
        let mut position = decode_snapshot_cursor(replacement["next"].as_str().unwrap()).unwrap();
        position.snapshot_id = "snapshot-2".into();
        let replacement_next = encode_snapshot_cursor(&position).unwrap();
        replacement["next"] = json!(replacement_next.clone());
        replica
            .install_snapshot_page(None, decode_response(replacement))
            .unwrap();
        assert_eq!(replica.counts().unwrap().0, 0);

        let mut terminal = fixture["snapshot_terminal"].clone();
        terminal["snapshot_id"] = json!("snapshot-2");
        replica
            .install_snapshot_page(Some(&replacement_next), decode_response(terminal))
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
                .next_device_sequence,
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
            .observe_state(replica_state_response(&state).unwrap())
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
            .install_review_pack(typed_review_pack(review_pack_json(&pack).unwrap()))
            .unwrap();
        assert!(replica.missing_projection_blobs().unwrap().is_empty());
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
        let server_cursor = replica_state_response(&server_state).unwrap().cursor;
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
    async fn review_pack_assets_are_not_projection_sync_requirements() {
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
        let state =
            replica_state_response(&service.replica_state(device_id.clone()).await.unwrap())
                .unwrap();
        let mut replica =
            LocalReplica::open(directory.path().join("replica.sqlite"), &device_id).unwrap();
        replica.observe_state(state.clone()).unwrap();

        let digest = format!("{:x}", Sha256::digest(b"pack-only"));
        let mut pack: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/protocol/offline-review-v1.json"
        ))
        .unwrap();
        pack["pack"]["pack"]["device_id"] = json!(device_id);
        pack["pack"]["pack"]["cursor"] = json!(state.cursor);
        pack["pack"]["pack"]["assets"] = json!([{
            "logical_path": format!("assets/{digest}/image.png"),
            "content_sha256": digest.clone(),
            "size_bytes": 9,
            "media_type": "image/png"
        }]);
        replica
            .install_review_pack(typed_review_pack(pack["pack"].clone()))
            .unwrap();

        assert!(replica.blob(&digest).unwrap().is_none());
        assert!(replica.missing_projection_blobs().unwrap().is_empty());
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn replica_asset_resolution_is_verified_and_tracks_tombstones() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("server.sqlite"),
            directory.path().join("server-blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device =
            generate_device("replica".into(), vec!["read".into(), "review".into()]).unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let path = add_shared_asset(&service, &device_id).await;
        add_thema(&service, "first", "First", &path, &device_id, 3).await;
        let replica_path = directory.path().join("replica.sqlite");
        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        let state = service.replica_state(device_id.clone()).await.unwrap();
        replica
            .observe_state(replica_state_response(&state).unwrap())
            .unwrap();
        install_server_snapshot(&mut replica, &service, &device_id).await;

        assert!(matches!(replica.asset(&path), Err(Error::MissingAssets)));
        install_missing_blobs(&mut replica, &service).await;
        let ReplicaAssetState::Available(asset) = replica.asset(&path).unwrap() else {
            panic!("active replica asset must resolve");
        };
        assert_eq!(asset.logical_path, path);
        assert_eq!(asset.media_type, "image/png");
        assert_eq!(asset.bytes, b"shared offline asset");
        let digest = asset.content_sha256;
        assert_eq!(
            replica
                .asset(&format!("assets/{}/missing.png", "b".repeat(64)))
                .unwrap(),
            ReplicaAssetState::Missing
        );

        let pack = service
            .create_review_pack(
                OFFLINE_REVIEW_VERSION,
                manual_source(&["first"]),
                Some(1),
                device_id.clone(),
                1_752_600_000_000_000,
            )
            .await
            .unwrap();
        let pack_id = replica
            .install_review_pack(typed_review_pack(review_pack_json(&pack).unwrap()))
            .unwrap();
        let thema = service.thema("first".into()).await.unwrap().unwrap();
        service
            .archive_thema(
                "first".into(),
                thema.revision,
                true,
                4,
                Some(device_id.clone()),
            )
            .await
            .unwrap();

        let (head, _) = service.asset(path.clone()).await.unwrap().unwrap();
        let tombstone = service
            .tombstone_asset(path.clone(), head.revision, 5, Some(device_id.clone()))
            .await
            .unwrap();
        pull_server_changes(&mut replica, &service).await;
        assert_eq!(
            replica.asset(&path).unwrap(),
            ReplicaAssetState::Tombstoned {
                revision: tombstone.revision
            }
        );
        drop(replica);

        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        let stored = replica.review_pack(&pack_id).unwrap().unwrap();
        assert_eq!(stored.assets[0].content_sha256, digest);
        assert_eq!(
            replica.blob(&digest).unwrap().unwrap(),
            b"shared offline asset"
        );
        assert!(matches!(
            replica.asset(&path).unwrap(),
            ReplicaAssetState::Tombstoned { .. }
        ));

        let replacement_server = "f".repeat(32);
        let mut replacement = replica_state_response(&state).unwrap();
        replacement.server_id = replacement_server.clone();
        replacement.cursor = encode_cursor(&replacement_server, 0, &"e".repeat(32)).unwrap();
        replica.observe_state(replacement).unwrap();
        assert!(matches!(replica.asset(&path), Err(Error::SyncOrder)));
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn offline_document_survives_restarts_exact_retry_and_feed_convergence() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("content-server.sqlite");
        let blobs = directory.path().join("content-blobs");
        let replica_path = directory.path().join("content-replica.sqlite");
        let runtime = ServiceRuntime::start(database.clone(), blobs.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let device = generate_device(
            "offline-content".into(),
            vec!["read".into(), "content:write".into()],
        )
        .unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();

        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        sync_server(&mut replica, &service, &device_id).await;
        let edit = OfflineDocumentEdit {
            operation_id: "offline-document-1".into(),
            document_id: "document".into(),
            logical_path: "note.org".into(),
            org: "#+title: Offline\n* Heading\n".into(),
            authored_at_us: 10,
        };
        let recorded = replica.record_document(edit.clone()).unwrap();
        assert_eq!(recorded.device_sequence, 1);
        assert_eq!(replica.record_document(edit.clone()).unwrap(), recorded);
        let mut changed_retry = edit;
        changed_retry.org.push_str("Changed\n");
        assert!(matches!(
            replica.record_document(changed_retry),
            Err(Error::Content)
        ));
        for (sequence, title) in [(2, "Second"), (3, "Third")] {
            let recorded = replica
                .record_document(OfflineDocumentEdit {
                    operation_id: format!("offline-document-{sequence}"),
                    document_id: "document".into(),
                    logical_path: "note.org".into(),
                    org: format!("#+title: {title}\n* Heading\n"),
                    authored_at_us: 10 + sequence,
                })
                .unwrap();
            assert_eq!(recorded.device_sequence, sequence);
        }
        assert_eq!(replica.status().unwrap().next_device_sequence, 4);
        drop(replica);

        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        let upload = replica.next_content_upload().unwrap().unwrap();
        let lost = reconcile_content_upload(&service, &upload, &device_id, 20).await;
        assert_eq!(lost["result"]["status"], "accepted");
        for mut malformed in malformed_content_results(&lost) {
            assert!(replica.record_content_result(malformed.take()).is_err());
            assert_eq!(
                replica.next_content_upload().unwrap().unwrap().operation_id,
                "offline-document-1"
            );
        }
        drop(service);
        runtime.finish().await.unwrap();

        let runtime = ServiceRuntime::start(database, blobs, 8).await.unwrap();
        let service = runtime.handle();
        drop(replica);
        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        let retry = replica.next_content_upload().unwrap().unwrap();
        assert_eq!(retry.canonical_payload, upload.canonical_payload);
        let replay = reconcile_content_upload(&service, &retry, &device_id, 30).await;
        assert_eq!(replay, lost);
        replica.record_content_result(replay).unwrap();
        for expected_sequence in [2, 3] {
            let pending = replica.next_content_upload().unwrap().unwrap();
            assert_eq!(pending.device_sequence, expected_sequence);
            let result =
                reconcile_content_upload(&service, &pending, &device_id, 30 + expected_sequence)
                    .await;
            assert_eq!(result["result"]["status"], "accepted");
            replica.record_content_result(result).unwrap();
        }
        assert!(replica.next_device_upload().unwrap().is_none());
        let results = replica.content_results(None, 10).unwrap();
        assert_eq!(results.items.len(), 3);
        assert!(
            results
                .items
                .iter()
                .all(|item| item.outcome == OfflineContentOutcome::Accepted)
        );

        pull_server_changes(&mut replica, &service).await;
        let document = replica.projection("document", "document").unwrap().unwrap();
        assert_eq!(document.revision, 3);
        assert_eq!(document.payload["projection"]["logical_path"], "note.org");
        assert_eq!(
            document.payload["projection"]["projection"]["title"],
            "Third"
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn offline_document_lifecycle_restarts_replays_and_converges_to_tombstone() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("lifecycle-server.sqlite");
        let blobs = directory.path().join("lifecycle-blobs");
        let replica_path = directory.path().join("lifecycle-replica.sqlite");
        let runtime = ServiceRuntime::start(database.clone(), blobs.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let device = generate_device(
            "offline-lifecycle".into(),
            vec!["read".into(), "content:write".into()],
        )
        .unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();

        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        sync_server(&mut replica, &service, &device_id).await;
        replica
            .record_document(OfflineDocumentEdit {
                operation_id: "create".into(),
                document_id: "document".into(),
                logical_path: "note.org".into(),
                org: "#+title: Offline\n* Heading\n".into(),
                authored_at_us: 10,
            })
            .unwrap();
        assert_eq!(
            replica.document_id_for_path("note.org").unwrap().as_deref(),
            Some("document")
        );
        let rename = OfflineDocumentRename {
            operation_id: "rename".into(),
            document_id: "document".into(),
            logical_path: "renamed.org".into(),
            authored_at_us: 11,
        };
        assert_eq!(
            replica
                .rename_document(rename.clone())
                .unwrap()
                .device_sequence,
            2
        );
        assert_eq!(replica.rename_document(rename).unwrap().device_sequence, 2);
        assert_eq!(
            replica
                .document_id_for_path("renamed.org")
                .unwrap()
                .as_deref(),
            Some("document")
        );
        replica
            .tombstone_document(OfflineDocumentTombstone {
                operation_id: "tombstone".into(),
                document_id: "document".into(),
                authored_at_us: 12,
            })
            .unwrap();
        assert!(
            replica
                .document_id_for_path("renamed.org")
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            replica.record_document(OfflineDocumentEdit {
                operation_id: "resurrection".into(),
                document_id: "document".into(),
                logical_path: "renamed.org".into(),
                org: "#+title: Resurrected\n".into(),
                authored_at_us: 13,
            }),
            Err(Error::Content)
        ));
        drop(replica);

        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        for expected in ["create", "rename"] {
            let upload = replica.next_content_upload().unwrap().unwrap();
            assert_eq!(upload.operation_id, expected);
            let result = reconcile_content_upload(&service, &upload, &device_id, 20).await;
            assert_eq!(result["result"]["status"], "accepted");
            if expected == "rename" {
                assert_eq!(
                    result["result"]["authoritative"]["logical_path"],
                    "renamed.org"
                );
                let mut wrong_digest = result.clone();
                wrong_digest["result"]["authoritative"]["content_sha256"] = json!("b".repeat(64));
                assert!(matches!(
                    replica.record_content_result(wrong_digest),
                    Err(Error::Result)
                ));
            }
            replica.record_content_result(result).unwrap();
        }
        let upload = replica.next_content_upload().unwrap().unwrap();
        assert_eq!(upload.operation_id, "tombstone");
        let lost = reconcile_content_upload(&service, &upload, &device_id, 22).await;
        let replay = reconcile_content_upload(&service, &upload, &device_id, 23).await;
        assert_eq!(replay, lost);
        assert_eq!(replay["result"]["authoritative"]["active"], false);
        let mut false_resurrection = replay.clone();
        false_resurrection["result"]["authoritative"]["active"] = json!(true);
        false_resurrection["result"]["authoritative"]["content_sha256"] = json!("a".repeat(64));
        assert!(matches!(
            replica.record_content_result(false_resurrection),
            Err(Error::Result)
        ));
        replica.record_content_result(replay).unwrap();
        assert!(replica.next_content_upload().unwrap().is_none());

        pull_server_changes(&mut replica, &service).await;
        let document = replica.projection("document", "document").unwrap().unwrap();
        assert_eq!(document.revision, 3);
        assert!(document.tombstone);
        assert!(document.blob_digests.is_empty());
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn offline_asset_outbox_restarts_replays_and_converges() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("asset-server.sqlite");
        let blobs = directory.path().join("asset-blobs");
        let replica_path = directory.path().join("asset-replica.sqlite");
        let runtime = ServiceRuntime::start(database.clone(), blobs.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let device = generate_device(
            "offline-asset".into(),
            vec!["read".into(), "content:write".into()],
        )
        .unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();

        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        sync_server(&mut replica, &service, &device_id).await;
        assert!(matches!(
            replica.record_asset(OfflineAssetCreate {
                operation_id: "oversized".into(),
                original_name: "large.png".into(),
                media_type: "application/octet-stream".into(),
                bytes: vec![0; crate::application::MAX_OFFLINE_ASSET_BYTES as usize + 1],
                authored_at_us: 9,
            }),
            Err(Error::Content)
        ));
        assert_eq!(replica.status().unwrap().next_device_sequence, 1);
        let create = OfflineAssetCreate {
            operation_id: "offline-asset-1".into(),
            original_name: "image.png".into(),
            media_type: "image/png".into(),
            bytes: b"offline image".to_vec(),
            authored_at_us: 10,
        };
        let recorded = replica.record_asset(create.clone()).unwrap();
        assert_eq!(recorded.device_sequence, 1);
        assert_eq!(replica.record_asset(create.clone()).unwrap(), recorded);
        assert!(matches!(
            replica.asset(&recorded.logical_path).unwrap(),
            ReplicaAssetState::Staged(asset) if asset.bytes == b"offline image"
        ));
        let document = replica
            .record_document(OfflineDocumentEdit {
                operation_id: "offline-document-2".into(),
                document_id: "asset-note".into(),
                logical_path: "asset-note.org".into(),
                org: format!("#+title: Asset\n[[file:{}]]\n", recorded.logical_path),
                authored_at_us: 11,
            })
            .unwrap();
        assert_eq!(document.device_sequence, 2);
        let mut changed = create;
        changed.bytes.push(0);
        assert!(matches!(replica.record_asset(changed), Err(Error::Content)));
        let upload = match replica.next_device_upload().unwrap().unwrap() {
            PendingDeviceUpload::Asset(upload) => upload,
            _ => panic!("expected asset upload"),
        };
        let missing = reconcile_asset_upload(&service, &upload, &device_id, 20).await;
        assert!(matches!(
            missing,
            Err(crate::service::ServiceError::Store(
                crate::store::StoreError::BlobRequired { .. }
            ))
        ));
        assert_eq!(
            service
                .replica_state(device_id.clone())
                .await
                .unwrap()
                .next_device_sequence,
            1
        );
        service
            .stage_blob(
                &upload.content_sha256,
                upload.bytes.clone(),
                upload.media_type.clone(),
                21,
            )
            .await
            .unwrap();
        drop(service);
        runtime.finish().await.unwrap();

        drop(replica);
        let runtime = ServiceRuntime::start(database, blobs, 8).await.unwrap();
        let service = runtime.handle();
        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        let retry = replica.next_asset_upload().unwrap().unwrap();
        assert_eq!(retry.canonical_payload, upload.canonical_payload);
        let missing_after_restart = reconcile_asset_upload(&service, &retry, &device_id, 22).await;
        assert!(matches!(
            missing_after_restart,
            Err(crate::service::ServiceError::Store(
                crate::store::StoreError::BlobRequired { .. }
            ))
        ));
        service
            .stage_blob(
                &retry.content_sha256,
                retry.bytes.clone(),
                retry.media_type.clone(),
                23,
            )
            .await
            .unwrap();
        let accepted = reconcile_asset_upload(&service, &retry, &device_id, 24)
            .await
            .unwrap();
        assert_eq!(accepted["result"]["status"], "accepted");
        let replay = reconcile_asset_upload(&service, &retry, &device_id, 25)
            .await
            .unwrap();
        assert_eq!(replay, accepted);
        replica.record_asset_result(accepted).unwrap();
        assert_eq!(replica.asset_results(None, 10).unwrap().items.len(), 1);
        assert!(matches!(
            replica.asset(&recorded.logical_path).unwrap(),
            ReplicaAssetState::Staged(asset) if asset.bytes == b"offline image"
        ));
        let document = match replica.next_device_upload().unwrap().unwrap() {
            PendingDeviceUpload::Content(upload) => upload,
            _ => panic!("expected dependent document upload"),
        };
        let document_result = reconcile_content_upload(&service, &document, &device_id, 26).await;
        assert_eq!(document_result["result"]["status"], "accepted");
        replica.record_content_result(document_result).unwrap();
        assert!(replica.next_device_upload().unwrap().is_none());

        pull_server_changes(&mut replica, &service).await;
        assert!(matches!(
            replica.asset(&recorded.logical_path).unwrap(),
            ReplicaAssetState::Available(asset) if asset.bytes == b"offline image"
        ));
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn offline_asset_conflict_is_queryable_and_reauthorable_after_resync() {
        let directory = tempfile::tempdir().unwrap();
        let replica_path = directory.path().join("replica.sqlite");
        let runtime = ServiceRuntime::start(
            directory.path().join("server.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device(
            "asset-conflict".into(),
            vec!["read".into(), "content:write".into()],
        )
        .unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        sync_server(&mut replica, &service, &device_id).await;
        let create = OfflineAssetCreate {
            operation_id: "stale-asset".into(),
            original_name: "image.png".into(),
            media_type: "image/png".into(),
            bytes: b"stale asset".to_vec(),
            authored_at_us: 2,
        };
        let recorded = replica.record_asset(create.clone()).unwrap();
        service
            .replace_note(
                prepare_note(
                    None,
                    "unrelated.org".into(),
                    "#+title: Changed\n".into(),
                    None,
                )
                .unwrap(),
                3,
                Some(device_id.clone()),
            )
            .await
            .unwrap();
        let changed_state = service.replica_state(device_id.clone()).await.unwrap();
        service
            .expire_replica_changes_through(changed_state.cursor)
            .await
            .unwrap();
        let upload = replica.next_asset_upload().unwrap().unwrap();
        service
            .stage_blob(
                &upload.content_sha256,
                upload.bytes.clone(),
                upload.media_type.clone(),
                4,
            )
            .await
            .unwrap();
        let result = reconcile_asset_upload(&service, &upload, &device_id, 5)
            .await
            .unwrap();
        assert_eq!(result["result"]["conflict_reason"], "cursor_changed");
        replica.record_asset_result(result).unwrap();
        let conflicts = replica.asset_conflicts(None, 10).unwrap();
        assert_eq!(conflicts.items.len(), 1);
        assert_eq!(
            conflicts.items[0].conflict_reason,
            Some(AssetConflictReason::CursorChanged)
        );
        assert_eq!(conflicts.items[0].logical_path, recorded.logical_path);
        assert_eq!(conflicts.items[0].operation["operation"]["size_bytes"], 11);

        replica
            .observe_state(replica_state_response(&changed_state).unwrap())
            .unwrap();
        install_server_snapshot(&mut replica, &service, &device_id).await;
        install_missing_blobs(&mut replica, &service).await;
        let mut reauthored = create;
        reauthored.operation_id = "reauthored-asset".into();
        assert_eq!(replica.record_asset(reauthored).unwrap().device_sequence, 2);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn malformed_offline_org_survives_restart_as_an_explicit_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let replica_path = directory.path().join("replica.sqlite");
        let runtime = ServiceRuntime::start(
            directory.path().join("server.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device(
            "invalid-org".into(),
            vec!["read".into(), "content:write".into()],
        )
        .unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        sync_server(&mut replica, &service, &device_id).await;
        replica
            .record_document(OfflineDocumentEdit {
                operation_id: "invalid-org".into(),
                document_id: "invalid-document".into(),
                logical_path: "invalid.org".into(),
                org: "#+title: Invalid\n[[file:../outside.png]]\n".into(),
                authored_at_us: 2,
            })
            .unwrap();
        drop(replica);

        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        let upload = replica.next_content_upload().unwrap().unwrap();
        let result = reconcile_content_upload(&service, &upload, &device_id, 3).await;
        assert_eq!(result["result"]["conflict_reason"], "invalid_content");
        replica.record_content_result(result).unwrap();
        let conflicts = replica.content_conflicts(None, 10).unwrap();
        assert_eq!(conflicts.items.len(), 1);
        assert_eq!(
            conflicts.items[0].conflict_reason,
            Some(ContentConflictReason::InvalidContent)
        );
        assert!(replica.next_device_upload().unwrap().is_none());
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn old_server_tombstone_does_not_block_a_new_server_document() {
        let directory = tempfile::tempdir().unwrap();
        let replica_path = directory.path().join("replica.sqlite");
        let device = generate_device(
            "offline-server-change".into(),
            vec!["read".into(), "content:write".into()],
        )
        .unwrap();
        let device_id = device.device_id.clone();
        let first = ServiceRuntime::start(
            directory.path().join("first.sqlite"),
            directory.path().join("first-blobs"),
            8,
        )
        .await
        .unwrap();
        let service = first.handle();
        service.create_device(device.clone(), 1).await.unwrap();
        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        sync_server(&mut replica, &service, &device_id).await;
        replica
            .record_document(OfflineDocumentEdit {
                operation_id: "old-create".into(),
                document_id: "document".into(),
                logical_path: "note.org".into(),
                org: "#+title: Old\n".into(),
                authored_at_us: 2,
            })
            .unwrap();
        replica
            .tombstone_document(OfflineDocumentTombstone {
                operation_id: "old-tombstone".into(),
                document_id: "document".into(),
                authored_at_us: 3,
            })
            .unwrap();
        for received_at_us in [4, 5] {
            let upload = replica.next_content_upload().unwrap().unwrap();
            let result =
                reconcile_content_upload(&service, &upload, &device_id, received_at_us).await;
            replica.record_content_result(result).unwrap();
        }
        drop(service);
        first.finish().await.unwrap();

        let second = ServiceRuntime::start(
            directory.path().join("second.sqlite"),
            directory.path().join("second-blobs"),
            8,
        )
        .await
        .unwrap();
        let service = second.handle();
        service.create_device(device, 1).await.unwrap();
        let state = service.replica_state(device_id.clone()).await.unwrap();
        replica
            .observe_state(replica_state_response(&state).unwrap())
            .unwrap();
        install_server_snapshot(&mut replica, &service, &device_id).await;
        install_missing_blobs(&mut replica, &service).await;
        let recorded = replica
            .record_document(OfflineDocumentEdit {
                operation_id: "new-create".into(),
                document_id: "document".into(),
                logical_path: "note.org".into(),
                org: "#+title: New\n".into(),
                authored_at_us: 6,
            })
            .unwrap();
        assert_eq!(recorded.device_sequence, 1);
        assert!(matches!(
            replica.tombstone_document(OfflineDocumentTombstone {
                operation_id: "new-tombstone".into(),
                document_id: "document".into(),
                authored_at_us: 7,
            }),
            Err(Error::Content)
        ));
        replica
            .install_content_capabilities(offline_content_capabilities(&state.server_id).unwrap())
            .unwrap();
        let upload = replica.next_content_upload().unwrap().unwrap();
        assert_eq!(
            upload.submission().unwrap()["operation"]["mutation"]["base"]["kind"],
            "absent"
        );
        let result = reconcile_content_upload(&service, &upload, &device_id, 8).await;
        assert_eq!(result["result"]["status"], "accepted");
        drop(service);
        second.finish().await.unwrap();
    }

    #[tokio::test]
    async fn lifecycle_capability_gates_no_file_id_rename_and_delete_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let replica_path = directory.path().join("replica.sqlite");
        let runtime = ServiceRuntime::start(
            directory.path().join("server.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device(
            "lifecycle-capability".into(),
            vec!["read".into(), "content:write".into()],
        )
        .unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        service
            .replace_note(
                prepare_note(
                    Some("opaque-document".into()),
                    "without-file-id.org".into(),
                    "#+title: No file ID\n* Heading\n".into(),
                    None,
                )
                .unwrap(),
                2,
                None,
            )
            .await
            .unwrap();
        service
            .replace_note(
                prepare_note(
                    Some("other-document".into()),
                    "taken.org".into(),
                    "#+title: Taken\n".into(),
                    None,
                )
                .unwrap(),
                3,
                None,
            )
            .await
            .unwrap();
        let state = service.replica_state(device_id.clone()).await.unwrap();
        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        replica
            .observe_state(replica_state_response(&state).unwrap())
            .unwrap();
        install_server_snapshot(&mut replica, &service, &device_id).await;
        install_missing_blobs(&mut replica, &service).await;
        let document_id = replica
            .document_id_for_path("without-file-id.org")
            .unwrap()
            .unwrap();
        assert_eq!(document_id, "opaque-document");
        let rename = OfflineDocumentRename {
            operation_id: "rename".into(),
            document_id: document_id.clone(),
            logical_path: "renamed.org".into(),
            authored_at_us: 3,
        };
        assert!(matches!(
            replica.rename_document(rename.clone()),
            Err(Error::Content)
        ));
        assert!(replica.next_content_upload().unwrap().is_none());
        assert!(
            replica
                .install_content_capabilities(
                    offline_content_capabilities(&"f".repeat(32)).unwrap()
                )
                .is_err()
        );
        replica
            .install_content_capabilities(offline_content_capabilities(&state.server_id).unwrap())
            .unwrap();
        assert!(matches!(
            replica.rename_document(OfflineDocumentRename {
                operation_id: "collision".into(),
                document_id: document_id.clone(),
                logical_path: "taken.org".into(),
                authored_at_us: 3,
            }),
            Err(Error::PathConflict)
        ));
        assert!(replica.next_content_upload().unwrap().is_none());
        drop(replica);

        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        assert_eq!(
            replica
                .document_id_for_path("taken.org")
                .unwrap()
                .as_deref(),
            Some("other-document")
        );
        assert_eq!(
            replica
                .document_id_for_path("without-file-id.org")
                .unwrap()
                .as_deref(),
            Some("opaque-document")
        );
        let rename = OfflineDocumentRename {
            authored_at_us: 4,
            ..rename
        };
        replica.rename_document(rename).unwrap();
        drop(replica);

        let mut replica = LocalReplica::open(&replica_path, &device_id).unwrap();
        assert!(
            replica
                .document_id_for_path("without-file-id.org")
                .unwrap()
                .is_none()
        );
        let document_id = replica
            .document_id_for_path("renamed.org")
            .unwrap()
            .unwrap();
        assert_eq!(document_id, "opaque-document");
        replica
            .tombstone_document(OfflineDocumentTombstone {
                operation_id: "tombstone".into(),
                document_id,
                authored_at_us: 5,
            })
            .unwrap();
        for received_at_us in [6, 7] {
            let upload = replica.next_content_upload().unwrap().unwrap();
            let result =
                reconcile_content_upload(&service, &upload, &device_id, received_at_us).await;
            replica.record_content_result(result).unwrap();
        }
        pull_server_changes(&mut replica, &service).await;
        assert!(
            replica
                .document_id_for_path("renamed.org")
                .unwrap()
                .is_none()
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn conflicting_document_does_not_publish_an_unreferenced_blob() {
        let directory = tempfile::tempdir().unwrap();
        let blobs = directory.path().join("blobs");
        let runtime =
            ServiceRuntime::start(directory.path().join("server.sqlite"), blobs.clone(), 8)
                .await
                .unwrap();
        let service = runtime.handle();
        let device = generate_device(
            "offline-content-conflict".into(),
            vec!["read".into(), "content:write".into()],
        )
        .unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let original = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: Original\n".into(),
            None,
        )
        .unwrap();
        service.replace_note(original, 2, None).await.unwrap();

        let mut replica =
            LocalReplica::open(directory.path().join("replica.sqlite"), &device_id).unwrap();
        sync_server(&mut replica, &service, &device_id).await;
        let offline_org = "#+title: Offline conflict\n";
        let offline_digest = format!("{:x}", Sha256::digest(offline_org.as_bytes()));
        replica
            .record_document(OfflineDocumentEdit {
                operation_id: "conflicting-document".into(),
                document_id: "document".into(),
                logical_path: "note.org".into(),
                org: offline_org.into(),
                authored_at_us: 3,
            })
            .unwrap();
        let online = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: Online wins\n".into(),
            Some(1),
        )
        .unwrap();
        service.replace_note(online, 4, None).await.unwrap();

        let upload = replica.next_content_upload().unwrap().unwrap();
        let result = reconcile_content_upload(&service, &upload, &device_id, 5).await;
        assert_eq!(result["result"]["conflict_reason"], "base_changed");
        assert!(
            !blobs
                .join(&offline_digest[..2])
                .join(offline_digest)
                .exists()
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    fn malformed_content_results(valid: &Value) -> Vec<Value> {
        let mut wrong_revision = valid.clone();
        wrong_revision["result"]["authoritative"]["revision"] = json!(2);
        let mut wrong_path = valid.clone();
        wrong_path["result"]["authoritative"]["logical_path"] = json!("wrong.org");
        let mut wrong_digest = valid.clone();
        wrong_digest["result"]["authoritative"]["content_sha256"] = json!("b".repeat(64));
        let mut tombstone = valid.clone();
        tombstone["result"]["authoritative"]["active"] = json!(false);
        tombstone["result"]["authoritative"]["content_sha256"] = Value::Null;
        let mut wrong_server = valid.clone();
        wrong_server["result"]["cursor"] =
            json!(encode_cursor(&"f".repeat(32), 1, &"e".repeat(32)).unwrap());
        vec![
            wrong_revision,
            wrong_path,
            wrong_digest,
            tombstone,
            wrong_server,
        ]
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
            .observe_state(replica_state_response(&state).unwrap())
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
            .install_review_pack(typed_review_pack(review_pack_json(&pack).unwrap()))
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
            .observe_state(replica_state_response(&changed_state).unwrap())
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
            .install_review_pack(typed_review_pack(review_pack_json(&pack).unwrap()))
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
        assert_eq!(stale.status().unwrap().next_device_sequence, 2);
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
            .install_review_pack(typed_review_pack(review_pack_json(&active).unwrap()))
            .unwrap();
        replica
            .install_review_pack(typed_review_pack(review_pack_json(&historical).unwrap()))
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
        let mut invalid_version = typed_review_pack(wire.clone());
        invalid_version.version = 1;
        assert!(matches!(
            replica.install_review_pack(invalid_version),
            Err(Error::ReviewWire)
        ));
        let mut exhausted = wire.clone();
        exhausted["pack"]["current_position"] = json!(1);
        assert!(matches!(
            replica.install_review_pack(typed_review_pack(exhausted)),
            Err(Error::ReviewWire)
        ));
        let mut invalid_state = wire.clone();
        invalid_state["pack"]["items"][0]["review_state"]["review_count"] = json!(1);
        assert!(matches!(
            replica.install_review_pack(typed_review_pack(invalid_state)),
            Err(Error::ReviewWire)
        ));
        let mut invalid_asset = wire.clone();
        invalid_asset["pack"]["assets"][0]["content_sha256"] = json!("b".repeat(64));
        assert!(matches!(
            replica.install_review_pack(typed_review_pack(invalid_asset)),
            Err(Error::ReviewWire)
        ));
        let mut missing_presentation_asset = wire.clone();
        missing_presentation_asset["pack"]["items"][0]["asset_paths"] = json!([]);
        assert!(matches!(
            replica.install_review_pack(typed_review_pack(missing_presentation_asset)),
            Err(Error::ReviewWire)
        ));
        let pack_id = replica
            .install_review_pack(typed_review_pack(wire))
            .unwrap();
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
            .observe_state(replica_state_response(&state).unwrap())
            .unwrap();
        replica
            .install_content_capabilities(offline_content_capabilities(&state.server_id).unwrap())
            .unwrap();
        replica
            .install_asset_capabilities(offline_asset_capabilities(&state.server_id).unwrap())
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
            .install_review_pack(typed_review_pack(review_pack_json(&pack).unwrap()))
            .unwrap()
    }

    async fn reconcile_content_upload(
        service: &ServiceHandle,
        upload: &PendingContentUpload,
        device_id: &str,
        received_at_us: u64,
    ) -> Value {
        let submission: OfflineContentSubmission =
            serde_json::from_value(upload.submission().unwrap()).unwrap();
        let decoded = submission.decode().unwrap();
        let result = service
            .reconcile_content_operation(
                decoded.version,
                decoded.operation,
                decoded.canonical_payload,
                Some(device_id.into()),
                received_at_us,
            )
            .await
            .unwrap();
        offline_content_result_json(&result).unwrap()
    }

    async fn reconcile_asset_upload(
        service: &ServiceHandle,
        upload: &PendingAssetUpload,
        device_id: &str,
        received_at_us: u64,
    ) -> Result<Value, crate::service::ServiceError> {
        let submission: OfflineAssetSubmission =
            serde_json::from_value(upload.submission().unwrap()).unwrap();
        let decoded = submission.decode().unwrap();
        let result = service
            .reconcile_asset_operation(
                decoded.version,
                decoded.operation,
                decoded.canonical_payload,
                Some(device_id.into()),
                received_at_us,
            )
            .await?;
        Ok(offline_asset_result_json(&result).unwrap())
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
            let wire = replica_snapshot_page(&page).unwrap();
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
        for digest in replica.missing_projection_blobs().unwrap() {
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
        let submission: OfflineReviewSubmission =
            serde_json::from_value(upload.submission().unwrap()).unwrap();
        let decoded = submission.decode().unwrap();
        let result = service
            .reconcile_offline_operation(
                decoded.version,
                decoded.operation,
                decoded.canonical_payload,
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
            let page = replica_change_page(&server_id, after, &token, latest, &changes).unwrap();
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

    fn decode_response<T: serde::de::DeserializeOwned>(value: Value) -> T {
        serde_json::from_value(value).unwrap()
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

    fn typed_review_pack(value: Value) -> crate::protocol::http_v1::ReviewPackEnvelopeV2 {
        serde_json::from_value(value).unwrap()
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
