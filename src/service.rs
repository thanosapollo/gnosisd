use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, SyncSender, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

#[cfg(test)]
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::oneshot;

mod backup;
mod import;

pub(crate) use backup::BackupSummary;
use backup::{
    BackupCancellation, BackupError, create_backup_cancellable, ensure_restore_complete,
    prune_backups, restore_backup, snapshot_backup_cancellable, validate_restore_cancellable,
    verify_backup_cancellable,
};

pub(crate) const SERVICE_SHUTDOWN_DEADLINE: Duration = Duration::from_secs(5);
const SERVICE_SHUTDOWN_POLL: Duration = Duration::from_millis(10);
#[cfg(test)]
use import::persist_server_import;
pub(crate) use import::protocol_logical_path;

#[cfg(test)]
use crate::application::{ImportSummary, PreparedServerImport};

use crate::{
    application::{
        AnswerInput, ApplicationError, AssetHead, ContentChange,
        DEFAULT_ONLINE_REVIEW_TARGET_COUNT, DashboardSummary, DeviceDesiredStateMutation,
        DeviceMutationResult, DevicePrincipal, DeviceRecord, DocumentHead, DocumentRevision,
        MAX_ONLINE_REVIEW_TARGET_COUNT, NewDevice, NodeCatalogPage, NodeRecord,
        NodeThemaAssociation, OfflineAssetOperation, OfflineAssetResult,
        OfflineContentConflictReason, OfflineContentOperation, OfflineContentResult,
        OfflineReviewOperation, OfflineReviewResult, PreparedAsset,
        PreparedDeviceDesiredStateMutation, PreparedDeviceDocumentLifecycleMutation,
        PreparedDeviceDocumentMutation, PreparedNote, PreparedThema, QueryDueThemataPage,
        ReplicaChange, ReplicaState, ReviewAttemptBeginResult, ReviewAttemptOperation,
        ReviewAttemptOperationResult, ReviewAttemptState, ReviewAttemptView, ReviewClozeCheck, ReviewError,
        ReviewHistoryEvent, ReviewHistoryQuery, ReviewOperationAuthority, ReviewPackAsset,
        ReviewPackView, ReviewQueueSummary, ReviewSessionQuery, ReviewSessionSummary,
        ReviewSessionView, ReviewSource, SettingsError, SettingsInput, SettingsRecord,
        SnapshotPage, SnapshotPosition, ThemaCatalogPage, ThemaRecord, ThemaReviewState,
        prepare_offline_document, retained_asset_paths, token_digest, validate_asset_path,
        validate_blob_metadata,
    },
    blob::{BlobError, BlobMeta, BlobStore},
    domain::{DocumentId, DueThema},
    protocol::MAX_REPAIR_BATCH_SIZE,
    store::{
        DeviceMutationExecution, DocumentAssetAssociation, DocumentAssetSource, SqliteStore,
        StoreError, StoredDocument,
    },
};

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RepairSummary {
    pub(crate) document_revisions_repaired: u64,
    pub(crate) blobs_verified: u64,
    pub(crate) max_batch_observed: u16,
}

pub(crate) fn cold_restore(
    backup: &Path,
    database: &Path,
    blob_root: &Path,
) -> Result<PathBuf, BackupError> {
    restore_backup(backup, database, blob_root)
}

pub(crate) fn ensure_cold_restore_complete(
    database: &Path,
    blob_root: &Path,
) -> Result<(), BackupError> {
    ensure_restore_complete(database, blob_root)
}

#[derive(Clone)]
pub(crate) struct ServiceHandle {
    sender: SyncSender<StoreCommand>,
    backup_sender: SyncSender<BackupCommand>,
    blobs: Arc<BlobStore>,
}

enum BackupCommand {
    Shutdown,
    #[cfg(test)]
    Block {
        started: SyncSender<()>,
        release: mpsc::Receiver<()>,
    },
    Create {
        source: PathBuf,
        destination: PathBuf,
        created_at_us: u64,
        response: oneshot::Sender<Result<BackupSummary, ServiceError>>,
    },
    Verify {
        path: PathBuf,
        response: oneshot::Sender<Result<BackupSummary, ServiceError>>,
    },
    Prune {
        directory: PathBuf,
        keep: u16,
        response: oneshot::Sender<Result<Vec<PathBuf>, ServiceError>>,
    },
    ValidateRestore {
        path: PathBuf,
        temporary: PathBuf,
        response: oneshot::Sender<Result<BackupSummary, ServiceError>>,
    },
}

#[allow(dead_code)]
impl ServiceHandle {
    pub(crate) async fn ready(&self) -> Result<(), ServiceError> {
        request(&self.sender, |response| StoreCommand::Ready { response })
            .await?
            .map_err(ServiceError::Store)
    }

    pub(crate) async fn repair(&self, batch_size: u16) -> Result<RepairSummary, ServiceError> {
        if batch_size == 0 || batch_size > MAX_REPAIR_BATCH_SIZE {
            return Err(ServiceError::InvalidRepairBatchSize);
        }
        request(&self.sender, |response| StoreCommand::Repair {
            batch_size,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    #[cfg(test)]
    pub(crate) fn stopped_for_test(blob_path: PathBuf) -> Result<Self, BlobError> {
        let (sender, receiver) = mpsc::sync_channel(1);
        let (backup_sender, backup_receiver) = mpsc::sync_channel(1);
        drop(receiver);
        drop(backup_receiver);
        Ok(Self {
            sender,
            backup_sender,
            blobs: Arc::new(BlobStore::open(blob_path)?),
        })
    }

    #[cfg(test)]
    pub(crate) async fn replace(
        &self,
        prepared: PreparedServerImport,
    ) -> Result<ImportSummary, ServiceError> {
        let created_at_us = system_time_us()?;
        request(&self.sender, |response| StoreCommand::Replace {
            prepared: Box::new(prepared),
            created_at_us,
            response,
        })
        .await?
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
        request(&self.sender, |response| StoreCommand::ReplaceNote {
            prepared: Box::new(prepared),
            created_at_us,
            author_device_id,
            response,
        })
        .await?
    }

    pub(crate) async fn apply_device_document_mutation(
        &self,
        operation: PreparedDeviceDocumentMutation,
        created_at_us: i64,
    ) -> Result<DeviceMutationExecution, ServiceError> {
        request(&self.sender, |response| {
            StoreCommand::ApplyDeviceDocumentMutation {
                operation: Box::new(operation),
                created_at_us,
                response,
            }
        })
        .await?
    }

    pub(crate) async fn apply_device_document_lifecycle_mutation(
        &self,
        operation: PreparedDeviceDocumentLifecycleMutation,
        created_at_us: i64,
    ) -> Result<DeviceMutationExecution, ServiceError> {
        request(&self.sender, |response| {
            StoreCommand::ApplyDeviceDocumentLifecycleMutation {
                operation: Box::new(operation),
                created_at_us,
                response,
            }
        })
        .await?
    }

    pub(crate) async fn apply_device_desired_state_mutation(
        &self,
        operation: PreparedDeviceDesiredStateMutation,
        asset_bytes: Option<Vec<u8>>,
        created_at_us: i64,
    ) -> Result<DeviceMutationExecution, ServiceError> {
        request(&self.sender, |response| {
            StoreCommand::ApplyDeviceDesiredStateMutation {
                operation: Box::new(operation),
                asset_bytes,
                created_at_us,
                response,
            }
        })
        .await?
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
        let Some(head) = head.filter(|head| head.active) else {
            return Ok(None);
        };
        let digest = head.blob_sha256.clone().ok_or(ServiceError::Corrupt)?;
        let bytes = read_blob(self.blobs.clone(), digest).await?;
        Ok(Some((head, bytes)))
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
        let Some(record) = record else {
            return Ok(None);
        };
        let Some(digest) = record.blob_sha256.clone() else {
            return Ok(Some((record, Vec::new())));
        };
        let bytes = read_blob(self.blobs.clone(), digest).await?;
        Ok(Some((record, bytes)))
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
        request(&self.sender, |response| StoreCommand::PutAsset {
            prepared,
            bytes,
            media_type,
            created_at_us,
            author_device_id,
            response,
        })
        .await?
    }

    pub(crate) async fn stage_blob(
        &self,
        expected_digest: &str,
        bytes: Vec<u8>,
        media_type: String,
        created_at_us: i64,
    ) -> Result<BlobMeta, ServiceError> {
        let size_bytes =
            u64::try_from(bytes.len()).map_err(|_| ServiceError::InvalidBlobMetadata)?;
        validate_blob_metadata(size_bytes, &media_type)
            .map_err(|_| ServiceError::InvalidBlobMetadata)?;
        let actual_digest = format!("{:x}", Sha256::digest(&bytes));
        if actual_digest != expected_digest {
            return Err(ServiceError::AssetDigest);
        }
        request(&self.sender, |response| StoreCommand::StageBlob {
            bytes,
            media_type,
            created_at_us,
            response,
        })
        .await?
    }

    pub(crate) async fn asset(
        &self,
        asset_path: String,
    ) -> Result<Option<(AssetHead, Vec<u8>)>, ServiceError> {
        validate_asset_path(&asset_path)
            .map_err(ApplicationError::InvalidContent)
            .map_err(ServiceError::Application)?;
        let expected_digest = asset_path
            .split('/')
            .nth(1)
            .ok_or(ServiceError::Corrupt)?
            .to_owned();
        let head = request(&self.sender, |response| StoreCommand::AssetHead {
            asset_path,
            response,
        })
        .await?
        .map_err(ServiceError::Store)?;
        let Some(head) = head.filter(|head| head.active) else {
            return Ok(None);
        };
        let digest = head.blob_sha256.clone().ok_or(ServiceError::Corrupt)?;
        if digest != expected_digest {
            return Err(ServiceError::Corrupt);
        }
        let bytes = read_blob(self.blobs.clone(), digest).await?;
        Ok(Some((head, bytes)))
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
        let digest = meta.sha256.clone();
        let bytes = read_blob(self.blobs.clone(), digest).await?;
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

    pub(crate) async fn node_themata(
        &self,
        node_id: String,
        limit: u16,
        after: Option<String>,
    ) -> Result<Vec<NodeThemaAssociation>, ServiceError> {
        request(&self.sender, |response| StoreCommand::NodeThemata {
            node_id,
            limit,
            after,
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

    pub(crate) async fn node_catalog(
        &self,
        device_id: String,
        document_id: Option<String>,
        limit: u16,
        after: Option<String>,
    ) -> Result<NodeCatalogPage, ServiceError> {
        request(&self.sender, |response| StoreCommand::NodeCatalog {
            device_id,
            document_id,
            limit,
            after,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn thema_catalog(
        &self,
        device_id: String,
        query: String,
        limit: u16,
        after: Option<String>,
    ) -> Result<ThemaCatalogPage, ServiceError> {
        request(&self.sender, |response| StoreCommand::ThemaCatalog {
            device_id,
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

    pub(crate) async fn dashboard(
        &self,
        days: u16,
        now_us: u64,
    ) -> Result<DashboardSummary, ServiceError> {
        request(&self.sender, |response| StoreCommand::Dashboard {
            days,
            now_us,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn review_sessions(
        &self,
        query: ReviewSessionQuery,
    ) -> Result<Vec<ReviewSessionSummary>, ServiceError> {
        request(&self.sender, |response| StoreCommand::ReviewSessions {
            query,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn review_session_summary(
        &self,
        session_id: String,
    ) -> Result<Option<ReviewSessionSummary>, ServiceError> {
        request(&self.sender, |response| {
            StoreCommand::ReviewSessionSummary {
                session_id,
                response,
            }
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn review_history(
        &self,
        query: ReviewHistoryQuery,
    ) -> Result<Vec<ReviewHistoryEvent>, ServiceError> {
        request(&self.sender, |response| StoreCommand::ReviewHistory {
            query,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn review_queue_summary(
        &self,
        source: ReviewSource,
        now_us: u64,
    ) -> Result<ReviewQueueSummary, ServiceError> {
        let source = source.validate().map_err(ServiceError::Review)?;
        request(&self.sender, |response| StoreCommand::ReviewQueueSummary {
            source,
            now_us,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn check_cloze_attempt(
        &self,
        attempt_id: String,
        actor_device_id: Option<String>,
        inputs: Vec<String>,
    ) -> Result<ReviewClozeCheck, ServiceError> {
        request(&self.sender, |response| StoreCommand::CheckClozeAttempt {
            attempt_id,
            actor_device_id,
            inputs,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn thema_review_state(
        &self,
        thema_id: String,
    ) -> Result<ThemaReviewState, ServiceError> {
        request(&self.sender, |response| StoreCommand::ThemaReviewState {
            thema_id,
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
        let target_count = Some(target_count.unwrap_or(DEFAULT_ONLINE_REVIEW_TARGET_COUNT));
        if !target_count.is_some_and(|count| (1..=MAX_ONLINE_REVIEW_TARGET_COUNT).contains(&count))
        {
            return Err(ServiceError::Review(ReviewError::Target));
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

    pub(crate) async fn begin_http_review_attempt(
        &self,
        session_id: String,
        actor_device_id: Option<String>,
        started_at_us: u64,
    ) -> Result<ReviewAttemptBeginResult, ServiceError> {
        request(&self.sender, |response| {
            StoreCommand::BeginHttpReviewAttempt {
                session_id,
                actor_device_id,
                started_at_us,
                response,
            }
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
        request(&self.sender, |response| StoreCommand::GradeReviewAttempt {
            attempt_id,
            actor_device_id,
            answer,
            answered_at_us,
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

    pub(crate) async fn review_attempt_state(
        &self,
        attempt_id: String,
        actor_device_id: Option<String>,
    ) -> Result<ReviewAttemptState, ServiceError> {
        request(&self.sender, |response| StoreCommand::ReviewAttemptState {
            attempt_id,
            actor_device_id,
            response,
        })
        .await?
        .map_err(ServiceError::Store)
    }

    pub(crate) async fn apply_review_attempt_operation(
        &self,
        operation: ReviewAttemptOperation,
        authority: ReviewOperationAuthority,
        operated_at_us: u64,
    ) -> Result<ReviewAttemptOperationResult, ServiceError> {
        request(&self.sender, |response| {
            StoreCommand::ApplyReviewAttemptOperation {
                operation,
                authority,
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
            return Err(ServiceError::Review(ReviewError::Target));
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
        let bytes = read_blob(self.blobs.clone(), asset.content_sha256.clone()).await?;
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
        if format_version != 1 {
            return Err(ServiceError::Store(StoreError::InvalidReviewState));
        }
        let (operation, prepared) = tokio::task::spawn_blocking(move || {
            let prepared = match prepare_offline_document(&operation) {
                Ok(note) => PreparedContentOperation::Ready(note.map(Box::new)),
                Err(reason) => PreparedContentOperation::Conflict(reason),
            };
            (operation, prepared)
        })
        .await
        .map_err(|_| ServiceError::Stopped)?;
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
            response,
        })
        .await?
    }

    pub(crate) async fn verify_backup(&self, path: PathBuf) -> Result<BackupSummary, ServiceError> {
        backup_request(&self.backup_sender, |response| BackupCommand::Verify {
            path,
            response,
        })
        .await
    }

    pub(crate) async fn prune_backups(
        &self,
        directory: PathBuf,
        keep: u16,
    ) -> Result<Vec<PathBuf>, ServiceError> {
        backup_request(&self.backup_sender, |response| BackupCommand::Prune {
            directory,
            keep,
            response,
        })
        .await
    }

    pub(crate) async fn validate_restore(
        &self,
        backup: PathBuf,
        temporary: PathBuf,
    ) -> Result<BackupSummary, ServiceError> {
        backup_request(&self.backup_sender, |response| {
            BackupCommand::ValidateRestore {
                path: backup,
                temporary,
                response,
            }
        })
        .await
    }
}

async fn read_blob(blobs: Arc<BlobStore>, digest: String) -> Result<Vec<u8>, ServiceError> {
    tokio::task::spawn_blocking(move || blobs.read(&digest))
        .await
        .map_err(|_| ServiceError::Stopped)?
        .map_err(ServiceError::Blob)
}

pub(crate) struct ServiceRuntime {
    handle: ServiceHandle,
    store_thread: thread::JoinHandle<()>,
    backup_thread: thread::JoinHandle<()>,
    backup_cancelled: Arc<BackupCancellation>,
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
        let (backup_sender, backup_receiver) = mpsc::sync_channel(capacity);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(1);
        let actor_blobs = blobs.clone();
        let backup_blobs = blobs.clone();
        let backup_cancelled = Arc::new(BackupCancellation::new());
        let actor_backup_cancelled = backup_cancelled.clone();
        let backup_thread = thread::Builder::new()
            .name("gnosis-backup".into())
            .spawn(move || backup_actor(backup_blobs, backup_receiver, actor_backup_cancelled))
            .map_err(ServiceStartError::Thread)?;
        let actor_backup_sender = backup_sender.clone();
        let store_thread = thread::Builder::new()
            .name("gnosis-sqlite".into())
            .spawn(move || {
                store_actor(
                    database_path,
                    actor_blobs,
                    actor_backup_sender,
                    receiver,
                    ready_sender,
                )
            })
            .map_err(ServiceStartError::Thread)?;
        ready_receiver
            .recv()
            .map_err(|_| ServiceStartError::Stopped)?
            .map_err(ServiceStartError::Store)?;
        Ok(Self {
            handle: ServiceHandle {
                sender,
                backup_sender,
                blobs,
            },
            store_thread,
            backup_thread,
            backup_cancelled,
        })
    }

    pub(crate) fn handle(&self) -> ServiceHandle {
        self.handle.clone()
    }

    #[cfg(test)]
    pub(crate) async fn finish(self) -> Result<(), ServiceError> {
        self.finish_with_deadline(SERVICE_SHUTDOWN_DEADLINE).await
    }

    pub(crate) fn request_shutdown(&self) {
        self.backup_cancelled.request();
    }

    pub(crate) async fn finish_with_deadline(self, deadline: Duration) -> Result<(), ServiceError> {
        let Self {
            handle,
            store_thread,
            backup_thread,
            backup_cancelled,
        } = self;
        let started = Instant::now();
        backup_cancelled.request();
        while !backup_cancelled.publication_idle() {
            sleep_until_next_shutdown_poll(started, deadline).await?;
        }
        complete_shutdown_phase(started, deadline)?;
        let (response, receiver) = mpsc::sync_channel(0);
        let mut command = StoreCommand::Shutdown { response };
        loop {
            match handle.sender.try_send(command) {
                Ok(()) => {
                    complete_shutdown_phase(started, deadline)?;
                    break;
                }
                Err(TrySendError::Full(returned)) => {
                    command = returned;
                    sleep_until_next_shutdown_poll(started, deadline).await?;
                }
                Err(TrySendError::Disconnected(_)) => return Err(ServiceError::Stopped),
            }
        }
        loop {
            match receiver.try_recv() {
                Ok(()) => {
                    complete_shutdown_phase(started, deadline)?;
                    break;
                }
                Err(mpsc::TryRecvError::Empty) => {
                    sleep_until_next_shutdown_poll(started, deadline).await?;
                }
                Err(mpsc::TryRecvError::Disconnected) => return Err(ServiceError::Stopped),
            }
        }
        let mut backup_command = BackupCommand::Shutdown;
        loop {
            match handle.backup_sender.try_send(backup_command) {
                Ok(()) => {
                    complete_shutdown_phase(started, deadline)?;
                    break;
                }
                Err(TrySendError::Full(returned)) => {
                    backup_command = returned;
                    sleep_until_next_shutdown_poll(started, deadline).await?;
                }
                Err(TrySendError::Disconnected(_)) => return Err(ServiceError::Stopped),
            }
        }
        drop(handle);
        while !store_thread.is_finished() || !backup_thread.is_finished() {
            sleep_until_next_shutdown_poll(started, deadline).await?;
        }
        complete_shutdown_phase(started, deadline)?;
        store_thread.join().map_err(|_| ServiceError::Stopped)?;
        backup_thread.join().map_err(|_| ServiceError::Stopped)?;
        complete_shutdown_phase(started, deadline)
    }
}

fn shutdown_remaining(started: Instant, deadline: Duration) -> Result<Duration, ServiceError> {
    deadline
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(ServiceError::ShutdownDeadlineExceeded)
}

fn complete_shutdown_phase(started: Instant, deadline: Duration) -> Result<(), ServiceError> {
    shutdown_remaining(started, deadline).map(drop)
}

async fn sleep_until_next_shutdown_poll(
    started: Instant,
    deadline: Duration,
) -> Result<(), ServiceError> {
    let remaining = shutdown_remaining(started, deadline)?;
    tokio::time::sleep(SERVICE_SHUTDOWN_POLL.min(remaining)).await;
    complete_shutdown_phase(started, deadline)
}
type StoreResult<T> = Result<T, StoreError>;

enum PreparedContentOperation {
    Ready(Option<Box<PreparedNote>>),
    Conflict(OfflineContentConflictReason),
}

#[allow(dead_code)]
enum StoreCommand {
    Shutdown {
        response: SyncSender<()>,
    },
    Ready {
        response: oneshot::Sender<StoreResult<()>>,
    },
    Repair {
        batch_size: u16,
        response: oneshot::Sender<StoreResult<RepairSummary>>,
    },
    #[cfg(test)]
    Replace {
        prepared: Box<PreparedServerImport>,
        created_at_us: i64,
        response: oneshot::Sender<Result<ImportSummary, ServiceError>>,
    },
    Get {
        document_id: DocumentId,
        response: oneshot::Sender<StoreResult<Option<StoredDocument>>>,
    },

    DueThemataPage {
        query: QueryDueThemataPage,
        response: oneshot::Sender<StoreResult<Vec<DueThema>>>,
    },
    ReplaceNote {
        prepared: Box<PreparedNote>,
        created_at_us: i64,
        author_device_id: Option<String>,
        response: oneshot::Sender<Result<DocumentHead, ServiceError>>,
    },
    ApplyDeviceDocumentMutation {
        operation: Box<PreparedDeviceDocumentMutation>,
        created_at_us: i64,
        response: oneshot::Sender<Result<DeviceMutationExecution, ServiceError>>,
    },
    ApplyDeviceDocumentLifecycleMutation {
        operation: Box<PreparedDeviceDocumentLifecycleMutation>,
        created_at_us: i64,
        response: oneshot::Sender<Result<DeviceMutationExecution, ServiceError>>,
    },
    ApplyDeviceDesiredStateMutation {
        operation: Box<PreparedDeviceDesiredStateMutation>,
        asset_bytes: Option<Vec<u8>>,
        created_at_us: i64,
        response: oneshot::Sender<Result<DeviceMutationExecution, ServiceError>>,
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
        bytes: Vec<u8>,
        media_type: String,
        created_at_us: i64,
        author_device_id: Option<String>,
        response: oneshot::Sender<Result<AssetHead, ServiceError>>,
    },
    StageBlob {
        bytes: Vec<u8>,
        media_type: String,
        created_at_us: i64,
        response: oneshot::Sender<Result<BlobMeta, ServiceError>>,
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
    NodeThemata {
        node_id: String,
        limit: u16,
        after: Option<String>,
        response: oneshot::Sender<StoreResult<Vec<NodeThemaAssociation>>>,
    },
    SearchNodes {
        query: String,
        limit: u16,
        after: Option<String>,
        response: oneshot::Sender<StoreResult<Vec<NodeRecord>>>,
    },
    NodeCatalog {
        device_id: String,
        document_id: Option<String>,
        limit: u16,
        after: Option<String>,
        response: oneshot::Sender<StoreResult<NodeCatalogPage>>,
    },
    ThemaCatalog {
        device_id: String,
        query: String,
        limit: u16,
        after: Option<String>,
        response: oneshot::Sender<StoreResult<ThemaCatalogPage>>,
    },
    GraphNodes {
        node_id: String,
        inbound: bool,
        limit: u16,
        after: Option<String>,
        response: oneshot::Sender<StoreResult<Vec<NodeRecord>>>,
    },
    Dashboard {
        days: u16,
        now_us: u64,
        response: oneshot::Sender<StoreResult<DashboardSummary>>,
    },
    ReviewSessions {
        query: ReviewSessionQuery,
        response: oneshot::Sender<StoreResult<Vec<ReviewSessionSummary>>>,
    },
    ReviewSessionSummary {
        session_id: String,
        response: oneshot::Sender<StoreResult<Option<ReviewSessionSummary>>>,
    },
    ReviewHistory {
        query: ReviewHistoryQuery,
        response: oneshot::Sender<StoreResult<Vec<ReviewHistoryEvent>>>,
    },
    ReviewQueueSummary {
        source: ReviewSource,
        now_us: u64,
        response: oneshot::Sender<StoreResult<ReviewQueueSummary>>,
    },
    ThemaReviewState {
        thema_id: String,
        response: oneshot::Sender<StoreResult<ThemaReviewState>>,
    },
    CheckClozeAttempt {
        attempt_id: String,
        actor_device_id: Option<String>,
        inputs: Vec<String>,
        response: oneshot::Sender<StoreResult<ReviewClozeCheck>>,
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
    BeginHttpReviewAttempt {
        session_id: String,
        actor_device_id: Option<String>,
        started_at_us: u64,
        response: oneshot::Sender<StoreResult<ReviewAttemptBeginResult>>,
    },
    GradeReviewAttempt {
        attempt_id: String,
        actor_device_id: Option<String>,
        answer: AnswerInput,
        answered_at_us: u64,
        response: oneshot::Sender<StoreResult<ReviewAttemptView>>,
    },
    OverrideReviewAttempt {
        attempt_id: String,
        actor_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<ReviewAttemptView>>,
    },
    ToggleAttemptSuspension {
        attempt_id: String,
        actor_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<ReviewAttemptView>>,
    },
    ReviewAttemptState {
        attempt_id: String,
        actor_device_id: Option<String>,
        response: oneshot::Sender<StoreResult<ReviewAttemptState>>,
    },
    ApplyReviewAttemptOperation {
        operation: ReviewAttemptOperation,
        authority: ReviewOperationAuthority,
        operated_at_us: u64,
        response: oneshot::Sender<StoreResult<ReviewAttemptOperationResult>>,
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
        response: oneshot::Sender<Result<BackupSummary, ServiceError>>,
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

async fn backup_request<T>(
    sender: &SyncSender<BackupCommand>,
    command: impl FnOnce(oneshot::Sender<Result<T, ServiceError>>) -> BackupCommand,
) -> Result<T, ServiceError> {
    let (response_sender, response_receiver) = oneshot::channel();
    match sender.try_send(command(response_sender)) {
        Ok(()) => response_receiver.await.map_err(|_| ServiceError::Stopped)?,
        Err(TrySendError::Full(_)) => Err(ServiceError::Busy),
        Err(TrySendError::Disconnected(_)) => Err(ServiceError::Stopped),
    }
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
    backup_sender: SyncSender<BackupCommand>,
    receiver: mpsc::Receiver<StoreCommand>,
    ready: SyncSender<Result<(), StoreError>>,
) {
    let mut store = match SqliteStore::open(&path) {
        Ok(store) => store,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    if ready.send(Ok(())).is_err() {
        return;
    }
    for command in receiver {
        match command {
            StoreCommand::Shutdown { response } => {
                let _ = response.send(());
                break;
            }
            command => run_store_command(&mut store, &path, &blobs, &backup_sender, command),
        }
    }
}

fn repair_canonical_state(
    store: &mut SqliteStore,
    blobs: &BlobStore,
    batch_size: u16,
) -> Result<RepairSummary, StoreError> {
    let mut summary = RepairSummary::default();
    let mut head_cursor = None;
    loop {
        let sources = store.document_asset_heads_page(head_cursor.as_deref(), batch_size)?;
        summary.max_batch_observed = summary
            .max_batch_observed
            .max(u16::try_from(sources.len()).unwrap_or(u16::MAX));
        if sources.is_empty() {
            break;
        }
        let associations = document_asset_associations(&sources, blobs)?;
        store.repair_document_assets(&associations)?;
        head_cursor = sources.last().map(|source| source.document_id.clone());
    }

    let mut document_cursor: Option<(String, u64)> = None;
    loop {
        let sources = store.document_asset_sources_page(
            document_cursor
                .as_ref()
                .map(|(document_id, revision)| (document_id.as_str(), *revision)),
            batch_size,
        )?;
        summary.max_batch_observed = summary
            .max_batch_observed
            .max(u16::try_from(sources.len()).unwrap_or(u16::MAX));
        if sources.is_empty() {
            break;
        }
        let associations = document_asset_associations(&sources, blobs)?;
        store.repair_document_assets(&associations)?;
        summary.document_revisions_repaired += sources.len() as u64;
        document_cursor = sources
            .last()
            .map(|source| (source.document_id.clone(), source.revision));
    }

    let mut blob_cursor = None;
    loop {
        let page = store.blobs_page(blob_cursor.as_deref(), batch_size)?;
        summary.max_batch_observed = summary
            .max_batch_observed
            .max(u16::try_from(page.len()).unwrap_or(u16::MAX));
        if page.is_empty() {
            break;
        }
        for (digest, size) in &page {
            blobs
                .verify(digest, *size)
                .map_err(|error| StoreError::Storage(Box::new(error)))?;
        }
        summary.blobs_verified += page.len() as u64;
        blob_cursor = page.last().map(|(digest, _)| digest.clone());
    }

    let mut staged_cursor = None;
    loop {
        let page = store.staged_blobs_page(staged_cursor.as_deref(), batch_size)?;
        summary.max_batch_observed = summary
            .max_batch_observed
            .max(u16::try_from(page.len()).unwrap_or(u16::MAX));
        if page.is_empty() {
            break;
        }
        for (digest, size) in &page {
            blobs
                .verify(digest, *size)
                .map_err(|error| StoreError::Storage(Box::new(error)))?;
        }
        summary.blobs_verified += page.len() as u64;
        staged_cursor = page.last().map(|(digest, _)| digest.clone());
    }

    let mut evidence_cursor = None;
    loop {
        let page = store.offline_asset_evidence_page(evidence_cursor.as_deref(), batch_size)?;
        summary.max_batch_observed = summary
            .max_batch_observed
            .max(u16::try_from(page.len()).unwrap_or(u16::MAX));
        if page.is_empty() {
            break;
        }
        for (_, digest, size) in &page {
            blobs
                .verify(digest, *size)
                .map_err(|error| StoreError::Storage(Box::new(error)))?;
        }
        summary.blobs_verified += page.len() as u64;
        evidence_cursor = page.last().map(|(operation_id, _, _)| operation_id.clone());
    }

    blobs
        .cleanup_temporary()
        .map_err(|error| StoreError::Storage(Box::new(error)))?;
    Ok(summary)
}

fn document_asset_associations(
    sources: &[DocumentAssetSource],
    blobs: &BlobStore,
) -> Result<Vec<DocumentAssetAssociation>, StoreError> {
    sources
        .iter()
        .map(|source| {
            let asset_paths = if let Some(blob_sha256) = &source.blob_sha256 {
                let bytes = blobs
                    .read(blob_sha256)
                    .map_err(|error| StoreError::Storage(Box::new(error)))?;
                let source_text =
                    String::from_utf8(bytes).map_err(|_| StoreError::CorruptData {
                        field: "document blob UTF-8",
                    })?;
                retained_asset_paths(&source.content_type, &source_text).map_err(|_| {
                    StoreError::CorruptData {
                        field: "document content type",
                    }
                })?
            } else {
                Vec::new()
            };
            Ok(DocumentAssetAssociation {
                document_id: source.document_id.clone(),
                revision: source.revision,
                asset_paths,
                active_head: source.active_head,
                head_revision: source.head_revision,
            })
        })
        .collect()
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
    if command.format_version != 1 {
        return Err(StoreError::InvalidReviewState);
    }
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
                .put(note.source.as_bytes(), &note.content_type)
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

#[cfg(test)]
fn replace_command(
    store: &mut SqliteStore,
    blobs: &BlobStore,
    prepared: &PreparedServerImport,
    created_at_us: i64,
) -> Result<ImportSummary, ServiceError> {
    let digest = format!("{:x}", Sha256::digest(prepared.org.as_bytes()));
    let existed = blobs.contains(&digest)?;
    let blob = blobs.put(prepared.org.as_bytes(), "text/org; charset=utf-8")?;
    match persist_server_import(store, prepared, &blob, created_at_us) {
        Ok(summary) => Ok(summary),
        Err(error) => {
            if !existed {
                blobs.discard(&blob.sha256)?;
            }
            Err(ServiceError::Application(error))
        }
    }
}

fn replace_note_command(
    store: &mut SqliteStore,
    blobs: &BlobStore,
    prepared: &PreparedNote,
    created_at_us: i64,
    author_device_id: Option<&str>,
) -> Result<DocumentHead, ServiceError> {
    let blob = blobs.put(prepared.source.as_bytes(), &prepared.content_type)?;
    store
        .replace_note(prepared, &blob, created_at_us, author_device_id)
        .map_err(ServiceError::Store)?;
    store
        .document_head(&prepared.document_id)
        .map_err(ServiceError::Store)?
        .ok_or(ServiceError::Store(StoreError::NotFound))
}

fn apply_device_document_mutation_command(
    store: &mut SqliteStore,
    blobs: &BlobStore,
    operation: &PreparedDeviceDocumentMutation,
    created_at_us: i64,
) -> Result<DeviceMutationExecution, ServiceError> {
    let needs_blob = store
        .device_document_mutation_needs_blob(operation)
        .map_err(ServiceError::Store)?;
    let blob = if needs_blob {
        let digest = &operation.prepared.blob_sha256;
        let existed = blobs.contains(digest)?;
        Some((
            blobs.put(
                operation.prepared.source.as_bytes(),
                &operation.prepared.content_type,
            )?,
            !existed,
        ))
    } else {
        None
    };
    let result = store
        .apply_device_document_mutation(
            operation,
            blob.as_ref().map(|(blob, _)| blob),
            created_at_us,
        )
        .map_err(ServiceError::Store);
    let discard_blob = match &result {
        Ok(execution) => matches!(&execution.result, DeviceMutationResult::Rejected { .. }),
        Err(_) => true,
    };
    if discard_blob
        && let Some((blob, true)) = &blob
        && !store
            .has_blob_record(&blob.sha256)
            .map_err(ServiceError::Store)?
    {
        blobs.discard(&blob.sha256)?;
    }
    result
}

fn apply_device_desired_state_mutation_command(
    store: &mut SqliteStore,
    blobs: &BlobStore,
    operation: &PreparedDeviceDesiredStateMutation,
    asset_bytes: Option<&[u8]>,
    created_at_us: i64,
) -> Result<DeviceMutationExecution, ServiceError> {
    let needs_blob = store
        .device_desired_state_mutation_needs_blob(operation)
        .map_err(ServiceError::Store)?;
    let mut new_blob_digest = None;
    match (&operation.mutation, asset_bytes) {
        (
            DeviceDesiredStateMutation::PutAsset {
                digest,
                body_sha256,
                size_bytes,
                media_type,
                ..
            },
            Some(bytes),
        ) => {
            let actual_digest = format!("{:x}", Sha256::digest(bytes));
            let actual_size = bytes.len() as u64;
            if actual_digest != *body_sha256 {
                return Err(ServiceError::Corrupt);
            }
            if body_sha256 != digest {
                return Err(ServiceError::AssetDigest);
            }
            if actual_size != *size_bytes {
                return Err(ServiceError::Corrupt);
            }
            if needs_blob {
                let existed = blobs.contains(digest)?;
                let blob = blobs.put(bytes, media_type)?;
                if blob.sha256 != actual_digest || blob.size_bytes != actual_size {
                    if !existed {
                        blobs.discard(&blob.sha256)?;
                    }
                    return Err(ServiceError::Corrupt);
                }
                if !existed {
                    new_blob_digest = Some(blob.sha256);
                }
            }
        }
        (DeviceDesiredStateMutation::PutAsset { .. }, None) | (_, Some(_)) => {
            return Err(ServiceError::Corrupt);
        }
        (_, None) => {}
    }
    let result = store
        .apply_device_desired_state_mutation(operation, created_at_us)
        .map_err(ServiceError::Store);
    let discard_blob = match &result {
        Ok(execution) => matches!(&execution.result, DeviceMutationResult::Rejected { .. }),
        Err(_) => true,
    };
    if discard_blob
        && let Some(digest) = new_blob_digest
        && !store
            .has_blob_record(&digest)
            .map_err(ServiceError::Store)?
    {
        blobs.discard(&digest)?;
    }
    result
}

struct AssetWrite<'a> {
    prepared: &'a PreparedAsset,
    bytes: &'a [u8],
    media_type: &'a str,
    created_at_us: i64,
    author_device_id: Option<&'a str>,
}

fn put_asset_command(
    store: &mut SqliteStore,
    blobs: &BlobStore,
    write: AssetWrite<'_>,
) -> Result<AssetHead, ServiceError> {
    let blob = blobs.put(write.bytes, write.media_type)?;
    store
        .put_asset(
            write.prepared,
            &blob,
            write.created_at_us,
            write.author_device_id,
        )
        .map_err(ServiceError::Store)?;
    store
        .asset_head(&write.prepared.logical_path)
        .map_err(ServiceError::Store)?
        .ok_or(ServiceError::Store(StoreError::NotFound))
}

fn stage_blob_command(
    store: &mut SqliteStore,
    blobs: &BlobStore,
    bytes: &[u8],
    media_type: &str,
    created_at_us: i64,
) -> Result<BlobMeta, ServiceError> {
    let digest = format!("{:x}", Sha256::digest(bytes));
    let existed = blobs.contains(&digest)?;
    let blob = blobs.put(bytes, media_type)?;
    match store.stage_blob(&blob, created_at_us) {
        Ok(()) => Ok(blob),
        Err(stage) if existed => Err(ServiceError::Store(stage)),
        Err(stage) => match store.has_blob_reference(&blob.sha256) {
            Ok(true) => Err(ServiceError::Store(stage)),
            Ok(false) => match blobs.discard(&blob.sha256) {
                Ok(()) => Err(ServiceError::Store(stage)),
                Err(cleanup) => Err(ServiceError::StageBlobCleanup { stage, cleanup }),
            },
            Err(reference_check) => Err(ServiceError::StageBlobReferenceCheck {
                stage,
                reference_check,
            }),
        },
    }
}

fn run_store_command(
    store: &mut SqliteStore,
    database: &Path,
    blobs: &BlobStore,
    backup_sender: &SyncSender<BackupCommand>,
    command: StoreCommand,
) {
    match command {
        StoreCommand::Ready { response } => send(response, store.settings().map(drop)),
        StoreCommand::Repair {
            batch_size,
            response,
        } => send(response, repair_canonical_state(store, blobs, batch_size)),
        #[cfg(test)]
        StoreCommand::Replace {
            prepared,
            created_at_us,
            response,
        } => send(
            response,
            replace_command(store, blobs, &prepared, created_at_us),
        ),
        StoreCommand::Get {
            document_id,
            response,
        } => send(response, store.get_document(&document_id)),

        StoreCommand::DueThemataPage { query, response } => run_due_page(store, query, response),
        StoreCommand::ReplaceNote {
            prepared,
            created_at_us,
            author_device_id,
            response,
        } => send(
            response,
            replace_note_command(
                store,
                blobs,
                &prepared,
                created_at_us,
                author_device_id.as_deref(),
            ),
        ),
        StoreCommand::ApplyDeviceDocumentMutation {
            operation,
            created_at_us,
            response,
        } => send(
            response,
            apply_device_document_mutation_command(store, blobs, &operation, created_at_us),
        ),
        StoreCommand::ApplyDeviceDocumentLifecycleMutation {
            operation,
            created_at_us,
            response,
        } => send(
            response,
            store
                .apply_device_document_lifecycle_mutation(&operation, created_at_us)
                .map_err(ServiceError::Store),
        ),
        StoreCommand::ApplyDeviceDesiredStateMutation {
            operation,
            asset_bytes,
            created_at_us,
            response,
        } => send(
            response,
            apply_device_desired_state_mutation_command(
                store,
                blobs,
                &operation,
                asset_bytes.as_deref(),
                created_at_us,
            ),
        ),
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
            bytes,
            media_type,
            created_at_us,
            author_device_id,
            response,
        } => send(
            response,
            put_asset_command(
                store,
                blobs,
                AssetWrite {
                    prepared: &prepared,
                    bytes: &bytes,
                    media_type: &media_type,
                    created_at_us,
                    author_device_id: author_device_id.as_deref(),
                },
            ),
        ),
        StoreCommand::StageBlob {
            bytes,
            media_type,
            created_at_us,
            response,
        } => send(
            response,
            stage_blob_command(store, blobs, &bytes, &media_type, created_at_us),
        ),
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
        StoreCommand::NodeThemata {
            node_id,
            limit,
            after,
            response,
        } => send(
            response,
            store.node_themata(&node_id, limit, after.as_deref()),
        ),
        StoreCommand::SearchNodes {
            query,
            limit,
            after,
            response,
        } => send(
            response,
            store.search_nodes(&query, limit, after.as_deref()),
        ),
        StoreCommand::NodeCatalog {
            device_id,
            document_id,
            limit,
            after,
            response,
        } => send(
            response,
            store.node_catalog(&device_id, document_id.as_deref(), limit, after.as_deref()),
        ),
        StoreCommand::ThemaCatalog {
            device_id,
            query,
            limit,
            after,
            response,
        } => send(
            response,
            store.thema_catalog(&device_id, &query, limit, after.as_deref()),
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
        StoreCommand::Dashboard {
            days,
            now_us,
            response,
        } => send(response, store.dashboard(days, now_us)),
        StoreCommand::ReviewSessions { query, response } => {
            send(response, store.review_sessions(&query))
        }
        StoreCommand::ReviewSessionSummary {
            session_id,
            response,
        } => send(response, store.review_session_summary(&session_id)),
        StoreCommand::ReviewHistory { query, response } => {
            send(response, store.review_history(&query))
        }
        StoreCommand::ReviewQueueSummary {
            source,
            now_us,
            response,
        } => send(response, store.review_queue_summary(&source, now_us)),
        StoreCommand::ThemaReviewState { thema_id, response } => {
            send(response, store.thema_review_state(&thema_id))
        }
        StoreCommand::CheckClozeAttempt {
            attempt_id,
            actor_device_id,
            inputs,
            response,
        } => send(
            response,
            store.check_cloze_attempt(&attempt_id, actor_device_id.as_deref(), &inputs),
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
        StoreCommand::BeginHttpReviewAttempt {
            session_id,
            actor_device_id,
            started_at_us,
            response,
        } => send(
            response,
            store.begin_http_review_attempt(&session_id, actor_device_id.as_deref(), started_at_us),
        ),
        StoreCommand::GradeReviewAttempt {
            attempt_id,
            actor_device_id,
            answer,
            answered_at_us,
            response,
        } => send(
            response,
            store.grade_review_attempt(
                &attempt_id,
                actor_device_id.as_deref(),
                &answer,
                answered_at_us,
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
        StoreCommand::ToggleAttemptSuspension {
            attempt_id,
            actor_device_id,
            response,
        } => send(
            response,
            store.toggle_attempt_suspension(&attempt_id, actor_device_id.as_deref()),
        ),
        StoreCommand::ReviewAttemptState {
            attempt_id,
            actor_device_id,
            response,
        } => send(
            response,
            store.review_attempt_state(&attempt_id, actor_device_id.as_deref()),
        ),
        StoreCommand::ApplyReviewAttemptOperation {
            operation,
            authority,
            operated_at_us,
            response,
        } => send(
            response,
            store.apply_review_attempt_operation(&operation, &authority, operated_at_us),
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
            response,
        } => {
            let command = BackupCommand::Create {
                source: database.to_owned(),
                destination,
                created_at_us,
                response,
            };
            match backup_sender.try_send(command) {
                Ok(()) => {}
                Err(TrySendError::Full(BackupCommand::Create { response, .. })) => {
                    send(response, Err(ServiceError::Busy));
                }
                Err(TrySendError::Disconnected(BackupCommand::Create { response, .. })) => {
                    send(response, Err(ServiceError::Stopped));
                }
                Err(_) => unreachable!("backup command kind changed during send"),
            }
        }
        StoreCommand::Shutdown { .. } => unreachable!("shutdown handled by store actor"),
    }
}

fn backup_actor(
    blobs: Arc<BlobStore>,
    receiver: mpsc::Receiver<BackupCommand>,
    cancelled: Arc<BackupCancellation>,
) {
    for command in receiver {
        match command {
            BackupCommand::Shutdown => break,
            #[cfg(test)]
            BackupCommand::Block { started, release } => {
                let _ = started.send(());
                let _ = release.recv();
            }
            BackupCommand::Create {
                source,
                destination,
                created_at_us,
                response,
            } => {
                let result = snapshot_backup_cancellable(&source, &destination, &cancelled)
                    .and_then(|snapshot| {
                        create_backup_cancellable(
                            snapshot,
                            &blobs,
                            &destination,
                            created_at_us,
                            &cancelled,
                        )
                    });
                send(response, result.map_err(ServiceError::Backup));
            }
            BackupCommand::Verify { path, response } => send(
                response,
                verify_backup_cancellable(&path, &cancelled).map_err(ServiceError::Backup),
            ),
            BackupCommand::Prune {
                directory,
                keep,
                response,
            } => send(
                response,
                prune_backups(&directory, keep).map_err(ServiceError::Backup),
            ),
            BackupCommand::ValidateRestore {
                path,
                temporary,
                response,
            } => send(
                response,
                validate_restore_cancellable(&path, &temporary, &cancelled)
                    .map_err(ServiceError::Backup),
            ),
        }
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
            cursor.thema_id().to_owned(),
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

#[allow(dead_code)]
#[derive(Debug, Error)]
pub(crate) enum ServiceError {
    #[error("store queue is full")]
    Busy,
    #[error("store service stopped")]
    Stopped,
    #[error("service shutdown exceeded its deadline")]
    ShutdownDeadlineExceeded,
    #[error("repair batch size is invalid")]
    InvalidRepairBatchSize,
    #[error("stored content is corrupt")]
    Corrupt,
    #[error("asset digest does not match its canonical path")]
    AssetDigest,

    #[error("blob metadata is invalid")]
    InvalidBlobMetadata,
    #[error("blob operation failed")]
    Blob(#[from] BlobError),
    #[error("store operation failed")]
    Store(#[source] StoreError),
    #[error("blob staging failed and reference verification also failed")]
    StageBlobReferenceCheck {
        #[source]
        stage: StoreError,
        reference_check: StoreError,
    },
    #[error("blob staging failed and file cleanup also failed")]
    StageBlobCleanup {
        #[source]
        stage: StoreError,
        cleanup: BlobError,
    },
    #[error("application operation failed")]
    Application(#[source] ApplicationError),
    #[error("review request is invalid")]
    Review(#[source] ReviewError),
    #[error("settings request is invalid")]
    Settings(#[source] SettingsError),
    #[error("backup operation failed")]
    Backup(#[source] BackupError),
}

#[cfg(test)]
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
    use std::{
        sync::{Arc, mpsc},
        thread,
        time::{Duration, Instant},
    };

    use sha2::Digest as _;
    use tokio::sync::oneshot;

    use super::{
        BackupCommand, BackupError, ServiceError, ServiceHandle, ServiceRuntime, StoreCommand,
        complete_shutdown_phase, repair_canonical_state, sleep_until_next_shutdown_poll,
        stage_blob_command, store_actor,
    };
    use crate::application::{
        AssetReconciliationStatus, ContentBase, DeviceDesiredStateMutation, DeviceMutationResult,
        ImportDocument, MAX_DOCUMENT_SOURCE_BYTES, OfflineAssetConflictReason,
        OfflineAssetOperation, OfflineContentMutation, OfflineContentOperation, ReplicaProjection,
        SettingsInput, canonical_asset_path, generate_device, prepare_asset,
        prepare_device_desired_state_mutation, prepare_device_document_mutation, prepare_note,
        prepare_thema, thema_content,
    };
    use crate::blob::{BlobMeta, BlobStore};
    use crate::protocol::canonical_asset_operation;
    use crate::store::{SqliteStore, StoreError};

    async fn runtime(directory: &tempfile::TempDir) -> ServiceRuntime {
        ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            8,
        )
        .await
        .unwrap()
    }

    fn offline_put(
        device_id: String,
        operation_id: &str,
        source: String,
    ) -> OfflineContentOperation {
        let content_sha256 = format!("{:x}", sha2::Sha256::digest(source.as_bytes()));
        OfflineContentOperation {
            operation_id: operation_id.into(),
            device_id,
            device_sequence: 1,
            cursor_server_id: "server".into(),
            cursor: 0,
            cursor_token: "cursor".into(),
            authored_at_us: 1,
            mutation: OfflineContentMutation::Put {
                document_id: "offline-document".into(),
                logical_path: "offline.org".into(),
                content_sha256,
                content_type: "text/org; charset=utf-8".into(),
                source,
                base: ContentBase::Absent,
            },
        }
    }

    #[tokio::test]
    async fn readiness_round_trip_requires_the_store_actor() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        assert!(service.ready().await.is_ok());
        drop(service);
        runtime.finish().await.unwrap();

        let blobs = Arc::new(BlobStore::open(directory.path().join("stopped-blobs")).unwrap());
        let (sender, receiver) = mpsc::sync_channel(1);
        let (backup_sender, backup_receiver) = mpsc::sync_channel(1);
        drop(receiver);
        drop(backup_receiver);
        let stopped = ServiceHandle {
            sender,
            backup_sender,
            blobs,
        };
        assert!(matches!(stopped.ready().await, Err(ServiceError::Stopped)));
    }

    #[tokio::test]
    async fn full_queue_rejects_blob_before_durable_write() {
        let directory = tempfile::tempdir().unwrap();
        let blob_path = directory.path().join("blobs");
        let blobs = Arc::new(BlobStore::open(&blob_path).unwrap());
        let (sender, _receiver) = mpsc::sync_channel(0);
        let (backup_sender, _backup_receiver) = mpsc::sync_channel(0);
        let service = ServiceHandle {
            sender,
            backup_sender,
            blobs,
        };
        let bytes = b"not accepted".to_vec();
        let digest = format!("{:x}", sha2::Sha256::digest(&bytes));

        let result = service
            .stage_blob(&digest, bytes, "application/octet-stream".into(), 1)
            .await;

        assert!(matches!(result, Err(ServiceError::Busy)));
        assert!(!blob_path.join(&digest[..2]).join(digest).exists());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn maximum_content_preparation_keeps_the_runtime_schedulable() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let device = generate_device("Device".into(), vec!["content:write".into()]).unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let mut source = "* Node\n".repeat(MAX_DOCUMENT_SOURCE_BYTES / 7);
        source.push_str(&"x".repeat(MAX_DOCUMENT_SOURCE_BYTES - source.len()));
        assert_eq!(source.len(), MAX_DOCUMENT_SOURCE_BYTES);
        let operation = offline_put(device_id.clone(), "maximum-content", source);
        let (started_sender, started_receiver) = oneshot::channel();
        let reconcile = tokio::spawn(async move {
            started_sender.send(()).unwrap();
            service
                .reconcile_content_operation(
                    1,
                    operation,
                    "maximum-content-payload".into(),
                    Some(device_id),
                    2,
                )
                .await
        });

        let started_at = Instant::now();
        started_receiver.await.unwrap();
        assert!(started_at.elapsed() < Duration::from_millis(100));
        let _ = reconcile.await.unwrap();
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn unsupported_content_format_has_no_durable_side_effects() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let blob_path = directory.path().join("blobs");
        let runtime = ServiceRuntime::start(database.clone(), blob_path.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let device = generate_device("Device".into(), vec!["content:write".into()]).unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let source = "#+TITLE: Unsupported\n".to_owned();
        let digest = format!("{:x}", sha2::Sha256::digest(source.as_bytes()));
        let result = service
            .reconcile_content_operation(
                2,
                offline_put(device_id.clone(), "unsupported-content", source),
                "unsupported-content-payload".into(),
                Some(device_id.clone()),
                2,
            )
            .await;
        assert!(matches!(
            result,
            Err(ServiceError::Store(StoreError::InvalidReviewState))
        ));
        drop(service);
        runtime.finish().await.unwrap();

        let connection = rusqlite::Connection::open(database).unwrap();
        let device_operations: u64 = connection
            .query_row("SELECT count(*) FROM device_operation", [], |row| {
                row.get(0)
            })
            .unwrap();
        let content_operations: u64 = connection
            .query_row(
                "SELECT count(*) FROM offline_content_operation",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let device_sequence: u64 = connection
            .query_row(
                "SELECT last_device_sequence FROM device WHERE device_id = ?1",
                [device_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            (device_operations, content_operations, device_sequence),
            (0, 0, 0)
        );
        assert!(!blob_path.join(&digest[..2]).join(digest).exists());
    }

    #[tokio::test]
    async fn restart_preserves_large_staged_history() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let blob_path = directory.path().join("blobs");
        let blobs = BlobStore::open(&blob_path).unwrap();
        drop(SqliteStore::open(&database).unwrap());
        let mut connection = rusqlite::Connection::open(&database).unwrap();
        let transaction = connection.transaction().unwrap();
        let mut preserved_digest = None;
        for value in 1_u64..=256 {
            let blob = blobs
                .put(&value.to_le_bytes(), "application/octet-stream")
                .unwrap();
            transaction
                .execute(
                    "INSERT INTO staged_blob(content_sha256, size_bytes, staged_at_us)
                     VALUES (?1, ?2, ?3)",
                    rusqlite::params![blob.sha256, blob.size_bytes, value],
                )
                .unwrap();
            preserved_digest.get_or_insert(blob.sha256);
        }
        transaction.commit().unwrap();
        let expected_count: u64 = connection
            .query_row("SELECT count(*) FROM staged_blob", [], |row| row.get(0))
            .unwrap();
        drop(connection);

        let runtime = ServiceRuntime::start(database.clone(), blob_path.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        service.ready().await.unwrap();
        drop(service);
        runtime.finish().await.unwrap();

        let connection = rusqlite::Connection::open(database).unwrap();
        let actual_count: u64 = connection
            .query_row("SELECT count(*) FROM staged_blob", [], |row| row.get(0))
            .unwrap();
        assert_eq!(actual_count, expected_count);
        let preserved_digest = preserved_digest.unwrap();
        assert!(blobs.contains(&preserved_digest).unwrap());
    }

    #[test]
    fn failed_stage_removes_only_new_unreferenced_blob() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let blob_path = directory.path().join("blobs");
        let blobs = BlobStore::open(&blob_path).unwrap();
        let mut store = SqliteStore::open(&database).unwrap();
        let existing = blobs
            .put(b"canonical blob", "application/octet-stream")
            .unwrap();
        let asset_path = canonical_asset_path(&existing.sha256, "existing.png").unwrap();
        let prepared = prepare_asset(asset_path, "existing.png".into(), None).unwrap();
        store.put_asset(&prepared, &existing, 1, None).unwrap();
        blobs.discard(&existing.sha256).unwrap();

        let new_bytes = b"new staged blob";
        let new_digest = format!("{:x}", sha2::Sha256::digest(new_bytes));
        let new_error =
            stage_blob_command(&mut store, &blobs, new_bytes, "application/octet-stream", 0)
                .unwrap_err();
        assert!(matches!(new_error, ServiceError::Store(_)));
        assert!(!blob_path.join(&new_digest[..2]).join(&new_digest).exists());

        let preexisting_orphan = blobs
            .put(b"preexisting orphan", "application/octet-stream")
            .unwrap();
        let orphan_error = stage_blob_command(
            &mut store,
            &blobs,
            b"preexisting orphan",
            "application/octet-stream",
            0,
        )
        .unwrap_err();
        assert!(matches!(orphan_error, ServiceError::Store(_)));
        assert!(
            blob_path
                .join(&preexisting_orphan.sha256[..2])
                .join(&preexisting_orphan.sha256)
                .exists()
        );

        let existing_error = stage_blob_command(
            &mut store,
            &blobs,
            b"canonical blob",
            "application/octet-stream",
            0,
        )
        .unwrap_err();
        assert!(matches!(existing_error, ServiceError::Store(_)));
        assert!(
            blob_path
                .join(&existing.sha256[..2])
                .join(&existing.sha256)
                .exists()
        );

        let staged_bytes = b"catalogued staged blob";
        let staged = blobs.put(staged_bytes, "application/octet-stream").unwrap();
        store.stage_blob(&staged, 4).unwrap();
        blobs.discard(&staged.sha256).unwrap();
        let staged_error = stage_blob_command(
            &mut store,
            &blobs,
            staged_bytes,
            "application/octet-stream",
            0,
        )
        .unwrap_err();
        assert!(matches!(staged_error, ServiceError::Store(_)));
        assert!(
            blob_path
                .join(&staged.sha256[..2])
                .join(&staged.sha256)
                .exists()
        );
    }

    #[test]
    fn repair_pages_and_preserves_staged_blobs() {
        let directory = tempfile::tempdir().unwrap();
        let blobs = BlobStore::open(directory.path().join("blobs")).unwrap();
        let mut store = SqliteStore::open(directory.path().join("gnosis.sqlite")).unwrap();
        for (created_at_us, bytes) in [(1, b"one".as_slice()), (2, b"two".as_slice())] {
            let blob = blobs.put(bytes, "application/octet-stream").unwrap();
            store
                .stage_blob(
                    &BlobMeta {
                        sha256: blob.sha256,
                        size_bytes: blob.size_bytes,
                        media_type: blob.media_type,
                    },
                    created_at_us,
                )
                .unwrap();
        }

        let summary = repair_canonical_state(&mut store, &blobs, 1).unwrap();
        assert_eq!(summary.blobs_verified, 2);
        assert_eq!(summary.max_batch_observed, 1);
        assert_eq!(store.staged_blobs_page(None, 10).unwrap().len(), 2);
    }

    #[tokio::test]
    async fn restart_defers_bounded_association_repair_and_repair_commits_each_page() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let blob_path = directory.path().join("blobs");
        let runtime = ServiceRuntime::start(database.clone(), blob_path.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();

        let mut asset_paths = Vec::new();
        for (created_at_us, name, bytes) in [
            (1, "active.png", b"active asset".to_vec()),
            (2, "history.png", b"history asset".to_vec()),
        ] {
            let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
            let path = canonical_asset_path(&digest, name).unwrap();
            service
                .put_asset(
                    prepare_asset(path.clone(), name.into(), None).unwrap(),
                    bytes,
                    "image/png".into(),
                    created_at_us,
                    None,
                )
                .await
                .unwrap();
            asset_paths.push(path);
        }

        let active_org = format!("#+title: Active\n[[file:{}]]\n", asset_paths[0]);
        service
            .replace_note(
                prepare_note(
                    Some("a-active".into()),
                    "active.org".into(),
                    active_org,
                    None,
                )
                .unwrap(),
                3,
                None,
            )
            .await
            .unwrap();
        let history_org = format!("#+title: History\n[[file:{}]]\n", asset_paths[1]);
        let history_note = prepare_note(
            Some("z-tombstone".into()),
            "history.org".into(),
            history_org.clone(),
            None,
        )
        .unwrap();
        let history_digest = history_note.blob_sha256.clone();
        service.replace_note(history_note, 4, None).await.unwrap();
        service
            .tombstone_note(
                crate::domain::DocumentId::new("z-tombstone").unwrap(),
                1,
                5,
                None,
            )
            .await
            .unwrap();
        drop(service);
        runtime.finish().await.unwrap();

        let connection = rusqlite::Connection::open(&database).unwrap();
        connection
            .execute("DELETE FROM document_revision_asset", [])
            .unwrap();
        connection
            .execute("DELETE FROM document_asset", [])
            .unwrap();
        connection
            .execute(
                "INSERT INTO document_asset(document_id, asset_path) VALUES ('z-tombstone', ?1)",
                [asset_paths[1].as_str()],
            )
            .unwrap();
        drop(connection);
        let history_blob = blob_path.join(&history_digest[..2]).join(&history_digest);
        std::fs::write(&history_blob, b"corrupt").unwrap();

        let runtime = ServiceRuntime::start(database.clone(), blob_path, 8)
            .await
            .unwrap();
        let service = runtime.handle();
        service.ready().await.unwrap();
        let connection = rusqlite::Connection::open(&database).unwrap();
        let stale_current: Vec<(String, String)> = connection
            .prepare("SELECT document_id, asset_path FROM document_asset ORDER BY document_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            stale_current,
            vec![("z-tombstone".into(), asset_paths[1].clone())]
        );
        drop(connection);

        assert!(service.repair(1).await.is_err());
        let connection = rusqlite::Connection::open(&database).unwrap();
        let partial_current: Vec<(String, String)> = connection
            .prepare("SELECT document_id, asset_path FROM document_asset ORDER BY document_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            partial_current,
            vec![("a-active".into(), asset_paths[0].clone())]
        );
        let partial_history: u64 = connection
            .query_row("SELECT count(*) FROM document_revision_asset", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(partial_history, 1);
        drop(connection);

        std::fs::write(&history_blob, history_org).unwrap();
        let summary = service.repair(1).await.unwrap();
        assert_eq!(summary.document_revisions_repaired, 3);
        assert_eq!(summary.max_batch_observed, 1);
        let connection = rusqlite::Connection::open(&database).unwrap();
        let current: Vec<(String, String)> = connection
            .prepare("SELECT document_id, asset_path FROM document_asset ORDER BY document_id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(current, vec![("a-active".into(), asset_paths[0].clone())]);
        let history: Vec<(String, u64, String)> = connection
            .prepare(
                "SELECT document_id, revision, asset_path FROM document_revision_asset
                 ORDER BY document_id, revision, asset_path",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(
            history,
            vec![
                ("a-active".into(), 1, asset_paths[0].clone()),
                ("z-tombstone".into(), 1, asset_paths[1].clone()),
            ]
        );
        drop(connection);
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn rejected_desired_asset_does_not_publish_a_blob() {
        let directory = tempfile::tempdir().unwrap();
        let blob_path = directory.path().join("blobs");
        let database = directory.path().join("gnosis.sqlite");
        let runtime = ServiceRuntime::start(database.clone(), blob_path.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let device = generate_device("Device".into(), vec!["content:write".into()]).unwrap();
        let device_id = device.device_id.clone();
        service.create_device(device, 1).await.unwrap();
        let first_bytes = b"accepted asset".to_vec();
        let first_digest = format!("{:x}", sha2::Sha256::digest(&first_bytes));
        let asset_path = canonical_asset_path(&first_digest, "image.png").unwrap();
        let first = prepare_device_desired_state_mutation(
            1,
            "accepted-asset".into(),
            device_id.clone(),
            DeviceDesiredStateMutation::PutAsset {
                prepared: prepare_asset(asset_path.clone(), "image.png".into(), None).unwrap(),
                digest: first_digest.clone(),
                body_sha256: first_digest.clone(),
                size_bytes: first_bytes.len() as u64,
                media_type: "image/png".into(),
            },
        )
        .unwrap();
        service
            .apply_device_desired_state_mutation(first, Some(first_bytes), 2)
            .await
            .unwrap();

        let bytes = b"rejected asset".to_vec();
        let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
        let operation = prepare_device_desired_state_mutation(
            1,
            "rejected-asset".into(),
            device_id,
            DeviceDesiredStateMutation::PutAsset {
                prepared: prepare_asset(asset_path, "image.png".into(), Some(99)).unwrap(),
                digest: digest.clone(),
                body_sha256: digest.clone(),
                size_bytes: bytes.len() as u64,
                media_type: "image/png".into(),
            },
        )
        .unwrap();

        let execution = service
            .apply_device_desired_state_mutation(operation.clone(), Some(bytes.clone()), 3)
            .await
            .unwrap();

        assert!(matches!(
            execution.result,
            DeviceMutationResult::Rejected { .. }
        ));
        assert!(!blob_path.join(&digest[..2]).join(&digest).exists());
        assert!(!blob_path.join(&digest[..2]).exists());
        assert!(
            blob_path
                .join(&first_digest[..2])
                .join(first_digest)
                .exists()
        );
        drop(service);
        runtime.finish().await.unwrap();

        let runtime = ServiceRuntime::start(database, blob_path.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let replay = service
            .apply_device_desired_state_mutation(operation, Some(bytes), 4)
            .await
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(execution.result, replay.result);
        assert!(!blob_path.join(&digest[..2]).join(&digest).exists());
        assert!(!blob_path.join(&digest[..2]).exists());
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[test]
    fn accepted_blob_write_survives_dropped_response() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let blob_path = directory.path().join("blobs");
        let blobs = Arc::new(BlobStore::open(&blob_path).unwrap());
        let bytes = b"accepted before disconnect".to_vec();
        let digest = format!("{:x}", sha2::Sha256::digest(&bytes));
        let (sender, receiver) = mpsc::sync_channel(1);
        let (response, response_receiver) = oneshot::channel();
        let command = StoreCommand::StageBlob {
            bytes: bytes.clone(),
            media_type: "application/octet-stream".into(),
            created_at_us: 1,
            response,
        };
        assert!(sender.try_send(command).is_ok());
        drop(response_receiver);

        let (ready, readiness) = mpsc::sync_channel(1);
        let (backup_sender, _backup_receiver) = mpsc::sync_channel(1);
        let actor_blobs = blobs.clone();
        let actor_database = database.clone();
        let actor = thread::spawn(move || {
            store_actor(actor_database, actor_blobs, backup_sender, receiver, ready)
        });
        readiness.recv().unwrap().unwrap();
        drop(sender);
        actor.join().unwrap();

        let connection = rusqlite::Connection::open(database).unwrap();
        let staged_size = connection
            .query_row(
                "SELECT size_bytes FROM staged_blob WHERE content_sha256 = ?1",
                [&digest],
                |row| row.get::<_, u64>(0),
            )
            .unwrap();
        assert_eq!(staged_size, bytes.len() as u64);
        assert!(blob_path.join(&digest[..2]).join(digest).exists());
    }

    #[test]
    fn accepted_document_mutation_survives_dropped_response() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let blob_path = directory.path().join("blobs");
        let blobs = Arc::new(BlobStore::open(&blob_path).unwrap());
        let device = generate_device("Device".into(), vec!["content:write".into()]).unwrap();
        let mut store = SqliteStore::open(&database).unwrap();
        store.create_device(&device, 1).unwrap();
        drop(store);
        let prepared = prepare_note(
            Some("document".into()),
            "note.org".into(),
            "#+title: Durable\n".into(),
            None,
        )
        .unwrap();
        let digest = prepared.blob_sha256.clone();
        let operation =
            prepare_device_document_mutation(1, "operation".into(), device.device_id, prepared)
                .unwrap();
        let (sender, receiver) = mpsc::sync_channel(1);
        let (response, response_receiver) = oneshot::channel();
        sender
            .try_send(StoreCommand::ApplyDeviceDocumentMutation {
                operation: Box::new(operation),
                created_at_us: 2,
                response,
            })
            .unwrap();
        drop(response_receiver);

        let (ready, readiness) = mpsc::sync_channel(1);
        let (backup_sender, _backup_receiver) = mpsc::sync_channel(1);
        let actor_blobs = blobs.clone();
        let actor_database = database.clone();
        let actor = thread::spawn(move || {
            store_actor(actor_database, actor_blobs, backup_sender, receiver, ready)
        });
        readiness.recv().unwrap().unwrap();
        drop(sender);
        actor.join().unwrap();

        let connection = rusqlite::Connection::open(database).unwrap();
        let receipts: u64 = connection
            .query_row("SELECT count(*) FROM device_mutation_receipt", [], |row| {
                row.get(0)
            })
            .unwrap();
        let revisions: u64 = connection
            .query_row("SELECT count(*) FROM document_revision", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!((receipts, revisions), (1, 1));
        assert!(blob_path.join(&digest[..2]).join(digest).exists());
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
    async fn backup_filesystem_work_does_not_occupy_the_store_actor() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            2,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let note = prepare_note(
            None,
            "notes/snapshot.org".into(),
            "#+TITLE: Snapshot\n".into(),
            None,
        )
        .unwrap();
        service.replace_note(note, 1, None).await.unwrap();
        let (started, started_receiver) = mpsc::sync_channel(1);
        let (release, release_receiver) = mpsc::sync_channel(1);
        service
            .backup_sender
            .try_send(BackupCommand::Block {
                started,
                release: release_receiver,
            })
            .unwrap();
        tokio::task::spawn_blocking(move || started_receiver.recv())
            .await
            .unwrap()
            .unwrap();

        let (response, backup_result) = oneshot::channel();
        service
            .sender
            .try_send(StoreCommand::CreateBackup {
                destination: directory.path().join("backup"),
                created_at_us: 1,
                response,
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), service.ready())
            .await
            .expect("store actor stayed available")
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), service.settings())
            .await
            .expect("normal read stayed available")
            .unwrap();

        let (second_started, second_started_receiver) = mpsc::sync_channel(1);
        let (second_release, second_release_receiver) = mpsc::sync_channel(1);
        service
            .backup_sender
            .try_send(BackupCommand::Block {
                started: second_started,
                release: second_release_receiver,
            })
            .unwrap();
        assert!(matches!(
            service
                .verify_backup(directory.path().join("never-enqueued"))
                .await,
            Err(ServiceError::Busy)
        ));

        release.send(()).unwrap();
        let summary = backup_result.await.unwrap().unwrap();
        assert_eq!(summary.blob_count, 1);
        tokio::task::spawn_blocking(move || second_started_receiver.recv())
            .await
            .unwrap()
            .unwrap();
        second_release.send(()).unwrap();
        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn shutdown_deadline_cancels_an_accepted_backup_without_publication() {
        let directory = tempfile::tempdir().unwrap();
        let backup = directory.path().join("backup");
        let runtime = ServiceRuntime::start(
            directory.path().join("gnosis.sqlite"),
            directory.path().join("blobs"),
            1,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let (started, started_receiver) = mpsc::sync_channel(1);
        let (release, release_receiver) = mpsc::sync_channel(1);
        service
            .backup_sender
            .try_send(BackupCommand::Block {
                started,
                release: release_receiver,
            })
            .unwrap();
        tokio::task::spawn_blocking(move || started_receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let (response, backup_result) = oneshot::channel();
        service
            .sender
            .try_send(StoreCommand::CreateBackup {
                destination: backup.clone(),
                created_at_us: 1,
                response,
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match service.ready().await {
                    Err(ServiceError::Busy) => tokio::task::yield_now().await,
                    result => break result,
                }
            }
        })
        .await
        .expect("store actor accepted readiness after backup dispatch")
        .unwrap();
        drop(service);

        let started = std::time::Instant::now();
        let finish = runtime
            .finish_with_deadline(Duration::from_millis(50))
            .await;
        assert!(matches!(
            finish,
            Err(ServiceError::ShutdownDeadlineExceeded)
        ));
        assert!(started.elapsed() < Duration::from_secs(1));
        release.send(()).unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), backup_result)
                .await
                .unwrap()
                .unwrap(),
            Err(ServiceError::Backup(BackupError::Cancelled))
        ));
        assert!(!backup.exists());
        assert!(std::fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".gnosis-backup-")
        }));
    }

    #[tokio::test]
    async fn shutdown_cancels_accepted_verify_and_restore_validation() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let backup = directory.path().join("backup");
        service.create_backup(backup.clone(), 1).await.unwrap();
        let (started, started_receiver) = mpsc::sync_channel(1);
        let (release, release_receiver) = mpsc::sync_channel(1);
        service
            .backup_sender
            .try_send(BackupCommand::Block {
                started,
                release: release_receiver,
            })
            .unwrap();
        tokio::task::spawn_blocking(move || started_receiver.recv())
            .await
            .unwrap()
            .unwrap();
        let (verify_response, verify_result) = oneshot::channel();
        service
            .backup_sender
            .try_send(BackupCommand::Verify {
                path: backup.clone(),
                response: verify_response,
            })
            .unwrap();
        let restore = directory.path().join("restore-check");
        let (restore_response, restore_result) = oneshot::channel();
        service
            .backup_sender
            .try_send(BackupCommand::ValidateRestore {
                path: backup,
                temporary: restore.clone(),
                response: restore_response,
            })
            .unwrap();

        let shutdown = tokio::spawn(async move { runtime.finish().await });
        tokio::task::yield_now().await;
        release.send(()).unwrap();

        assert!(matches!(
            verify_result.await.unwrap(),
            Err(ServiceError::Backup(BackupError::Cancelled))
        ));
        assert!(matches!(
            restore_result.await.unwrap(),
            Err(ServiceError::Backup(BackupError::Cancelled))
        ));
        shutdown.await.unwrap().unwrap();
        assert!(!restore.exists());
    }

    #[tokio::test]
    async fn shutdown_finishes_while_a_stopped_service_handle_survives() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();

        runtime
            .finish_with_deadline(Duration::from_millis(250))
            .await
            .unwrap();

        assert!(matches!(service.ready().await, Err(ServiceError::Stopped)));
    }

    #[test]
    fn shutdown_completion_observed_after_deadline_is_rejected() {
        let started = Instant::now()
            .checked_sub(Duration::from_millis(20))
            .unwrap();

        assert!(matches!(
            complete_shutdown_phase(started, Duration::from_millis(10)),
            Err(ServiceError::ShutdownDeadlineExceeded)
        ));
    }

    #[tokio::test]
    async fn shutdown_poll_rechecks_deadline_after_wake() {
        let result = sleep_until_next_shutdown_poll(Instant::now(), Duration::from_millis(1)).await;

        assert!(matches!(
            result,
            Err(ServiceError::ShutdownDeadlineExceeded)
        ));
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
        let repair = service.repair(1).await.unwrap();
        assert_eq!(repair.blobs_verified, 1);
        assert_eq!(repair.max_batch_observed, 1);
        let backup = service
            .create_backup(directory.path().join("backup"), 5)
            .await
            .unwrap();
        assert_eq!(backup.blob_count, 1);
        drop(service);
        runtime.finish().await.unwrap();

        let runtime = ServiceRuntime::start(database, blobs.clone(), 8)
            .await
            .unwrap();
        let service = runtime.handle();
        let blob_path = blobs.join(&digest[..2]).join(&digest);
        std::fs::remove_file(&blob_path).unwrap();
        let stage_error = service
            .stage_blob(
                &digest,
                b"conflicted asset".to_vec(),
                "application/octet-stream".into(),
                0,
            )
            .await
            .unwrap_err();
        assert!(matches!(stage_error, ServiceError::Store(_)));
        assert!(blob_path.exists());
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
    async fn stored_asset_digest_mismatch_is_corruption() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("gnosis.sqlite");
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let first = b"first asset".to_vec();
        let second = b"second asset".to_vec();
        let first_digest = format!("{:x}", sha2::Sha256::digest(&first));
        let second_digest = format!("{:x}", sha2::Sha256::digest(&second));
        let first_path = canonical_asset_path(&first_digest, "first.png").unwrap();
        let second_path = canonical_asset_path(&second_digest, "second.png").unwrap();
        for (path, name, bytes) in [
            (first_path.clone(), "first.png", first),
            (second_path, "second.png", second),
        ] {
            let asset = prepare_asset(path, name.into(), None).unwrap();
            service
                .put_asset(asset, bytes, "image/png".into(), 10, None)
                .await
                .unwrap();
        }
        rusqlite::Connection::open(database)
            .unwrap()
            .execute(
                "UPDATE managed_asset SET blob_sha256 = ?1 WHERE asset_path = ?2",
                [&second_digest, &first_path],
            )
            .unwrap();

        let error = service.asset(first_path).await.unwrap_err();

        assert!(matches!(error, ServiceError::Corrupt));
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
    async fn rejected_document_imports_do_not_publish_blobs() {
        let directory = tempfile::tempdir().unwrap();
        let blob_root = directory.path().join("blobs");
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let accepted = ImportDocument::new("document", "#+TITLE: First\n".into(), None)
            .unwrap()
            .prepare_server()
            .unwrap();
        service.replace(accepted).await.unwrap();

        let stale_org = "#+TITLE: Stale\n";
        let stale_digest = format!("{:x}", sha2::Sha256::digest(stale_org.as_bytes()));
        for _ in 0..2 {
            let stale = ImportDocument::new("document", stale_org.into(), Some(0))
                .unwrap()
                .prepare_server()
                .unwrap();
            assert!(matches!(
                service.replace(stale).await,
                Err(ServiceError::Application(
                    crate::application::ApplicationError::Conflict { .. }
                ))
            ));
            assert!(
                !blob_root
                    .join(&stale_digest[..2])
                    .join(&stale_digest)
                    .exists()
            );
        }

        let missing_asset_org = "#+TITLE: Missing asset\n";
        let missing_asset_digest =
            format!("{:x}", sha2::Sha256::digest(missing_asset_org.as_bytes()));
        let mut missing_asset =
            ImportDocument::new("missing-asset", missing_asset_org.into(), None)
                .unwrap()
                .prepare_server()
                .unwrap();
        missing_asset.asset_paths.push("assets/missing.png".into());
        assert!(service.replace(missing_asset).await.is_err());
        assert!(
            !blob_root
                .join(&missing_asset_digest[..2])
                .join(&missing_asset_digest)
                .exists()
        );

        drop(service);
        runtime.finish().await.unwrap();
    }

    #[tokio::test]
    async fn document_imports_enter_the_replica_feed_without_changing_the_response() {
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
    async fn settings_updates_are_revisioned() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = runtime(&directory).await;
        let service = runtime.handle();
        let initial = service.settings().await.unwrap();
        assert_eq!(initial.timezone, "Europe/Athens");
        let input = SettingsInput {
            timezone: "Europe/Sofia".into(),
            day_boundary_hour: 4,
            desired_retention: 0.9,
        };
        let updated = service.update_settings(input, 1, 10, None).await.unwrap();
        assert_eq!(updated.revision, 2);
        assert_eq!(updated.timezone, "Europe/Sofia");
        assert_eq!(service.latest_content_cursor().await.unwrap(), 1);
        drop(service);
        runtime.finish().await.unwrap();
    }
}
