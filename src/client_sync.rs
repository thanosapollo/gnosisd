//! Durable synchronization between the authenticated HTTP API and a local replica.

use std::fmt;

use serde::Serialize;
use thiserror::Error;

use crate::{
    client_http::{DeliveryStatus, HttpClient, HttpClientError},
    client_replica::{
        DeviceUploadKind, Error as LocalReplicaError, LocalReplica, PendingDeviceUpload,
    },
    protocol::http_v1::ApiErrorCodeV1,
};

pub struct ReplicaSync<'a> {
    client: &'a HttpClient,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct ReplicaSyncReport {
    pub snapshot_pages: u64,
    pub snapshot_recoveries: u64,
    pub change_pages: u64,
    pub downloaded_blobs: u64,
    pub staged_upload_blobs: u64,
    pub uploaded_operations: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicaSyncOperation {
    pub kind: DeviceUploadKind,
    pub operation_id: String,
    pub device_sequence: u64,
}

impl fmt::Display for ReplicaSyncOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:?} operation {} at device sequence {}",
            self.kind, self.operation_id, self.device_sequence
        )
    }
}

#[derive(Debug, Error)]
pub enum ReplicaSyncError {
    #[error(transparent)]
    Http(#[from] HttpClientError),
    #[error(transparent)]
    Replica(#[from] LocalReplicaError),
    #[error("authenticated device {authenticated} does not match replica device {replica}")]
    DeviceIdentity {
        authenticated: String,
        replica: String,
    },
    #[error("failed to stage asset for {operation} ({delivery} delivery)")]
    StageAsset {
        operation: ReplicaSyncOperation,
        delivery: DeliveryStatus,
        #[source]
        source: HttpClientError,
    },
    #[error("failed to upload {operation} ({delivery} delivery)")]
    Upload {
        operation: ReplicaSyncOperation,
        delivery: DeliveryStatus,
        #[source]
        source: HttpClientError,
    },
    #[error("failed to record the server result for {operation}")]
    RecordResult {
        operation: ReplicaSyncOperation,
        #[source]
        source: LocalReplicaError,
    },
    #[error("synchronized replica has no durable cursor")]
    MissingCursor,
}

impl<'a> ReplicaSync<'a> {
    pub const fn new(client: &'a HttpClient) -> Self {
        Self { client }
    }

    pub async fn synchronize(
        &self,
        replica: &mut LocalReplica,
    ) -> Result<ReplicaSyncReport, ReplicaSyncError> {
        let mut report = ReplicaSyncReport::default();
        self.verify_device(replica).await?;
        self.observe_server(replica).await?;
        self.pull_server(replica, &mut report).await?;
        self.fetch_missing_blobs(replica, &mut report).await?;
        self.drain_outbox(replica, &mut report).await?;
        if report.uploaded_operations > 0 {
            self.pull_server(replica, &mut report).await?;
            self.fetch_missing_blobs(replica, &mut report).await?;
        }
        Ok(report)
    }

    async fn verify_device(&self, replica: &LocalReplica) -> Result<(), ReplicaSyncError> {
        let authenticated = self.client.device_identity().await?.device_id;
        let local = replica.device_id()?;
        if authenticated == local {
            Ok(())
        } else {
            Err(ReplicaSyncError::DeviceIdentity {
                authenticated,
                replica: local,
            })
        }
    }

    async fn observe_server(&self, replica: &mut LocalReplica) -> Result<(), ReplicaSyncError> {
        replica.observe_state(self.client.replica_state().await?)?;
        replica.install_content_capabilities(self.client.content_capabilities().await?)?;
        replica.install_asset_capabilities(self.client.asset_capabilities().await?)?;
        Ok(())
    }

    async fn pull_server(
        &self,
        replica: &mut LocalReplica,
        report: &mut ReplicaSyncReport,
    ) -> Result<(), ReplicaSyncError> {
        let result = self.pull_from_status(replica, report).await;
        match result {
            Ok(()) => Ok(()),
            Err(error) if snapshot_required(&error) => {
                report.snapshot_recoveries += 1;
                self.download_snapshot(replica, None, report).await?;
                self.pull_changes(replica, report).await
            }
            Err(error) => Err(error),
        }
    }

    async fn pull_from_status(
        &self,
        replica: &mut LocalReplica,
        report: &mut ReplicaSyncReport,
    ) -> Result<(), ReplicaSyncError> {
        let status = replica.status()?;
        if let Some(next) = status.snapshot_next {
            self.download_snapshot(replica, Some(next), report).await?;
        } else if status.server_id != status.observed_server_id || status.cursor.is_none() {
            self.download_snapshot(replica, None, report).await?;
        }
        self.pull_changes(replica, report).await
    }

    async fn download_snapshot(
        &self,
        replica: &mut LocalReplica,
        mut requested: Option<String>,
        report: &mut ReplicaSyncReport,
    ) -> Result<(), ReplicaSyncError> {
        loop {
            let page = self.client.snapshot(requested.as_deref()).await?;
            replica.install_snapshot_page(requested.as_deref(), page)?;
            report.snapshot_pages += 1;
            requested = replica.status()?.snapshot_next;
            if requested.is_none() {
                return Ok(());
            }
        }
    }

    async fn pull_changes(
        &self,
        replica: &mut LocalReplica,
        report: &mut ReplicaSyncReport,
    ) -> Result<(), ReplicaSyncError> {
        loop {
            let cursor = replica
                .status()?
                .cursor
                .ok_or(ReplicaSyncError::MissingCursor)?;
            let has_more = replica.install_change_page(self.client.changes(&cursor).await?)?;
            report.change_pages += 1;
            if !has_more {
                return Ok(());
            }
        }
    }

    async fn fetch_missing_blobs(
        &self,
        replica: &mut LocalReplica,
        report: &mut ReplicaSyncReport,
    ) -> Result<(), ReplicaSyncError> {
        for digest in replica.missing_projection_blobs()? {
            let bytes = self.client.blob(&digest).await?;
            replica.install_blob(&digest, &bytes)?;
            report.downloaded_blobs += 1;
        }
        Ok(())
    }

    async fn drain_outbox(
        &self,
        replica: &mut LocalReplica,
        report: &mut ReplicaSyncReport,
    ) -> Result<(), ReplicaSyncError> {
        while let Some(upload) = replica.next_device_upload()? {
            let operation = ReplicaSyncOperation::from(&upload);
            self.stage_asset(&upload, report).await.map_err(|source| {
                ReplicaSyncError::StageAsset {
                    operation: operation.clone(),
                    delivery: source.delivery(),
                    source,
                }
            })?;
            let result =
                self.client
                    .upload(&upload)
                    .await
                    .map_err(|source| ReplicaSyncError::Upload {
                        operation: operation.clone(),
                        delivery: source.delivery(),
                        source,
                    })?;
            replica
                .record_device_result(result)
                .map_err(|source| ReplicaSyncError::RecordResult { operation, source })?;
            report.uploaded_operations += 1;
        }
        Ok(())
    }

    async fn stage_asset(
        &self,
        upload: &PendingDeviceUpload,
        report: &mut ReplicaSyncReport,
    ) -> Result<(), HttpClientError> {
        if let PendingDeviceUpload::Asset(asset) = upload {
            self.client.stage_blob(asset).await?;
            report.staged_upload_blobs += 1;
        }
        Ok(())
    }
}

impl From<&PendingDeviceUpload> for ReplicaSyncOperation {
    fn from(value: &PendingDeviceUpload) -> Self {
        Self {
            kind: value.kind(),
            operation_id: value.operation_id().to_owned(),
            device_sequence: value.device_sequence(),
        }
    }
}

fn snapshot_required(error: &ReplicaSyncError) -> bool {
    matches!(
        error,
        ReplicaSyncError::Http(HttpClientError::Server(server))
            if server.body().error.code() == ApiErrorCodeV1::SnapshotRequired
    )
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use sha2::{Digest as _, Sha256};
    use tokio::sync::oneshot;

    use super::ReplicaSync;
    use crate::{
        application::{canonical_asset_path, generate_device, prepare_asset},
        client_http::{HttpClient, SecretToken},
        client_replica::LocalReplica,
        http::router,
        protocol::http_v1::ReviewPackEnvelopeV2,
        service::{ServiceHandle, ServiceRuntime},
    };

    #[tokio::test]
    async fn periodic_sync_deduplicates_projections_and_skips_pack_assets() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = ServiceRuntime::start(
            directory.path().join("server.sqlite"),
            directory.path().join("server-blobs"),
            8,
        )
        .await
        .unwrap();
        let service = runtime.handle();
        let device = generate_device(
            "offline".into(),
            vec!["read".into(), "review".into(), "content:write".into()],
        )
        .unwrap();
        let device_id = device.device_id.clone();
        let token = device.token.clone();
        service.create_device(device, 1).await.unwrap();
        add_duplicate_blob_projections(&service, &device_id).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let (shutdown, receiver) = oneshot::channel();
        let server = tokio::spawn(async move {
            axum::serve(listener, router(service.clone()))
                .with_graceful_shutdown(async {
                    let _ = receiver.await;
                })
                .await
                .unwrap();
            service
        });
        let client = HttpClient::new(&base_url, SecretToken::new(token).unwrap()).unwrap();
        let mut replica =
            LocalReplica::open(directory.path().join("replica.sqlite"), &device_id).unwrap();
        let bootstrap = ReplicaSync::new(&client)
            .synchronize(&mut replica)
            .await
            .unwrap();
        assert_eq!(bootstrap.downloaded_blobs, 1);

        let digest = format!("{:x}", Sha256::digest(b"pack-only"));
        let state = client.replica_state().await.unwrap();
        replica
            .install_review_pack(pack_only_asset(&device_id, &state.cursor, &digest))
            .unwrap();
        let report = ReplicaSync::new(&client)
            .synchronize(&mut replica)
            .await
            .unwrap();
        assert_eq!(report.downloaded_blobs, 0);
        assert!(replica.blob(&digest).unwrap().is_none());

        shutdown.send(()).unwrap();
        let service = server.await.unwrap();
        drop(service);
        runtime.finish().await.unwrap();
    }

    async fn add_duplicate_blob_projections(service: &ServiceHandle, device_id: &str) {
        let bytes = b"shared projection blob";
        let digest = format!("{:x}", Sha256::digest(bytes));
        for (offset, name) in ["first.png", "second.png"].into_iter().enumerate() {
            let path = canonical_asset_path(&digest, name).unwrap();
            let prepared = prepare_asset(path, name.into(), None).unwrap();
            service
                .put_asset(
                    prepared,
                    bytes.to_vec(),
                    "image/png".into(),
                    1_752_600_000_000_000 + offset as i64,
                    Some(device_id.into()),
                )
                .await
                .unwrap();
        }
    }

    fn pack_only_asset(device_id: &str, cursor: &str, digest: &str) -> ReviewPackEnvelopeV2 {
        let mut fixture: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/protocol/offline-review-v1.json"
        ))
        .unwrap();
        fixture["pack"]["pack"]["device_id"] = json!(device_id);
        fixture["pack"]["pack"]["cursor"] = json!(cursor);
        fixture["pack"]["pack"]["assets"] = json!([{
            "logical_path": format!("assets/{digest}/image.png"),
            "content_sha256": digest,
            "size_bytes": 9,
            "media_type": "image/png"
        }]);
        serde_json::from_value(fixture["pack"].clone()).unwrap()
    }
}
