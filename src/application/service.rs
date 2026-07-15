use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        mpsc::{self, SyncSender, TrySendError},
    },
    thread,
    time::{SystemTime, UNIX_EPOCH},
};

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::oneshot;

use super::{
    AnswerInput, ApplicationError, AssetHead, BackupError, BackupSummary, ContentChange,
    DevicePrincipal, DeviceRecord, DocumentHead, DocumentRevision, ImportSummary,
    MAX_OFFLINE_ASSET_BYTES, NewDevice, NodeRecord, OfflineAssetOperation, OfflineAssetResult,
    OfflineContentConflictReason, OfflineContentOperation, OfflineContentResult,
    OfflineReviewOperation, OfflineReviewResult, PreparedAsset, PreparedNote, PreparedServerImport,
    PreparedThema, QueryDueThemata, QueryDueThemataPage, ReplicaChange, ReplicaState,
    ReviewAttemptView, ReviewPackAsset, ReviewPackView, ReviewSessionView, ReviewSource,
    SettingsInput, SettingsRecord, SnapshotPage, SnapshotPosition, ThemaRecord, create_backup,
    persist_server_import, prepare_offline_document, prune_backups, retained_asset_paths_in_org,
    token_digest, validate_restore, verify_backup,
};
use crate::{
    blob::{BlobError, BlobMeta, BlobStore},
    domain::{DocumentId, DueThema, LegacyDatabaseInfo, LegacyThema, ThemaId},
    store::{DocumentAssetAssociation, SqliteStore, StoreError, StoredDocument},
};

#[derive(Clone)]
pub(crate) struct ServiceHandle {
    sender: SyncSender<StoreCommand>,
    blobs: Arc<BlobStore>,
}

impl ServiceHandle {
    pub(crate) async fn replace(
        &self,
        prepared: PreparedServerImport,
    ) -> Result<ImportSummary, ServiceError> {
        let blob = self
            .blobs
            .put(prepared.org.as_bytes(), "text/org; charset=utf-8")?;
        let created_at_us = system_time_us()?;
        request(&self.sender, |response| StoreCommand::Replace {
            prepared: Box::new(prepared),
            blob,
            created_at_us,
            response,
        })
        .await?
        .map_err(ServiceError::Application)
    }

    pub(crate) async fn get(
        &self,
        document_id: DocumentId,
    ) -> Result<Option<StoredDocument>, ServiceError> {
        request(&self.sender, |response| StoreCommand::Get {
            document_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn legacy_info(&self) -> Result<Option<LegacyDatabaseInfo>, ServiceError> {
        request(&self.sender, |response| StoreCommand::LegacyInfo {
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn get_legacy_thema(
        &self,
        thema_id: ThemaId,
    ) -> Result<Option<LegacyThema>, ServiceError> {
        request(&self.sender, |response| StoreCommand::GetLegacyThema {
            thema_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn due_themata(
        &self,
        query: QueryDueThemata,
    ) -> Result<Vec<DueThema>, ServiceError> {
        request(&self.sender, |response| StoreCommand::DueThemata {
            query,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn due_themata_page(
        &self,
        query: QueryDueThemataPage,
    ) -> Result<Vec<DueThema>, ServiceError> {
        request(&self.sender, |response| StoreCommand::DueThemataPage {
            query,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn replace_note(
        &self,
        prepared: PreparedNote,
        created_at_us: i64,
        author_device_id: Option<String>,
    ) -> Result<DocumentHead, ServiceError> {
        let blob = self
            .blobs
            .put(prepared.org.as_bytes(), "text/org; charset=utf-8")?;
        request(&self.sender, |response| StoreCommand::ReplaceNote {
            prepared: Box::new(prepared),
            blob,
            created_at_us,
            author_device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn note(
        &self,
        document_id: DocumentId,
    ) -> Result<Option<(DocumentHead, Vec<u8>)>, ServiceError> {
        let head = request(&self.sender, |response| StoreCommand::DocumentHead {
            document_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)?;
        read_document_blob(&self.blobs, head)
    }

    pub(crate) async fn note_revisions(
        &self,
        document_id: DocumentId,
        after: u64,
        limit: u16,
    ) -> Result<Vec<DocumentRevision>, ServiceError> {
        request(&self.sender, |response| StoreCommand::DocumentRevisions {
            document_id,
            after,
            limit,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn note_revision(
        &self,
        document_id: DocumentId,
        revision: u64,
    ) -> Result<Option<(DocumentRevision, Vec<u8>)>, ServiceError> {
        let record = request(&self.sender, |response| StoreCommand::DocumentRevision {
            document_id,
            revision,
            response,
        })
        .await?
        .map_err(ServiceError::Store)?;
        read_revision_blob(&self.blobs, record)
    }

    pub(crate) async fn rename_note(
        &self,
        document_id: DocumentId,
        logical_path: String,
        expected_revision: u64,
        updated_at_us: i64,
        author_device_id: Option<String>,
    ) -> Result<DocumentHead, ServiceError> {
        request(&self.sender, |response| StoreCommand::RenameDocument {
            document_id,
            logical_path,
            expected_revision,
            updated_at_us,
            author_device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn tombstone_note(
        &self,
        document_id: DocumentId,
        expected_revision: u64,
        updated_at_us: i64,
        author_device_id: Option<String>,
    ) -> Result<DocumentHead, ServiceError> {
        request(&self.sender, |response| StoreCommand::TombstoneDocument {
            document_id,
            expected_revision,
            updated_at_us,
            author_device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn put_asset(
        &self,
        prepared: PreparedAsset,
        bytes: Vec<u8>,
        media_type: String,
        created_at_us: i64,
        author_device_id: Option<String>,
    ) -> Result<AssetHead, ServiceError> {
        let expected_digest = prepared
            .logical_path
            .split('/')
            .nth(1)
            .ok_or(ServiceError::AssetDigest)?;
        let actual_digest = format!("{:x}", Sha256::digest(&bytes));
        if actual_digest != expected_digest {
            return Err(ServiceError::AssetDigest);
        }
        let blob = self.blobs.put(&bytes, &media_type)?;
        request(&self.sender, |response| StoreCommand::PutAsset {
            prepared,
            blob,
            created_at_us,
            author_device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn stage_blob(
        &self,
        expected_digest: &str,
        bytes: Vec<u8>,
        media_type: String,
        created_at_us: i64,
    ) -> Result<BlobMeta, ServiceError> {
        validate_blob_metadata(&bytes, &media_type)?;
        let actual_digest = format!("{:x}", Sha256::digest(&bytes));
        if actual_digest != expected_digest {
            return Err(ServiceError::AssetDigest);
        }
        let blob = self.blobs.put(&bytes, &media_type)?;
        request(&self.sender, |response| StoreCommand::StageBlob {
            blob: blob.clone(),
            created_at_us,
            response,
        })
        .await?
        .map_err(ServiceError::Store)?;
        Ok(blob)
    }

    pub(crate) async fn asset(
        &self,
        asset_path: String,
    ) -> Result<Option<(AssetHead, Vec<u8>)>, ServiceError> {
        let head = request(&self.sender, |response| StoreCommand::AssetHead {
            asset_path,
            response,
        })
        .await?
        .map_err(ServiceError::Store)?;
        read_asset_blob(&self.blobs, head)
    }

    pub(crate) async fn tombstone_asset(
        &self,
        asset_path: String,
        expected_revision: u64,
        updated_at_us: i64,
        author_device_id: Option<String>,
    ) -> Result<AssetHead, ServiceError> {
        request(&self.sender, |response| StoreCommand::TombstoneAsset {
            asset_path,
            expected_revision,
            updated_at_us,
            author_device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn changes(
        &self,
        after: u64,
        limit: u16,
    ) -> Result<Vec<ContentChange>, ServiceError> {
        request(&self.sender, |response| StoreCommand::ContentChanges {
            after,
            limit,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn latest_content_cursor(&self) -> Result<u64, ServiceError> {
        request(&self.sender, |response| StoreCommand::LatestContentCursor {
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn replica_state(
        &self,
        device_id: String,
    ) -> Result<ReplicaState, ServiceError> {
        request(&self.sender, |response| StoreCommand::ReplicaState {
            device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn replica_changes(
        &self,
        server_id: String,
        after: u64,
        after_token: String,
        limit: u16,
    ) -> Result<(Vec<ReplicaChange>, u64, String), ServiceError> {
        request(&self.sender, |response| StoreCommand::ReplicaChanges {
            server_id,
            after,
            after_token,
            limit,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn create_replica_snapshot(
        &self,
        device_id: String,
        created_at_us: u64,
        limit: u16,
    ) -> Result<SnapshotPage, ServiceError> {
        request(&self.sender, |response| {
            StoreCommand::CreateReplicaSnapshot {
                device_id,
                created_at_us,
                limit,
                response,
            }
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn replica_snapshot_page(
        &self,
        device_id: String,
        position: SnapshotPosition,
        limit: u16,
    ) -> Result<SnapshotPage, ServiceError> {
        request(&self.sender, |response| StoreCommand::ReplicaSnapshotPage {
            device_id,
            position,
            limit,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    #[cfg(test)]
    pub(crate) async fn expire_replica_changes_through(
        &self,
        cursor: u64,
    ) -> Result<(), ServiceError> {
        request(&self.sender, |response| {
            StoreCommand::ExpireReplicaChanges { cursor, response }
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn blob(
        &self,
        digest: String,
    ) -> Result<Option<(BlobMeta, Vec<u8>)>, ServiceError> {
        let meta = request(&self.sender, |response| StoreCommand::BlobMeta {
            digest,
            response,
        })
        .await?
        .map_err(ServiceError::Store)?;
        let Some(meta) = meta else {
            return Ok(None);
        };
        let blobs = self.blobs.clone();
        let digest = meta.sha256.clone();
        let bytes = tokio::task::spawn_blocking(move || blobs.read(&digest))
            .await
            .map_err(|_| ServiceError::Stopped)??;
        Ok(Some((meta, bytes)))
    }

    pub(crate) async fn settings(&self) -> Result<SettingsRecord, ServiceError> {
        request(&self.sender, |response| StoreCommand::GetSettings {
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn update_settings(
        &self,
        input: SettingsInput,
        expected_revision: u64,
        updated_at_us: i64,
        author_device_id: Option<String>,
    ) -> Result<SettingsRecord, ServiceError> {
        let input = input.validate().map_err(ServiceError::Settings)?;
        request(&self.sender, |response| StoreCommand::UpdateSettings {
            input,
            expected_revision,
            updated_at_us,
            author_device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn put_thema(
        &self,
        prepared: PreparedThema,
        initial_review_day: u32,
        updated_at_us: i64,
        author_device_id: Option<String>,
    ) -> Result<ThemaRecord, ServiceError> {
        request(&self.sender, |response| StoreCommand::PutThema {
            prepared: Box::new(prepared),
            initial_review_day,
            updated_at_us,
            author_device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn thema(
        &self,
        thema_id: String,
    ) -> Result<Option<ThemaRecord>, ServiceError> {
        request(&self.sender, |response| StoreCommand::GetThema {
            thema_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn archive_thema(
        &self,
        thema_id: String,
        expected_revision: u64,
        archived: bool,
        updated_at_us: i64,
        author_device_id: Option<String>,
    ) -> Result<ThemaRecord, ServiceError> {
        request(&self.sender, |response| StoreCommand::ArchiveThema {
            thema_id,
            expected_revision,
            archived,
            updated_at_us,
            author_device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn search_themata(
        &self,
        query: String,
        limit: u16,
        after: Option<String>,
    ) -> Result<Vec<ThemaRecord>, ServiceError> {
        request(&self.sender, |response| StoreCommand::SearchThemata {
            query,
            limit,
            after,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn node(&self, node_id: String) -> Result<Option<NodeRecord>, ServiceError> {
        request(&self.sender, |response| StoreCommand::GetNode {
            node_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn search_nodes(
        &self,
        query: String,
        limit: u16,
        after: Option<String>,
    ) -> Result<Vec<NodeRecord>, ServiceError> {
        request(&self.sender, |response| StoreCommand::SearchNodes {
            query,
            limit,
            after,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn graph_nodes(
        &self,
        node_id: String,
        inbound: bool,
        limit: u16,
        after: Option<String>,
    ) -> Result<Vec<NodeRecord>, ServiceError> {
        request(&self.sender, |response| StoreCommand::GraphNodes {
            node_id,
            inbound,
            limit,
            after,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn create_device(
        &self,
        device: NewDevice,
        created_at_us: i64,
    ) -> Result<DeviceRecord, ServiceError> {
        request(&self.sender, |response| StoreCommand::CreateDevice {
            device,
            created_at_us,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn devices(&self) -> Result<Vec<DeviceRecord>, ServiceError> {
        request(&self.sender, |response| StoreCommand::ListDevices {
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn revoke_device(
        &self,
        device_id: String,
        revoked_at_us: i64,
    ) -> Result<DeviceRecord, ServiceError> {
        request(&self.sender, |response| StoreCommand::RevokeDevice {
            device_id,
            revoked_at_us,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn authenticate(
        &self,
        token: String,
        used_at_us: i64,
    ) -> Result<Option<DevicePrincipal>, ServiceError> {
        let digest = token_digest(&token);
        request(&self.sender, |response| StoreCommand::Authenticate {
            digest,
            used_at_us,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn create_review_session(
        &self,
        source: ReviewSource,
        target_count: Option<u32>,
        owner_device_id: Option<String>,
        started_at_us: u64,
    ) -> Result<ReviewSessionView, ServiceError> {
        let source = source.validate().map_err(ServiceError::Review)?;
        if target_count == Some(0) {
            return Err(ServiceError::Review(super::ReviewError::Target));
        }
        request(&self.sender, |response| StoreCommand::CreateReviewSession {
            source,
            target_count,
            owner_device_id,
            started_at_us,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn active_review_session(
        &self,
        actor_device_id: Option<String>,
    ) -> Result<Option<ReviewSessionView>, ServiceError> {
        request(&self.sender, |response| StoreCommand::ActiveReviewSession {
            actor_device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn resume_review_session(
        &self,
        session_id: String,
        actor_device_id: Option<String>,
    ) -> Result<ReviewSessionView, ServiceError> {
        request(&self.sender, |response| StoreCommand::ResumeReviewSession {
            session_id,
            actor_device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn stop_review_session(
        &self,
        session_id: String,
        actor_device_id: Option<String>,
        abort: bool,
        ended_at_us: u64,
    ) -> Result<ReviewSessionView, ServiceError> {
        request(&self.sender, |response| StoreCommand::StopReviewSession {
            session_id,
            actor_device_id,
            abort,
            ended_at_us,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn begin_review_attempt(
        &self,
        session_id: String,
        actor_device_id: Option<String>,
        started_at_us: u64,
    ) -> Result<ReviewAttemptView, ServiceError> {
        request(&self.sender, |response| StoreCommand::BeginReviewAttempt {
            session_id,
            actor_device_id,
            started_at_us,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn grade_review_attempt(
        &self,
        attempt_id: String,
        actor_device_id: Option<String>,
        answer: AnswerInput,
        answered_at_us: u64,
    ) -> Result<ReviewAttemptView, ServiceError> {
        let fuzz_sample = random_fuzz_sample()?;
        request(&self.sender, |response| StoreCommand::GradeReviewAttempt {
            attempt_id,
            actor_device_id,
            answer,
            answered_at_us,
            fuzz_sample,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn override_review_attempt(
        &self,
        attempt_id: String,
        actor_device_id: Option<String>,
    ) -> Result<ReviewAttemptView, ServiceError> {
        request(&self.sender, |response| {
            StoreCommand::OverrideReviewAttempt {
                attempt_id,
                actor_device_id,
                response,
            }
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn override_local_review_attempt(
        &self,
        attempt_id: String,
        request_id: String,
        request_sha256: String,
        operated_at_us: u64,
    ) -> Result<ReviewAttemptView, ServiceError> {
        validate_local_review_replay(&request_id, &request_sha256, operated_at_us)?;
        request(&self.sender, |response| {
            StoreCommand::OverrideLocalReviewAttempt {
                attempt_id,
                request_id,
                request_sha256,
                operated_at_us,
                response,
            }
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn toggle_attempt_suspension(
        &self,
        attempt_id: String,
        actor_device_id: Option<String>,
    ) -> Result<ReviewAttemptView, ServiceError> {
        request(&self.sender, |response| {
            StoreCommand::ToggleAttemptSuspension {
                attempt_id,
                actor_device_id,
                response,
            }
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn toggle_local_attempt_suspension(
        &self,
        attempt_id: String,
        request_id: String,
        request_sha256: String,
        operated_at_us: u64,
    ) -> Result<ReviewAttemptView, ServiceError> {
        validate_local_review_replay(&request_id, &request_sha256, operated_at_us)?;
        request(&self.sender, |response| {
            StoreCommand::ToggleLocalAttemptSuspension {
                attempt_id,
                request_id,
                request_sha256,
                operated_at_us,
                response,
            }
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn commit_review_attempt(
        &self,
        attempt_id: String,
        actor_device_id: Option<String>,
        committed_at_us: u64,
    ) -> Result<String, ServiceError> {
        request(&self.sender, |response| StoreCommand::CommitReviewAttempt {
            attempt_id,
            actor_device_id,
            committed_at_us,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn create_review_pack(
        &self,
        format_version: u32,
        source: ReviewSource,
        target_count: Option<u32>,
        device_id: String,
        issued_at_us: u64,
    ) -> Result<ReviewPackView, ServiceError> {
        let source = source.validate().map_err(ServiceError::Review)?;
        let target_count = Some(target_count.unwrap_or(100));
        if !target_count.is_some_and(|count| (1..=500).contains(&count)) {
            return Err(ServiceError::Review(super::ReviewError::Target));
        }
        request(&self.sender, |response| StoreCommand::CreateReviewPack {
            format_version,
            source,
            target_count,
            device_id,
            issued_at_us,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn review_pack(
        &self,
        pack_id: String,
        device_id: Option<String>,
    ) -> Result<Option<ReviewPackView>, ServiceError> {
        request(&self.sender, |response| StoreCommand::GetReviewPack {
            pack_id,
            device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn active_review_pack(
        &self,
        device_id: Option<String>,
    ) -> Result<Option<ReviewPackView>, ServiceError> {
        request(&self.sender, |response| StoreCommand::GetActiveReviewPack {
            device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn review_pack_asset(
        &self,
        pack_id: String,
        device_id: Option<String>,
        logical_path: &str,
        digest: &str,
    ) -> Result<Option<(ReviewPackAsset, Vec<u8>)>, ServiceError> {
        let Some(pack) = self.review_pack(pack_id, device_id).await? else {
            return Ok(None);
        };
        let Some(asset) = pack
            .assets
            .into_iter()
            .find(|asset| asset.logical_path == logical_path && asset.content_sha256 == digest)
        else {
            return Ok(None);
        };
        let bytes = self.blobs.read(&asset.content_sha256)?;
        if bytes.len() as u64 != asset.size_bytes {
            return Err(ServiceError::Corrupt);
        }
        Ok(Some((asset, bytes)))
    }

    pub(crate) async fn abandon_review_pack(
        &self,
        pack_id: String,
        device_id: Option<String>,
        ended_at_us: u64,
    ) -> Result<ReviewPackView, ServiceError> {
        request(&self.sender, |response| StoreCommand::AbandonReviewPack {
            pack_id,
            device_id,
            ended_at_us,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn reconcile_offline_operation(
        &self,
        format_version: u32,
        operation: OfflineReviewOperation,
        canonical_payload: String,
        device_id: Option<String>,
        received_at_us: u64,
    ) -> Result<OfflineReviewResult, ServiceError> {
        request(&self.sender, |response| {
            StoreCommand::ReconcileOfflineOperation {
                format_version,
                operation: Box::new(operation),
                canonical_payload,
                device_id,
                received_at_us,
                response,
            }
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn reconcile_content_operation(
        &self,
        format_version: u32,
        operation: OfflineContentOperation,
        canonical_payload: String,
        device_id: Option<String>,
        received_at_us: u64,
    ) -> Result<OfflineContentResult, ServiceError> {
        let prepared = match prepare_offline_document(&operation) {
            Ok(note) => PreparedContentOperation::Ready(note.map(Box::new)),
            Err(reason) => PreparedContentOperation::Conflict(reason),
        };
        request(&self.sender, |response| {
            StoreCommand::ReconcileContentOperation {
                format_version,
                operation: Box::new(operation),
                canonical_payload,
                device_id,
                received_at_us,
                prepared,
                response,
            }
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn reconcile_asset_operation(
        &self,
        format_version: u32,
        operation: OfflineAssetOperation,
        canonical_payload: String,
        device_id: Option<String>,
        received_at_us: u64,
    ) -> Result<OfflineAssetResult, ServiceError> {
        request(&self.sender, |response| {
            StoreCommand::ReconcileAssetOperation {
                format_version,
                operation: Box::new(operation),
                canonical_payload,
                device_id,
                received_at_us,
                response,
            }
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn create_backup(
        &self,
        destination: PathBuf,
        created_at_us: u64,
    ) -> Result<BackupSummary, ServiceError> {
        request(&self.sender, |response| StoreCommand::CreateBackup {
            destination,
            created_at_us,
            blobs: self.blobs.clone(),
            response,
        })
        .await?
        .map_err(ServiceError::Backup)
    }

    pub(crate) async fn verify_backup(&self, path: PathBuf) -> Result<BackupSummary, ServiceError> {
        request(&self.sender, |response| StoreCommand::VerifyBackup {
            path,
            response,
        })
        .await?
        .map_err(ServiceError::Backup)
    }

    pub(crate) async fn prune_backups(
        &self,
        directory: PathBuf,
        keep: u16,
    ) -> Result<Vec<PathBuf>, ServiceError> {
        request(&self.sender, |response| StoreCommand::PruneBackups {
            directory,
            keep,
            response,
        })
        .await?
        .map_err(ServiceError::Backup)
    }

    pub(crate) async fn validate_restore(
        &self,
        backup: PathBuf,
        temporary: PathBuf,
    ) -> Result<BackupSummary, ServiceError> {
        request(&self.sender, |response| StoreCommand::ValidateRestore {
            backup,
            temporary,
            response,
        })
        .await?
        .map_err(ServiceError::Backup)
    }
}

fn read_document_blob(
    blobs: &BlobStore,
    head: Option<DocumentHead>,
) -> Result<Option<(DocumentHead, Vec<u8>)>, ServiceError> {
    let Some(head) = head.filter(|head| head.active) else {
        return Ok(None);
    };
    let digest = head.blob_sha256.as_deref().ok_or(ServiceError::Corrupt)?;
    Ok(Some((head.clone(), blobs.read(digest)?)))
}

fn read_asset_blob(
    blobs: &BlobStore,
    head: Option<AssetHead>,
) -> Result<Option<(AssetHead, Vec<u8>)>, ServiceError> {
    let Some(head) = head.filter(|head| head.active) else {
        return Ok(None);
    };
    let digest = head.blob_sha256.as_deref().ok_or(ServiceError::Corrupt)?;
    Ok(Some((head.clone(), blobs.read(digest)?)))
}

fn read_revision_blob(
    blobs: &BlobStore,
    revision: Option<DocumentRevision>,
) -> Result<Option<(DocumentRevision, Vec<u8>)>, ServiceError> {
    let Some(revision) = revision else {
        return Ok(None);
    };
    let Some(digest) = revision.blob_sha256.as_deref() else {
        return Ok(Some((revision, Vec::new())));
    };
    Ok(Some((revision.clone(), blobs.read(digest)?)))
}

pub(crate) struct ServiceRuntime {
    handle: ServiceHandle,
    join: thread::JoinHandle<()>,
}

impl ServiceRuntime {
    pub(crate) async fn start(
        database_path: PathBuf,
        blob_path: PathBuf,
        capacity: usize,
    ) -> Result<Self, ServiceStartError> {
        tokio::task::spawn_blocking(move || {
            Self::start_blocking(database_path, blob_path, capacity)
        })
        .await
        .map_err(|_| ServiceStartError::Stopped)?
    }

    fn start_blocking(
        database_path: PathBuf,
        blob_path: PathBuf,
        capacity: usize,
    ) -> Result<Self, ServiceStartError> {
        let blobs = Arc::new(BlobStore::open(blob_path).map_err(ServiceStartError::Blob)?);
        let (sender, receiver) = mpsc::sync_channel(capacity);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let actor_blobs = blobs.clone();
        let join = thread::Builder::new()
            .name("gnosis-sqlite".into())
            .spawn(move || store_actor(database_path, actor_blobs, receiver, ready_sender))
            .map_err(ServiceStartError::Thread)?;
        ready_receiver
            .recv()
            .map_err(|_| ServiceStartError::Stopped)?
            .map_err(ServiceStartError::Store)?;
        Ok(Self {
            handle: ServiceHandle { sender, blobs },
            join,
        })
    }

    pub(crate) fn handle(&self) -> ServiceHandle {
        self.handle.clone()
    }

    pub(crate) async fn finish(self) -> Result<(), ServiceError> {
        drop(self.handle);
        tokio::task::spawn_blocking(move || self.join.join())
            .await
            .map_err(|_| ServiceError::Stopped)?
            .map_err(|_| ServiceError::Stopped)
    }
}

type StoreResult<T> = Result<T, StoreError>;

enum PreparedContentOperation {
    Ready(Option<Box<PreparedNote>>),
    Conflict(OfflineContentConflictReason),
}

enum StoreCommand {
    Replace {
        prepared: Box<PreparedServerImport>,
        blob: BlobMeta,
        created_at_us: i64,
        response: oneshot::Sender<Result<ImportSummary, ApplicationError>>,
    },
    Get {
        document_id: DocumentId,
        response: oneshot::Sender<StoreResult<Option<StoredDocument>>>,
    },
    LegacyInfo {
        response: oneshot::Sender<StoreResult<Option<LegacyDatabaseInfo>>>,
    },
    GetLegacyThema {
        thema_id: ThemaId,
        response: oneshot::Sender<StoreResult<Option<LegacyThema>>>,
    },
    DueThemata {
        query: QueryDueThemata,
        response: oneshot::Sender<StoreResult<Vec<DueThema>>>,
    },
    DueThemataPage {
        query: QueryDueThemataPage,
        response: oneshot::Sender<StoreResult<Vec<DueThema>>>,
    },
    ReplaceNote {
        prepared: Box<PreparedNote>,
        blob: crate::blob::BlobMeta,
        created_at_us: i64,
        author_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<DocumentHead>>,
    },
    DocumentHead {
        document_id: DocumentId,
        response: oneshot::Sender<StoreResult<Option<DocumentHead>>>,
    },
    DocumentRevisions {
        document_id: DocumentId,
        after: u64,
        limit: u16,
        response: oneshot::Sender<StoreResult<Vec<DocumentRevision>>>,
    },
    DocumentRevision {
        document_id: DocumentId,
        revision: u64,
        response: oneshot::Sender<StoreResult<Option<DocumentRevision>>>,
    },
    RenameDocument {
        document_id: DocumentId,
        logical_path: String,
        expected_revision: u64,
        updated_at_us: i64,
        author_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<DocumentHead>>,
    },
    TombstoneDocument {
        document_id: DocumentId,
        expected_revision: u64,
        updated_at_us: i64,
        author_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<DocumentHead>>,
    },
    PutAsset {
        prepared: PreparedAsset,
        blob: crate::blob::BlobMeta,
        created_at_us: i64,
        author_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<AssetHead>>,
    },
    StageBlob {
        blob: BlobMeta,
        created_at_us: i64,
        response: oneshot::Sender<StoreResult<()>>,
    },
    AssetHead {
        asset_path: String,
        response: oneshot::Sender<StoreResult<Option<AssetHead>>>,
    },
    TombstoneAsset {
        asset_path: String,
        expected_revision: u64,
        updated_at_us: i64,
        author_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<AssetHead>>,
    },
    ContentChanges {
        after: u64,
        limit: u16,
        response: oneshot::Sender<StoreResult<Vec<ContentChange>>>,
    },
    LatestContentCursor {
        response: oneshot::Sender<StoreResult<u64>>,
    },
    ReplicaState {
        device_id: String,
        response: oneshot::Sender<StoreResult<ReplicaState>>,
    },
    ReplicaChanges {
        server_id: String,
        after: u64,
        after_token: String,
        limit: u16,
        response: oneshot::Sender<StoreResult<(Vec<ReplicaChange>, u64, String)>>,
    },
    CreateReplicaSnapshot {
        device_id: String,
        created_at_us: u64,
        limit: u16,
        response: oneshot::Sender<StoreResult<SnapshotPage>>,
    },
    ReplicaSnapshotPage {
        device_id: String,
        position: SnapshotPosition,
        limit: u16,
        response: oneshot::Sender<StoreResult<SnapshotPage>>,
    },
    #[cfg(test)]
    ExpireReplicaChanges {
        cursor: u64,
        response: oneshot::Sender<StoreResult<()>>,
    },
    BlobMeta {
        digest: String,
        response: oneshot::Sender<StoreResult<Option<BlobMeta>>>,
    },
    GetSettings {
        response: oneshot::Sender<StoreResult<SettingsRecord>>,
    },
    UpdateSettings {
        input: SettingsInput,
        expected_revision: u64,
        updated_at_us: i64,
        author_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<SettingsRecord>>,
    },
    PutThema {
        prepared: Box<PreparedThema>,
        initial_review_day: u32,
        updated_at_us: i64,
        author_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<ThemaRecord>>,
    },
    GetThema {
        thema_id: String,
        response: oneshot::Sender<StoreResult<Option<ThemaRecord>>>,
    },
    ArchiveThema {
        thema_id: String,
        expected_revision: u64,
        archived: bool,
        updated_at_us: i64,
        author_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<ThemaRecord>>,
    },
    SearchThemata {
        query: String,
        limit: u16,
        after: Option<String>,
        response: oneshot::Sender<StoreResult<Vec<ThemaRecord>>>,
    },
    GetNode {
        node_id: String,
        response: oneshot::Sender<StoreResult<Option<NodeRecord>>>,
    },
    SearchNodes {
        query: String,
        limit: u16,
        after: Option<String>,
        response: oneshot::Sender<StoreResult<Vec<NodeRecord>>>,
    },
    GraphNodes {
        node_id: String,
        inbound: bool,
        limit: u16,
        after: Option<String>,
        response: oneshot::Sender<StoreResult<Vec<NodeRecord>>>,
    },
    CreateDevice {
        device: NewDevice,
        created_at_us: i64,
        response: oneshot::Sender<StoreResult<DeviceRecord>>,
    },
    ListDevices {
        response: oneshot::Sender<StoreResult<Vec<DeviceRecord>>>,
    },
    RevokeDevice {
        device_id: String,
        revoked_at_us: i64,
        response: oneshot::Sender<StoreResult<DeviceRecord>>,
    },
    Authenticate {
        digest: String,
        used_at_us: i64,
        response: oneshot::Sender<StoreResult<Option<DevicePrincipal>>>,
    },
    CreateReviewSession {
        source: ReviewSource,
        target_count: Option<u32>,
        owner_device_id: Option<String>,
        started_at_us: u64,
        response: oneshot::Sender<StoreResult<ReviewSessionView>>,
    },
    ActiveReviewSession {
        actor_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<Option<ReviewSessionView>>>,
    },
    ResumeReviewSession {
        session_id: String,
        actor_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<ReviewSessionView>>,
    },
    StopReviewSession {
        session_id: String,
        actor_device_id: Option<String>,
        abort: bool,
        ended_at_us: u64,
        response: oneshot::Sender<StoreResult<ReviewSessionView>>,
    },
    BeginReviewAttempt {
        session_id: String,
        actor_device_id: Option<String>,
        started_at_us: u64,
        response: oneshot::Sender<StoreResult<ReviewAttemptView>>,
    },
    GradeReviewAttempt {
        attempt_id: String,
        actor_device_id: Option<String>,
        answer: AnswerInput,
        answered_at_us: u64,
        fuzz_sample: f64,
        response: oneshot::Sender<StoreResult<ReviewAttemptView>>,
    },
    OverrideReviewAttempt {
        attempt_id: String,
        actor_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<ReviewAttemptView>>,
    },
    OverrideLocalReviewAttempt {
        attempt_id: String,
        request_id: String,
        request_sha256: String,
        operated_at_us: u64,
        response: oneshot::Sender<StoreResult<ReviewAttemptView>>,
    },
    ToggleAttemptSuspension {
        attempt_id: String,
        actor_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<ReviewAttemptView>>,
    },
    ToggleLocalAttemptSuspension {
        attempt_id: String,
        request_id: String,
        request_sha256: String,
        operated_at_us: u64,
        response: oneshot::Sender<StoreResult<ReviewAttemptView>>,
    },
    CommitReviewAttempt {
        attempt_id: String,
        actor_device_id: Option<String>,
        committed_at_us: u64,
        response: oneshot::Sender<StoreResult<String>>,
    },
    CreateReviewPack {
        format_version: u32,
        source: ReviewSource,
        target_count: Option<u32>,
        device_id: String,
        issued_at_us: u64,
        response: oneshot::Sender<StoreResult<ReviewPackView>>,
    },
    GetReviewPack {
        pack_id: String,
        device_id: Option<String>,
        response: oneshot::Sender<StoreResult<Option<ReviewPackView>>>,
    },
    GetActiveReviewPack {
        device_id: Option<String>,
        response: oneshot::Sender<StoreResult<Option<ReviewPackView>>>,
    },
    AbandonReviewPack {
        pack_id: String,
        device_id: Option<String>,
        ended_at_us: u64,
        response: oneshot::Sender<StoreResult<ReviewPackView>>,
    },
    ReconcileOfflineOperation {
        format_version: u32,
        operation: Box<OfflineReviewOperation>,
        canonical_payload: String,
        device_id: Option<String>,
        received_at_us: u64,
        response: oneshot::Sender<StoreResult<OfflineReviewResult>>,
    },
    ReconcileContentOperation {
        format_version: u32,
        operation: Box<OfflineContentOperation>,
        canonical_payload: String,
        device_id: Option<String>,
        received_at_us: u64,
        prepared: PreparedContentOperation,
        response: oneshot::Sender<StoreResult<OfflineContentResult>>,
    },
    ReconcileAssetOperation {
        format_version: u32,
        operation: Box<OfflineAssetOperation>,
        canonical_payload: String,
        device_id: Option<String>,
        received_at_us: u64,
        response: oneshot::Sender<StoreResult<OfflineAssetResult>>,
    },
    CreateBackup {
        destination: PathBuf,
        created_at_us: u64,
        blobs: Arc<BlobStore>,
        response: oneshot::Sender<Result<BackupSummary, BackupError>>,
    },
    VerifyBackup {
        path: PathBuf,
        response: oneshot::Sender<Result<BackupSummary, BackupError>>,
    },
    PruneBackups {
        directory: PathBuf,
        keep: u16,
        response: oneshot::Sender<Result<Vec<PathBuf>, BackupError>>,
    },
    ValidateRestore {
        backup: PathBuf,
        temporary: PathBuf,
        response: oneshot::Sender<Result<BackupSummary, BackupError>>,
    },
}

async fn request<T>(
    sender: &SyncSender<StoreCommand>,
    command: impl FnOnce(oneshot::Sender<T>) -> StoreCommand,
) -> Result<T, ServiceError> {
    let (response_sender, response_receiver) = oneshot::channel();
    enqueue(sender, command(response_sender))?;
    response_receiver.await.map_err(|_| ServiceError::Stopped)
}

fn enqueue(sender: &SyncSender<StoreCommand>, command: StoreCommand) -> Result<(), ServiceError> {
    match sender.try_send(command) {
        Ok(()) => Ok(()),
        Err(TrySendError::Full(_)) => Err(ServiceError::Busy),
        Err(TrySendError::Disconnected(_)) => Err(ServiceError::Stopped),
    }
}

fn store_actor(
    path: PathBuf,
    blobs: Arc<BlobStore>,
    receiver: mpsc::Receiver<StoreCommand>,
    ready: SyncSender<Result<(), StoreError>>,
) {
    let mut store = match SqliteStore::open(path) {
        Ok(store) => store,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };
    let recovery = store
        .clear_staged_blobs()
        .and_then(|()| store.referenced_blobs())
        .and_then(|expected| {
            blobs
                .recover(&expected)
                .map_err(|error| StoreError::Storage(Box::new(error)))
        });
    if let Err(error) = recovery {
        let _ = ready.send(Err(error));
        return;
    }
    if let Err(error) = rebuild_document_asset_index(&mut store, &blobs) {
        let _ = ready.send(Err(error));
        return;
    }
    if ready.send(Ok(())).is_err() {
        return;
    }
    for command in receiver {
        run_store_command(&mut store, &blobs, command);
    }
}

fn rebuild_document_asset_index(
    store: &mut SqliteStore,
    blobs: &BlobStore,
) -> Result<(), StoreError> {
    let mut parsed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut associations = Vec::new();
    for source in store.document_asset_sources()? {
        let asset_paths = match parsed.get(&source.blob_sha256) {
            Some(asset_paths) => asset_paths.clone(),
            None => {
                let bytes = blobs
                    .read(&source.blob_sha256)
                    .map_err(|error| StoreError::Storage(Box::new(error)))?;
                let org = String::from_utf8(bytes).map_err(|_| StoreError::CorruptData {
                    field: "document blob UTF-8",
                })?;
                let asset_paths = retained_asset_paths_in_org(&org);
                parsed.insert(source.blob_sha256.clone(), asset_paths.clone());
                asset_paths
            }
        };
        associations.push(DocumentAssetAssociation {
            document_id: source.document_id,
            revision: source.revision,
            asset_paths,
            active_head: source.active_head,
        });
    }
    store.rebuild_document_assets(&associations)
}

struct ContentCommand<'a> {
    format_version: u32,
    operation: &'a OfflineContentOperation,
    canonical_payload: &'a str,
    device_id: Option<&'a str>,
    received_at_us: u64,
    preparation: Result<Option<&'a PreparedNote>, OfflineContentConflictReason>,
}

fn reconcile_content_command(
    store: &mut SqliteStore,
    blobs: &BlobStore,
    command: ContentCommand<'_>,
) -> StoreResult<OfflineContentResult> {
    let needs_blob = store.content_operation_needs_blob(
        command.format_version,
        command.operation,
        command.canonical_payload,
        command.device_id,
        command.preparation,
    )?;
    let blob = if needs_blob {
        let note = command
            .preparation
            .map_err(|_| StoreError::CorruptData {
                field: "content preparation",
            })?
            .ok_or(StoreError::CorruptData {
                field: "content preparation",
            })?;
        Some(
            blobs
                .put(note.org.as_bytes(), "text/org; charset=utf-8")
                .map_err(|error| StoreError::Storage(Box::new(error)))?,
        )
    } else {
        None
    };
    store.reconcile_content_operation(
        command.format_version,
        command.operation,
        command.canonical_payload,
        command.device_id,
        command.received_at_us,
        command.preparation.map(|note| (note, blob.as_ref())),
    )
}

fn run_store_command(store: &mut SqliteStore, blobs: &BlobStore, command: StoreCommand) {
    match command {
        StoreCommand::Replace {
            prepared,
            blob,
            created_at_us,
            response,
        } => send(
            response,
            persist_server_import(store, &prepared, &blob, created_at_us),
        ),
        StoreCommand::Get {
            document_id,
            response,
        } => send(response, store.get_document(&document_id)),
        StoreCommand::LegacyInfo { response } => send(response, store.legacy_database_info()),
        StoreCommand::GetLegacyThema { thema_id, response } => {
            send(response, store.get_legacy_thema(&thema_id))
        }
        StoreCommand::DueThemata { query, response } => send(
            response,
            store.due_legacy_themata(query.day(), query.limit()),
        ),
        StoreCommand::DueThemataPage { query, response } => run_due_page(store, query, response),
        StoreCommand::ReplaceNote {
            prepared,
            blob,
            created_at_us,
            author_device_id,
            response,
        } => {
            let result = store
                .replace_note(&prepared, &blob, created_at_us, author_device_id.as_deref())
                .and_then(|_| {
                    store
                        .document_head(&prepared.document_id)?
                        .ok_or(StoreError::NotFound)
                });
            send(response, result);
        }
        StoreCommand::DocumentHead {
            document_id,
            response,
        } => send(response, store.document_head(&document_id)),
        StoreCommand::DocumentRevisions {
            document_id,
            after,
            limit,
            response,
        } => send(
            response,
            store.document_revisions(&document_id, after, limit),
        ),
        StoreCommand::DocumentRevision {
            document_id,
            revision,
            response,
        } => send(response, store.document_revision(&document_id, revision)),
        StoreCommand::RenameDocument {
            document_id,
            logical_path,
            expected_revision,
            updated_at_us,
            author_device_id,
            response,
        } => send(
            response,
            store.rename_document(
                &document_id,
                &logical_path,
                expected_revision,
                updated_at_us,
                author_device_id.as_deref(),
            ),
        ),
        StoreCommand::TombstoneDocument {
            document_id,
            expected_revision,
            updated_at_us,
            author_device_id,
            response,
        } => send(
            response,
            store.tombstone_document(
                &document_id,
                expected_revision,
                updated_at_us,
                author_device_id.as_deref(),
            ),
        ),
        StoreCommand::PutAsset {
            prepared,
            blob,
            created_at_us,
            author_device_id,
            response,
        } => {
            let result = store
                .put_asset(&prepared, &blob, created_at_us, author_device_id.as_deref())
                .and_then(|_| {
                    store
                        .asset_head(&prepared.logical_path)?
                        .ok_or(StoreError::NotFound)
                });
            send(response, result);
        }
        StoreCommand::StageBlob {
            blob,
            created_at_us,
            response,
        } => send(response, store.stage_blob(&blob, created_at_us)),
        StoreCommand::AssetHead {
            asset_path,
            response,
        } => send(response, store.asset_head(&asset_path)),
        StoreCommand::TombstoneAsset {
            asset_path,
            expected_revision,
            updated_at_us,
            author_device_id,
            response,
        } => send(
            response,
            store.tombstone_asset(
                &asset_path,
                expected_revision,
                updated_at_us,
                author_device_id.as_deref(),
            ),
        ),
        StoreCommand::ContentChanges {
            after,
            limit,
            response,
        } => send(response, store.content_changes(after, limit)),
        StoreCommand::LatestContentCursor { response } => {
            send(response, store.latest_content_cursor())
        }
        StoreCommand::ReplicaState {
            device_id,
            response,
        } => send(response, store.replica_state(&device_id)),
        StoreCommand::ReplicaChanges {
            server_id,
            after,
            after_token,
            limit,
            response,
        } => send(
            response,
            store.replica_changes(&server_id, after, &after_token, limit),
        ),
        StoreCommand::CreateReplicaSnapshot {
            device_id,
            created_at_us,
            limit,
            response,
        } => send(
            response,
            store.create_replica_snapshot(&device_id, created_at_us, limit),
        ),
        StoreCommand::ReplicaSnapshotPage {
            device_id,
            position,
            limit,
            response,
        } => send(
            response,
            store.replica_snapshot_page(&device_id, &position, limit),
        ),
        #[cfg(test)]
        StoreCommand::ExpireReplicaChanges { cursor, response } => {
            send(response, store.expire_replica_changes_through(cursor))
        }
        StoreCommand::BlobMeta { digest, response } => send(response, store.blob_meta(&digest)),
        StoreCommand::GetSettings { response } => send(response, store.settings()),
        StoreCommand::UpdateSettings {
            input,
            expected_revision,
            updated_at_us,
            author_device_id,
            response,
        } => send(
            response,
            store.update_settings(
                &input,
                expected_revision,
                updated_at_us,
                author_device_id.as_deref(),
            ),
        ),
        StoreCommand::PutThema {
            prepared,
            initial_review_day,
            updated_at_us,
            author_device_id,
            response,
        } => send(
            response,
            store.put_thema(
                &prepared,
                initial_review_day,
                updated_at_us,
                author_device_id.as_deref(),
            ),
        ),
        StoreCommand::GetThema { thema_id, response } => send(response, store.get_thema(&thema_id)),
        StoreCommand::ArchiveThema {
            thema_id,
            expected_revision,
            archived,
            updated_at_us,
            author_device_id,
            response,
        } => send(
            response,
            store.set_thema_archived(
                &thema_id,
                expected_revision,
                archived,
                updated_at_us,
                author_device_id.as_deref(),
            ),
        ),
        StoreCommand::SearchThemata {
            query,
            limit,
            after,
            response,
        } => send(
            response,
            store.search_themata(&query, limit, after.as_deref()),
        ),
        StoreCommand::GetNode { node_id, response } => send(response, store.get_node(&node_id)),
        StoreCommand::SearchNodes {
            query,
            limit,
            after,
            response,
        } => send(
            response,
            store.search_nodes(&query, limit, after.as_deref()),
        ),
        StoreCommand::GraphNodes {
            node_id,
            inbound,
            limit,
            after,
            response,
        } => send(
            response,
            store.graph_nodes(&node_id, inbound, limit, after.as_deref()),
        ),
        StoreCommand::CreateDevice {
            device,
            created_at_us,
            response,
        } => send(response, store.create_device(&device, created_at_us)),
        StoreCommand::ListDevices { response } => send(response, store.devices()),
        StoreCommand::RevokeDevice {
            device_id,
            revoked_at_us,
            response,
        } => send(response, store.revoke_device(&device_id, revoked_at_us)),
        StoreCommand::Authenticate {
            digest,
            used_at_us,
            response,
        } => send(response, store.authenticate(&digest, used_at_us)),
        StoreCommand::CreateReviewSession {
            source,
            target_count,
            owner_device_id,
            started_at_us,
            response,
        } => send(
            response,
            store.create_review_session(
                &source,
                target_count,
                owner_device_id.as_deref(),
                started_at_us,
            ),
        ),
        StoreCommand::ActiveReviewSession {
            actor_device_id,
            response,
        } => send(
            response,
            store.active_review_session(actor_device_id.as_deref()),
        ),
        StoreCommand::ResumeReviewSession {
            session_id,
            actor_device_id,
            response,
        } => send(
            response,
            store.resume_review_session(&session_id, actor_device_id.as_deref()),
        ),
        StoreCommand::StopReviewSession {
            session_id,
            actor_device_id,
            abort,
            ended_at_us,
            response,
        } => send(
            response,
            store.stop_review_session(&session_id, actor_device_id.as_deref(), abort, ended_at_us),
        ),
        StoreCommand::BeginReviewAttempt {
            session_id,
            actor_device_id,
            started_at_us,
            response,
        } => send(
            response,
            store.begin_review_attempt(&session_id, actor_device_id.as_deref(), started_at_us),
        ),
        StoreCommand::GradeReviewAttempt {
            attempt_id,
            actor_device_id,
            answer,
            answered_at_us,
            fuzz_sample,
            response,
        } => send(
            response,
            store.grade_review_attempt(
                &attempt_id,
                actor_device_id.as_deref(),
                &answer,
                answered_at_us,
                fuzz_sample,
            ),
        ),
        StoreCommand::OverrideReviewAttempt {
            attempt_id,
            actor_device_id,
            response,
        } => send(
            response,
            store.override_review_attempt(&attempt_id, actor_device_id.as_deref()),
        ),
        StoreCommand::OverrideLocalReviewAttempt {
            attempt_id,
            request_id,
            request_sha256,
            operated_at_us,
            response,
        } => send(
            response,
            store.override_local_review_attempt(
                &attempt_id,
                None,
                &request_id,
                &request_sha256,
                operated_at_us,
            ),
        ),
        StoreCommand::ToggleAttemptSuspension {
            attempt_id,
            actor_device_id,
            response,
        } => send(
            response,
            store.toggle_attempt_suspension(&attempt_id, actor_device_id.as_deref()),
        ),
        StoreCommand::ToggleLocalAttemptSuspension {
            attempt_id,
            request_id,
            request_sha256,
            operated_at_us,
            response,
        } => send(
            response,
            store.toggle_local_attempt_suspension(
                &attempt_id,
                None,
                &request_id,
                &request_sha256,
                operated_at_us,
            ),
        ),
        StoreCommand::CommitReviewAttempt {
            attempt_id,
            actor_device_id,
            committed_at_us,
            response,
        } => send(
            response,
            store.commit_review_attempt(&attempt_id, actor_device_id.as_deref(), committed_at_us),
        ),
        StoreCommand::CreateReviewPack {
            format_version,
            source,
            target_count,
            device_id,
            issued_at_us,
            response,
        } => send(
            response,
            store.create_review_pack(
                format_version,
                &source,
                target_count,
                &device_id,
                issued_at_us,
            ),
        ),
        StoreCommand::GetReviewPack {
            pack_id,
            device_id,
            response,
        } => send(response, store.review_pack(&pack_id, device_id.as_deref())),
        StoreCommand::GetActiveReviewPack {
            device_id,
            response,
        } => send(response, store.active_review_pack(device_id.as_deref())),
        StoreCommand::AbandonReviewPack {
            pack_id,
            device_id,
            ended_at_us,
            response,
        } => send(
            response,
            store.abandon_review_pack(&pack_id, device_id.as_deref(), ended_at_us),
        ),
        StoreCommand::ReconcileOfflineOperation {
            format_version,
            operation,
            canonical_payload,
            device_id,
            received_at_us,
            response,
        } => send(
            response,
            store.reconcile_offline_operation(
                format_version,
                &operation,
                &canonical_payload,
                device_id.as_deref(),
                received_at_us,
            ),
        ),
        StoreCommand::ReconcileContentOperation {
            format_version,
            operation,
            canonical_payload,
            device_id,
            received_at_us,
            prepared,
            response,
        } => {
            let preparation = match &prepared {
                PreparedContentOperation::Ready(note) => Ok(note.as_deref()),
                PreparedContentOperation::Conflict(reason) => Err(*reason),
            };
            let result = reconcile_content_command(
                store,
                blobs,
                ContentCommand {
                    format_version,
                    operation: &operation,
                    canonical_payload: &canonical_payload,
                    device_id: device_id.as_deref(),
                    received_at_us,
                    preparation,
                },
            );
            send(response, result);
        }
        StoreCommand::ReconcileAssetOperation {
            format_version,
            operation,
            canonical_payload,
            device_id,
            received_at_us,
            response,
        } => {
            let needs_blob = store.asset_operation_needs_blob(
                format_version,
                &operation,
                &canonical_payload,
                device_id.as_deref(),
            );
            let result = needs_blob.and_then(|needs_blob| {
                if needs_blob {
                    match blobs.read(&operation.content_sha256) {
                        Ok(_) => {}
                        Err(BlobError::Io(error))
                            if error.kind() == std::io::ErrorKind::NotFound =>
                        {
                            return Err(StoreError::BlobRequired {
                                digest: operation.content_sha256.clone(),
                            });
                        }
                        Err(error) => return Err(StoreError::Storage(Box::new(error))),
                    }
                }
                store.reconcile_asset_operation(
                    format_version,
                    &operation,
                    &canonical_payload,
                    device_id.as_deref(),
                    received_at_us,
                )
            });
            send(response, result);
        }
        StoreCommand::CreateBackup {
            destination,
            created_at_us,
            blobs,
            response,
        } => send(
            response,
            create_backup(store, &blobs, &destination, created_at_us),
        ),
        StoreCommand::VerifyBackup { path, response } => send(response, verify_backup(&path)),
        StoreCommand::PruneBackups {
            directory,
            keep,
            response,
        } => send(response, prune_backups(&directory, keep)),
        StoreCommand::ValidateRestore {
            backup,
            temporary,
            response,
        } => send(response, validate_restore(&backup, &temporary)),
    }
}

fn run_due_page(
    store: &SqliteStore,
    query: QueryDueThemataPage,
    response: oneshot::Sender<StoreResult<Vec<DueThema>>>,
) {
    let after = query.after().map(|cursor| {
        (
            cursor.reviewed(),
            cursor.next_review_day(),
            cursor.thema_id(),
        )
    });
    send(
        response,
        store.due_legacy_themata_page(query.day(), query.limit() + 1, after),
    );
}

fn send<T>(sender: oneshot::Sender<T>, value: T) {
    let _ = sender.send(value);
}

#[derive(Debug, Error)]
pub(crate) enum ServiceError {
    #[error("store queue is full")]
    Busy,
    #[error("store service stopped")]
    Stopped,
    #[error("stored content is corrupt")]
    Corrupt,
    #[error("asset digest does not match its canonical path")]
    AssetDigest,
    #[error("local review replay identity is invalid")]
    InvalidReviewReplay,
    #[error("blob metadata is invalid")]
    InvalidBlobMetadata,
    #[error("blob operation failed")]
    Blob(#[from] BlobError),
    #[error("store operation failed")]
    Store(#[source] StoreError),
    #[error("application operation failed")]
    Application(#[source] ApplicationError),
    #[error("review request is invalid")]
    Review(#[source] super::ReviewError),
    #[error("settings request is invalid")]
    Settings(#[source] super::SettingsError),
    #[error("backup operation failed")]
    Backup(#[source] BackupError),
}

fn validate_local_review_replay(
    request_id: &str,
    request_sha256: &str,
    operated_at_us: u64,
) -> Result<(), ServiceError> {
    let valid_digest = request_sha256.len() == 64
        && request_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if request_id.is_empty() || request_id.len() > 128 || !valid_digest || operated_at_us == 0 {
        Err(ServiceError::InvalidReviewReplay)
    } else {
        Ok(())
    }
}

fn validate_blob_metadata(bytes: &[u8], media_type: &str) -> Result<(), ServiceError> {
    let valid_media = !media_type.is_empty()
        && media_type.len() <= 255
        && media_type
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        && media_type.trim() == media_type;
    let size = u64::try_from(bytes.len()).map_err(|_| ServiceError::InvalidBlobMetadata)?;
    if size == 0 || size > MAX_OFFLINE_ASSET_BYTES || !valid_media {
        Err(ServiceError::InvalidBlobMetadata)
    } else {
        Ok(())
    }
}

fn random_fuzz_sample() -> Result<f64, ServiceError> {
    let mut bytes = [0_u8; 2];
    getrandom::fill(&mut bytes).map_err(|_| ServiceError::Corrupt)?;
    let value = u16::from_le_bytes(bytes) % 1001;
    Ok(2.0 * (f64::from(value) / 1000.0) - 1.0)
}

fn system_time_us() -> Result<i64, ServiceError> {
    let micros = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ServiceError::Corrupt)?
        .as_micros();
    i64::try_from(micros).map_err(|_| ServiceError::Corrupt)
}

#[derive(Debug, Error)]
pub(crate) enum ServiceStartError {
    #[error("store service stopped during startup")]
    Stopped,
    #[error("store thread could not start")]
    Thread(#[source] std::io::Error),
    #[error("blob store could not start")]
    Blob(#[source] BlobError),
    #[error("database store could not start")]
    Store(#[source] StoreError),
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use sha2::Digest as _;

    use super::{ServiceError, ServiceRuntime};
    use crate::application::{
        AssetReconciliationStatus, ImportDocument, OfflineAssetConflictReason,
        OfflineAssetOperation, ParametersSnapshot, ReplicaProjection, SettingsInput,
        canonical_asset_path, generate_device, prepare_asset, prepare_note, prepare_thema,
        thema_content,
    };
    use crate::blob::BlobStore;
    use crate::protocol::canonical_asset_operation;
    use crate::store::StoreError;

    async fn runtime(directory: &tempfile::TempDir) -> ServiceRuntime {
        ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn backup_verifies_and_materializes_a_temporary_restore() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let note = prepare_note(
            None,
            "notes/test.org".into(),
            "#+TITLE: Test\n".into(),
            None,
        )
        .unwrap();
        service.replace_note(note, 10, None).await.unwrap();
        let backup = directory.path().join("backup");
        let created = service.create_backup(backup.clone(), 20).await.unwrap();
        assert_eq!(created.blob_count, 1);
        assert_eq!(
            service.verify_backup(backup.clone()).await.unwrap(),
            created
        );
        let restore = directory.path().join("restore-check");
        service
            .validate_restore(backup, restore.clone())
            .await
            .unwrap();
        assert!(!restore.exists());
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn rejected_asset_evidence_survives_restart_and_backup() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let blobs = directory.path().join("blobs");
        let runtime = ServiceRuntime::start(database.clone(), blobs.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let device = generate_device("offline".into(), vec!["content:write".into()]).unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let state = service.replica_state(device_id.clone()).await.unwrap();
        let bytes = b"conflicted asset".to_vec();
        let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
        let operation = OfflineAssetOperation {
            operation_id: "conflicted-asset".into(),
            device_id: device_id.clone(),
            device_sequence: 1,
            cursor_server_id: state.server_id,
            cursor: state.cursor,
            cursor_token: state.cursor_token,
            authored_at_us: 2,
            logical_path: canonical_asset_path(&digest, "image.png").unwrap(),
            original_name: "image.png".into(),
            content_sha256: digest.clone(),
            size_bytes: bytes.len() as u64 + 1,
            media_type: "image/png".into(),
        };
        let payload = canonical_asset_operation(&operation).unwrap();
        service
            .stage_blob(&digest, bytes, "application/octet-stream".into(), 3)
            .await
            .unwrap();
        let result = service
            .reconcile_asset_operation(
                1,
                operation.clone(),
                payload.clone(),
                Some(device_id.clone()),
                4,
            )
            .await
            .unwrap();
        assert_eq!(result.status, AssetReconciliationStatus::Conflict);
        assert_eq!(
            result.conflict_reason,
            Some(OfflineAssetConflictReason::InvalidContent)
        );
        assert!(
            service
                .asset(operation.logical_path.clone())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(service.latest_content_cursor().await.unwrap(), 0);
        let backup = service
            .create_backup(directory.path().join("backup"), 5)
            .await
            .unwrap();
        assert_eq!(backup.blob_count, 1);
        drop(service);
        runtime.finish().await.unwrap();

        let runtime = ServiceRuntime::start(database, blobs, 8).await.unwrap();
        let service = runtime.handle();
        let replay = service
            .reconcile_asset_operation(1, operation.clone(), payload, Some(device_id.clone()), 6)
            .await
            .unwrap();
        assert_eq!(replay, result);
        let mut reused = operation;
        reused.content_sha256 = "a".repeat(64);
        reused.logical_path = canonical_asset_path(&reused.content_sha256, "image.png").unwrap();
        let reused_payload = canonical_asset_operation(&reused).unwrap();
        assert!(matches!(
            service
                .reconcile_asset_operation(1, reused, reused_payload, Some(device_id), 7)
                .await,
            Err(ServiceError::Store(StoreError::IdempotencyConflict))
        ));
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn backup_pruning_keeps_the_newest_verified_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let backups = directory.path().join("backups");
        std::fs::create_dir(&backups).unwrap();
        let names = [
            "gnosis-20260713T010000Z",
            "gnosis-20260714T010000Z",
            "gnosis-20260715T010000Z",
        ];
        for (index, name) in names.iter().enumerate() {
            service
                .create_backup(backups.join(name), index as u64)
                .await
                .unwrap();
        }

        let removed = service.prune_backups(backups.clone(), 2).await.unwrap();

        assert_eq!(removed, vec![backups.join(names[0])]);
        assert!(!backups.join(names[0]).exists());
        assert!(backups.join(names[1]).exists());
        assert!(backups.join(names[2]).exists());
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn backup_pruning_removes_old_corruption_and_ignores_unrelated_entries() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let backups = directory.path().join("backups");
        std::fs::create_dir(&backups).unwrap();
        let corrupt = backups.join("gnosis-20260713T010000Z");
        std::fs::create_dir(&corrupt).unwrap();
        std::fs::write(corrupt.join("manifest.json"), b"corrupt").unwrap();
        let unrelated = backups.join("manual-copy");
        std::fs::create_dir(&unrelated).unwrap();
        let newest = backups.join("gnosis-20260715T010000Z");
        service.create_backup(newest.clone(), 10).await.unwrap();

        let removed = service.prune_backups(backups, 1).await.unwrap();

        assert_eq!(removed, vec![corrupt]);
        assert!(newest.exists());
        assert!(unrelated.exists());
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn asset_digest_is_rejected_before_a_content_change() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let path = canonical_asset_path(&"a".repeat(64), "image.png").unwrap();
        let asset = prepare_asset(path, "image.png".into(), None).unwrap();
        let error = service
            .put_asset(asset, b"different".to_vec(), "image/png".into(), 10, None)
            .await
            .unwrap_err();
        assert!(matches!(error, ServiceError::AssetDigest));
        assert_eq!(service.latest_content_cursor().await.unwrap(), 0);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn online_assets_reuse_identical_bytes_across_request_media_types() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let bytes = b"shared bytes".to_vec();
        let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
        for (name, media_type) in [
            ("first.png", "image/png"),
            ("second.jpg", "application/octet-stream"),
        ] {
            let path = canonical_asset_path(&digest, name).unwrap();
            let head = service
                .put_asset(
                    prepare_asset(path.clone(), name.into(), None).unwrap(),
                    bytes.clone(),
                    media_type.into(),
                    10,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(head.logical_path, path);
        }
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn active_document_assets_block_tombstones_until_the_reference_is_removed() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let bytes = b"document image".to_vec();
        let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
        let path = canonical_asset_path(&digest, "image.png").unwrap();
        let asset = service
            .put_asset(
                prepare_asset(path.clone(), "image.png".into(), None).unwrap(),
                bytes,
                "image/png".into(),
                1,
                None,
            )
            .await
            .unwrap();
        let note = prepare_note(
            Some("document".into()),
            "note.org".into(),
            format!("#+title: Linked\n[[file:{path}]]\n"),
            None,
        )
        .unwrap();
        service.replace_note(note, 2, None).await.unwrap();
        assert!(matches!(
            service
                .tombstone_asset(path.clone(), asset.revision, 3, None)
                .await,
            Err(ServiceError::Store(StoreError::InvalidProjection { .. }))
        ));
        let note = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: Unlinked\n".into(),
            Some(1),
        )
        .unwrap();
        service.replace_note(note, 4, None).await.unwrap();
        assert!(
            !service
                .tombstone_asset(path, asset.revision, 5, None)
                .await
                .unwrap()
                .active
        );
        drop(service);
        runtime.finish().await.unwrap();
        let connection =
            rusqlite::Connection::open(directory.path().join("gnosis.sqlite")).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM document_asset", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM document_revision_asset", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn missing_asset_cannot_replace_a_document_head_or_advance_the_feed() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let path = canonical_asset_path(&"a".repeat(64), "missing.png").unwrap();
        let note = prepare_note(
            Some("document".into()),
            "note.org".into(),
            format!("#+title: Missing\n[[file:{path}]]\n"),
            None,
        )
        .unwrap();
        assert!(matches!(
            service.replace_note(note, 1, None).await,
            Err(ServiceError::Store(StoreError::NotFound))
        ));
        assert_eq!(service.latest_content_cursor().await.unwrap(), 0);
        assert!(
            service
                .note(crate::domain::DocumentId::new("document").unwrap())
                .await
                .unwrap()
                .is_none()
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn active_thema_assets_block_tombstones_until_archived() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let bytes = b"thema image".to_vec();
        let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
        let path = canonical_asset_path(&digest, "image.png").unwrap();
        let asset = service
            .put_asset(
                prepare_asset(path.clone(), "image.png".into(), None).unwrap(),
                bytes,
                "image/png".into(),
                1,
                None,
            )
            .await
            .unwrap();
        let content = thema_content(
            "basic",
            "Question".into(),
            vec![],
            vec!["Answer".into()],
            None,
            None,
            Some(path.clone()),
        )
        .unwrap();
        let thema = service
            .put_thema(
                prepare_thema(
                    Some("thema".into()),
                    content,
                    vec![],
                    vec![],
                    vec![path.clone()],
                    None,
                )
                .unwrap(),
                20260715,
                2,
                None,
            )
            .await
            .unwrap();
        assert!(matches!(
            service
                .tombstone_asset(path.clone(), asset.revision, 3, None)
                .await,
            Err(ServiceError::Store(StoreError::InvalidProjection { .. }))
        ));
        let archived = service
            .archive_thema("thema".into(), thema.revision, true, 4, None)
            .await
            .unwrap();
        assert!(
            !service
                .tombstone_asset(path, asset.revision, 5, None)
                .await
                .unwrap()
                .active
        );
        let cursor = service.latest_content_cursor().await.unwrap();
        assert!(matches!(
            service
                .archive_thema("thema".into(), archived.revision, false, 6, None)
                .await,
            Err(ServiceError::Store(StoreError::InvalidProjection { .. }))
        ));
        assert_eq!(service.latest_content_cursor().await.unwrap(), cursor);
        assert!(
            service
                .thema("thema".into())
                .await
                .unwrap()
                .unwrap()
                .archived
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn daemon_upgrade_rebuilds_document_asset_associations() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let blobs = directory.path().join("blobs");
        let runtime = ServiceRuntime::start(database.clone(), blobs.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let bytes = b"restart image".to_vec();
        let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
        let path = canonical_asset_path(&digest, "image.png").unwrap();
        let asset = service
            .put_asset(
                prepare_asset(path.clone(), "image.png".into(), None).unwrap(),
                bytes,
                "image/png".into(),
                1,
                None,
            )
            .await
            .unwrap();
        service
            .replace_note(
                prepare_note(
                    Some("document".into()),
                    "note.org".into(),
                    format!("#+title: Linked\n[[file:{path}]]\n"),
                    None,
                )
                .unwrap(),
                2,
                None,
            )
            .await
            .unwrap();
        drop(service);
        runtime.finish().await.unwrap();
        let legacy_org = format!("#+title: Linked\n[[file:image.png]]\n[[file:{path}]]\n");
        let legacy_blob = BlobStore::open(blobs.clone())
            .unwrap()
            .put(legacy_org.as_bytes(), "text/org; charset=utf-8")
            .unwrap();
        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute(
                "INSERT INTO blob(content_sha256, size_bytes, media_type, created_at_us)
                 VALUES (?1, ?2, ?3, 3)",
                rusqlite::params![
                    legacy_blob.sha256,
                    i64::try_from(legacy_blob.size_bytes).unwrap(),
                    legacy_blob.media_type
                ],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE document SET head_blob_sha256 = ?1, content_sha256 = ?1
                 WHERE document_id = 'document'",
                [&legacy_blob.sha256],
            )
            .unwrap();
        connection
            .execute(
                "UPDATE document_revision SET blob_sha256 = ?1
                 WHERE document_id = 'document' AND revision = 1",
                [&legacy_blob.sha256],
            )
            .unwrap();
        connection
            .execute_batch(
                "DROP TABLE local_review_replay;
                 DROP TABLE offline_asset_evidence;
                 DROP TABLE offline_asset_result;
                 DROP TABLE offline_asset_operation;
                 DROP TABLE staged_blob;
                 DROP TABLE document_revision_asset;
                 DROP TABLE document_asset;
                 DELETE FROM schema_migration WHERE version = 10;
                 DELETE FROM schema_migration WHERE version = 9;
                 DELETE FROM schema_migration WHERE version = 8;
                 PRAGMA user_version = 7;",
            )
            .unwrap();
        drop(connection);

        let runtime = ServiceRuntime::start(database.clone(), blobs, 8)
            .await
            .unwrap();
        let service = runtime.handle();
        assert!(matches!(
            service.tombstone_asset(path, asset.revision, 3, None).await,
            Err(ServiceError::Store(StoreError::InvalidProjection { .. }))
        ));
        drop(service);
        runtime.finish().await.unwrap();
        let connection = rusqlite::Connection::open(database).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM document_asset", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM document_revision_asset", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn unix_imports_enter_the_replica_feed_without_changing_the_response() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let prepared = ImportDocument::new("document", "#+TITLE: Synced\n".into(), None)
            .unwrap()
            .prepare_server()
            .unwrap();
        let summary = service.replace(prepared).await.unwrap();
        assert_eq!(summary.revision, 1);

        let device = generate_device("replica".into(), vec!["read".into()]).unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let snapshot = service
            .create_replica_snapshot(device_id, 2, 10)
            .await
            .unwrap();
        let document = snapshot
            .items
            .iter()
            .find_map(|item| match item {
                ReplicaProjection::Document(value) => Some(value),
                _ => None,
            })
            .unwrap();
        assert_eq!(document.revision.document_id, "document");
        assert_eq!(document.projection.as_ref().unwrap().title, "Synced");
        assert_eq!(service.latest_content_cursor().await.unwrap(), 1);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn tombstoned_assets_can_be_reactivated_without_losing_history() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let bytes = b"image".to_vec();
        let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
        let path = canonical_asset_path(&digest, "image.png").unwrap();
        let first = service
            .put_asset(
                prepare_asset(path.clone(), "image.png".into(), None).unwrap(),
                bytes.clone(),
                "image/png".into(),
                1,
                None,
            )
            .await
            .unwrap();
        let tombstone = service
            .tombstone_asset(path.clone(), first.revision, 2, None)
            .await
            .unwrap();
        let active = service
            .put_asset(
                prepare_asset(path, "image.png".into(), Some(tombstone.revision)).unwrap(),
                bytes,
                "image/png".into(),
                3,
                None,
            )
            .await
            .unwrap();
        assert!(active.active);
        assert_eq!(active.revision, 3);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn unchanged_note_content_reactivates_a_tombstoned_document() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let org = "#+TITLE: Restored\n";
        let first = service
            .replace_note(
                prepare_note(
                    Some("document".into()),
                    "restored.org".into(),
                    org.into(),
                    None,
                )
                .unwrap(),
                1,
                None,
            )
            .await
            .unwrap();
        let tombstone = service
            .tombstone_note(
                crate::domain::DocumentId::new("document").unwrap(),
                first.revision,
                2,
                None,
            )
            .await
            .unwrap();

        let restored = service
            .replace_note(
                prepare_note(
                    Some("document".into()),
                    "restored.org".into(),
                    org.into(),
                    Some(tombstone.revision),
                )
                .unwrap(),
                3,
                None,
            )
            .await
            .unwrap();

        assert!(restored.active);
        assert_eq!(restored.revision, 3);
        assert!(
            service
                .note(crate::domain::DocumentId::new("document").unwrap())
                .await
                .unwrap()
                .is_some()
        );
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn removed_nodes_keep_their_historical_graph_links() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let original = prepare_note(
            Some("document".into()),
            "graph.org".into(),
            "#+TITLE: Graph\n* A\n:PROPERTIES:\n:ID: a\n:END:\n[[id:b]]\n* B\n:PROPERTIES:\n:ID: b\n:END:\n"
                .into(),
            None,
        )
        .unwrap();
        service.replace_note(original, 1, None).await.unwrap();
        let replacement = prepare_note(
            Some("document".into()),
            "graph.org".into(),
            "#+TITLE: Graph\n* A\n:PROPERTIES:\n:ID: a\n:END:\n".into(),
            Some(1),
        )
        .unwrap();
        service.replace_note(replacement, 2, None).await.unwrap();
        let retired = service.node("b".into()).await.unwrap().unwrap();
        assert!(!retired.active);
        let inbound = service
            .graph_nodes("b".into(), true, 10, None)
            .await
            .unwrap();
        assert_eq!(inbound[0].node_id, "a");
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn settings_updates_are_revisioned_and_preserve_rules() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let initial = service.settings().await.unwrap();
        assert_eq!(initial.timezone, "UTC");
        assert!(initial.tag_rules.contains_key("tag-a"));
        let input = SettingsInput {
            timezone: "Europe/Sofia".into(),
            day_boundary_hour: 4,
            scheduler: ParametersSnapshot {
                scheduler_version: 1,
                amnesia: 0.5,
                epignosis: 0.1,
                agnoia: 0.2,
                anagnosis: 3,
                lethe: 2,
                synolon_max: 3.0,
                interval_fuzz: 0.1,
                proto: vec![0, 1, 2],
            },
            tag_rules: BTreeMap::new(),
        };
        let updated = service.update_settings(input, 1, 10, None).await.unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.timezone, "Europe/Sofia");
        assert_eq!(service.latest_content_cursor().await.unwrap(), 1);
        drop(service);
        runtime.finish().await.unwrap();
    }
}
