//! Explicit review-pack acquisition for offline clients.

use thiserror::Error;

use crate::{
    application::PresentationSnapshot,
    client_http::{HttpClient, HttpClientError},
    client_replica::{
        CapturedPresentation, Error as LocalReplicaError, LocalReplica, StoredOfflineReviewPack,
    },
    protocol::{
        client_v1::OfflineReviewQuestionV1,
        http_v1::{
            ReviewPackAssetV2, ReviewPackEnvelopeV2, ReviewPresentationV1, ReviewSessionRequestV1,
        },
    },
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

pub fn active_question(
    replica: &LocalReplica,
) -> Result<Option<OfflineReviewQuestionV1>, LocalReplicaError> {
    let Some(pack) = replica.active_review_pack()? else {
        return Ok(None);
    };
    current_question(&pack)
}

fn current_question(
    pack: &StoredOfflineReviewPack,
) -> Result<Option<OfflineReviewQuestionV1>, LocalReplicaError> {
    if pack.status == "completed" || pack.status == "abandoned" {
        return Ok(None);
    }
    if pack.status != "active" {
        return Err(LocalReplicaError::Response);
    }
    let item = pack
        .items
        .get(pack.local_position as usize)
        .ok_or(LocalReplicaError::Response)?;
    if item.position != pack.local_position {
        return Err(LocalReplicaError::Response);
    }
    let queue_length = u32::try_from(pack.items.len()).map_err(|_| LocalReplicaError::Response)?;
    let presentation = ReviewPresentationV1::from_snapshot(&snapshot(&item.presentation))
        .map_err(|_| LocalReplicaError::Response)?;
    Ok(Some(OfflineReviewQuestionV1 {
        version: 1,
        pack_id: pack.pack_id.clone(),
        session_id: pack.session_id.clone(),
        session_position: item.position,
        queue_length,
        thema_id: item.thema_id.clone(),
        thema_revision: item.thema_revision,
        content_sha256: item.content_sha256.clone(),
        presentation,
        asset_paths: item.asset_paths.clone(),
    }))
}

fn snapshot(value: &CapturedPresentation) -> PresentationSnapshot {
    PresentationSnapshot {
        kind: value.kind.clone(),
        keimenon: value.keimenon.clone(),
        hypothesis: value.hypothesis.clone(),
        answer: value.answer.clone(),
        source_guid: value.source_guid.clone(),
        parathema: value.parathema.clone(),
        review_image: value.review_image.clone(),
        tags: value.tags.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::current_question;
    use crate::client_replica::{
        CapturedPresentation, StoredOfflineReviewItem, StoredOfflineReviewPack,
    };

    #[test]
    fn safe_questions_never_serialize_captured_answers() {
        let cases = [
            presentation("basic", "Question", vec![], vec!["Secret"]),
            presentation(
                "cloze",
                "The concealed phrase is Secret",
                vec![Some("hint")],
                vec!["Secret"],
            ),
        ];
        for presentation in cases {
            let question = current_question(&pack("active", 0, presentation))
                .unwrap()
                .unwrap();
            let json = serde_json::to_value(question).unwrap();
            assert_no_answer_field(&json);
            assert!(!json.to_string().contains("Secret"));
        }
    }

    #[test]
    fn safe_multiple_choice_question_exposes_choices_without_correctness() {
        let presentation = presentation(
            "mcq",
            "Choose one",
            vec![Some("Alpha"), Some("Beta")],
            vec!["Beta"],
        );

        let question = current_question(&pack("active", 0, presentation))
            .unwrap()
            .unwrap();
        let json = serde_json::to_value(question).unwrap();

        assert_no_answer_field(&json);
        assert_eq!(
            json["presentation"]["response"]["choices"],
            serde_json::json!(["Alpha", "Beta"])
        );
        assert!(json.get("detected_success").is_none());
    }

    #[test]
    fn inactive_or_finished_pack_has_no_current_question() {
        let presentation = presentation("basic", "Question", vec![], vec!["Answer"]);

        assert!(
            current_question(&pack("completed", 1, presentation.clone()))
                .unwrap()
                .is_none()
        );
        assert!(
            current_question(&pack("abandoned", 0, presentation))
                .unwrap()
                .is_none()
        );
    }

    fn pack(
        status: &str,
        local_position: u32,
        presentation: CapturedPresentation,
    ) -> StoredOfflineReviewPack {
        StoredOfflineReviewPack {
            pack_id: "pack".into(),
            session_id: "session".into(),
            status: status.into(),
            local_position,
            issued_at_us: 1,
            timezone: "UTC".into(),
            day_boundary_hour: 0,
            items: vec![StoredOfflineReviewItem {
                position: 0,
                thema_id: "thema".into(),
                thema_revision: 1,
                content_sha256: "a".repeat(64),
                presentation,
                asset_paths: vec!["assets/a/image.png".into()],
            }],
            assets: vec![],
        }
    }

    fn presentation(
        kind: &str,
        keimenon: &str,
        hypothesis: Vec<Option<&str>>,
        answer: Vec<&str>,
    ) -> CapturedPresentation {
        CapturedPresentation {
            kind: kind.into(),
            keimenon: keimenon.into(),
            hypothesis: hypothesis
                .into_iter()
                .map(|value| value.map(str::to_owned))
                .collect(),
            answer: answer.into_iter().map(str::to_owned).collect(),
            source_guid: None,
            parathema: None,
            review_image: None,
            tags: vec![],
        }
    }

    fn assert_no_answer_field(value: &Value) {
        match value {
            Value::Object(object) => {
                assert!(!object.contains_key("answer"));
                object.values().for_each(assert_no_answer_field);
            }
            Value::Array(values) => values.iter().for_each(assert_no_answer_field),
            _ => {}
        }
    }
}
