//! Explicit review-pack acquisition for offline clients.

use thiserror::Error;

use crate::{
    client_http::{HttpClient, HttpClientError},
    client_replica::{Error as LocalReplicaError, LocalReplica},
    protocol::http_v1::{ReviewPackAssetV2, ReviewPackEnvelopeV2, ReviewSessionRequestV1},
};

pub struct ReviewPackAcquisition<'a> {
    client: &'a HttpClient,
}

#[derive(Debug, Error)]
pub enum ReviewPackAcquisitionError {
    #[error(transparent)]
    Http(#[from] HttpClientError),
    #[error(transparent)]
    Replica(#[from] LocalReplicaError),
    #[error("stored review pack asset has an inconsistent size")]
    AssetSize,
}

impl<'a> ReviewPackAcquisition<'a> {
    pub const fn new(client: &'a HttpClient) -> Self {
        Self { client }
    }

    pub async fn create(
        &self,
        replica: &mut LocalReplica,
        input: &ReviewSessionRequestV1,
    ) -> Result<String, ReviewPackAcquisitionError> {
        let response = self.client.create_review_pack(input).await?;
        self.install(replica, response).await
    }

    pub async fn resume_active(
        &self,
        replica: &mut LocalReplica,
    ) -> Result<Option<String>, ReviewPackAcquisitionError> {
        let response = self.client.active_review_pack().await?;
        let Some(pack) = response.pack else {
            return Ok(None);
        };
        self.install(
            replica,
            ReviewPackEnvelopeV2 {
                version: response.version,
                pack,
            },
        )
        .await
        .map(Some)
    }

    pub async fn acquire(
        &self,
        replica: &mut LocalReplica,
        pack_id: &str,
    ) -> Result<String, ReviewPackAcquisitionError> {
        let response = self.client.review_pack(pack_id).await?;
        self.install(replica, response).await
    }

    pub async fn abandon(
        &self,
        replica: &mut LocalReplica,
        pack_id: &str,
    ) -> Result<String, ReviewPackAcquisitionError> {
        let response = self.client.abandon_review_pack(pack_id).await?;
        Ok(replica.install_review_pack(response)?)
    }

    async fn install(
        &self,
        replica: &mut LocalReplica,
        response: ReviewPackEnvelopeV2,
    ) -> Result<String, ReviewPackAcquisitionError> {
        let assets = response.pack.assets.clone();
        let pack_id = replica.install_review_pack(response)?;
        for asset in assets {
            self.install_asset(replica, &pack_id, &asset).await?;
        }
        Ok(pack_id)
    }

    async fn install_asset(
        &self,
        replica: &mut LocalReplica,
        pack_id: &str,
        asset: &ReviewPackAssetV2,
    ) -> Result<(), ReviewPackAcquisitionError> {
        if let Some(bytes) = replica.blob(&asset.content_sha256)? {
            return require_size(&bytes, asset.size_bytes);
        }
        let bytes = self.client.review_pack_asset(pack_id, asset).await?;
        replica.install_blob(&asset.content_sha256, &bytes)?;
        Ok(())
    }
}

fn require_size(bytes: &[u8], expected: u64) -> Result<(), ReviewPackAcquisitionError> {
    let actual = u64::try_from(bytes.len()).map_err(|_| ReviewPackAcquisitionError::AssetSize)?;
    (actual == expected)
        .then_some(())
        .ok_or(ReviewPackAcquisitionError::AssetSize)
}
