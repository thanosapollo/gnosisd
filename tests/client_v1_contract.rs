use gnosis_core::protocol::client_v1::{
    OfflineReviewAnswerReceiptV1, OfflineReviewAnswerRequestV1, OfflineReviewQuestionV1,
    ReviewPackAcquireRequestV1, ReviewPackCurrentV1, ReviewPackReceiptV1,
};
use serde_json::{Value, json};

#[test]
fn offline_question_is_closed_and_requires_version_one() {
    let value = question();
    let decoded: OfflineReviewQuestionV1 = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);

    let mut wrong_version = value.clone();
    wrong_version["version"] = json!(2);
    assert!(serde_json::from_value::<OfflineReviewQuestionV1>(wrong_version).is_err());

    let mut unknown = value;
    unknown["answer"] = json!(["hidden"]);
    assert!(serde_json::from_value::<OfflineReviewQuestionV1>(unknown).is_err());
}

#[test]
fn review_pack_acquisition_request_is_closed_and_versioned() {
    let value = json!({
        "version": 1,
        "session": {
            "source": {
                "kind": "manual",
                "tags": [],
                "exclude_tags": [],
                "topic_node_id": null,
                "topic_forward_depth": null,
                "topic_backward_depth": null,
                "manual_ids": ["thema"]
            },
            "target_count": null
        }
    });
    let decoded: ReviewPackAcquireRequestV1 = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);

    assert_rejects_wrong_version::<ReviewPackAcquireRequestV1>(&value);
    assert_rejects_unknown_field::<ReviewPackAcquireRequestV1>(&value);
}

#[test]
fn offline_answer_request_requires_explicit_nullable_override() {
    let value = answer_request();
    let decoded: OfflineReviewAnswerRequestV1 = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);

    let mut missing_override = value.clone();
    missing_override
        .as_object_mut()
        .unwrap()
        .remove("accepted_success");
    assert!(serde_json::from_value::<OfflineReviewAnswerRequestV1>(missing_override).is_err());
    assert_rejects_wrong_version::<OfflineReviewAnswerRequestV1>(&value);
    assert_rejects_unknown_field::<OfflineReviewAnswerRequestV1>(&value);
}

#[test]
fn offline_review_cli_responses_are_closed_and_versioned() {
    let current = json!({"version": 1, "question": question()});
    let receipt = json!({
        "version": 1,
        "attempt_id": "attempt",
        "device_sequence": 1,
        "question": null
    });
    let pack = json!({"version": 1, "pack_id": "pack"});

    assert_round_trip::<ReviewPackCurrentV1>(&current);
    assert_round_trip::<OfflineReviewAnswerReceiptV1>(&receipt);
    assert_round_trip::<ReviewPackReceiptV1>(&pack);
    assert_rejects_wrong_version::<ReviewPackCurrentV1>(&current);
    assert_rejects_wrong_version::<OfflineReviewAnswerReceiptV1>(&receipt);
    assert_rejects_wrong_version::<ReviewPackReceiptV1>(&pack);
}

fn answer_request() -> Value {
    json!({
        "version": 1,
        "pack_id": "pack",
        "attempt_id": "attempt",
        "session_position": 0,
        "response": {"kind": "text", "value": "Answer"},
        "accepted_success": null,
        "desired_suspended": false,
        "answered_at_us": 2,
        "fuzz_sample": 0.0
    })
}

fn assert_round_trip<T>(value: &Value)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let decoded: T = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), *value);
}

fn assert_rejects_wrong_version<T>(value: &Value)
where
    T: serde::de::DeserializeOwned,
{
    let mut invalid = value.clone();
    invalid["version"] = json!(2);
    assert!(serde_json::from_value::<T>(invalid).is_err());
}

fn assert_rejects_unknown_field<T>(value: &Value)
where
    T: serde::de::DeserializeOwned,
{
    let mut invalid = value.clone();
    invalid["unknown"] = json!(true);
    assert!(serde_json::from_value::<T>(invalid).is_err());
}

fn question() -> Value {
    json!({
        "version": 1,
        "pack_id": "pack",
        "session_id": "session",
        "session_position": 0,
        "queue_length": 1,
        "thema_id": "thema",
        "thema_revision": 1,
        "content_sha256": "a".repeat(64),
        "presentation": {
            "kind": "basic",
            "question": "Question",
            "response": {"kind": "text", "input_script": null},
            "source_guid": null,
            "review_image": null,
            "tags": []
        },
        "asset_paths": []
    })
}
