use gnosis_core::protocol::client_v1::{
    ClientDeliveryV1, ClientFailureCodeV1, ClientFailureV1, OfflineReviewAnswerReceiptV1,
    OfflineReviewAnswerRequestV1, OfflineReviewAssetRequestV1, OfflineReviewAssetV1,
    OfflineReviewConflictReasonV1, OfflineReviewQuestionV1, OfflineReviewResultPageRequestV1,
    OfflineReviewResultPageV1, ReviewPackAcquireRequestV1, ReviewPackCurrentV1,
    ReviewPackReceiptV1,
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
fn readme_offline_answer_matches_the_fsrs_contract() {
    let readme = include_str!("../README.org");
    let documented = readme
        .split_once("#+name: offline-answer-v1\n#+begin_src json\n")
        .unwrap()
        .1
        .split_once("\n#+end_src")
        .unwrap()
        .0;

    let decoded: OfflineReviewAnswerRequestV1 = serde_json::from_str(documented).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), answer_request());
    for obsolete in [
        "fuzz sample",
        "interval fuzz",
        "Amboss",
        "Bulgarian",
        "Unking",
        "vocabulary rules",
    ] {
        assert!(
            !readme.contains(obsolete),
            "stale scheduler term: {obsolete}"
        );
    }
}

#[test]
fn readme_storage_versions_match_the_canonical_schemas() {
    let readme = include_str!("../README.org");
    assert!(readme.contains("server's canonical schema v3"));
    assert!(readme.contains("replica database retains its distinct canonical schema v1"));
    assert!(include_str!("../src/store/sqlite.rs").contains("const SCHEMA_VERSION: u32 = 3;"));
    assert!(include_str!("../src/store/replica.rs").contains("const SCHEMA_VERSION: u32 = 1;"));
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

#[test]
fn offline_asset_contract_is_closed_and_content_addressed() {
    let request = json!({
        "version": 1,
        "pack_id": "pack",
        "logical_path": "assets/abc/image.png"
    });
    let response = json!({
        "version": 1,
        "pack_id": "pack",
        "logical_path": "assets/abc/image.png",
        "content_sha256": "a".repeat(64),
        "size_bytes": 3,
        "media_type": "image/png",
        "data_base64": "YWJj"
    });

    assert_round_trip::<OfflineReviewAssetRequestV1>(&request);
    assert_round_trip::<OfflineReviewAssetV1>(&response);
    assert_rejects_wrong_version::<OfflineReviewAssetRequestV1>(&request);
    assert_rejects_wrong_version::<OfflineReviewAssetV1>(&response);
    assert_rejects_unknown_field::<OfflineReviewAssetRequestV1>(&request);
    assert_rejects_unknown_field::<OfflineReviewAssetV1>(&response);
}

#[test]
fn review_result_pages_exclude_answer_bearing_operations() {
    let request = json!({
        "version": 1,
        "after": null,
        "limit": 50,
        "conflicts_only": true
    });
    let page = json!({
        "version": 1,
        "items": [{
            "status": "conflict",
            "result_sequence": 2,
            "server_id": "a".repeat(32),
            "attempt_id": "attempt",
            "device_sequence": 4,
            "conflict_reason": "content_changed"
        }],
        "next_after": null
    });

    assert_round_trip::<OfflineReviewResultPageRequestV1>(&request);
    let decoded: OfflineReviewResultPageV1 = serde_json::from_value(page.clone()).unwrap();
    assert_eq!(
        decoded.items[0].conflict_reason(),
        Some(OfflineReviewConflictReasonV1::ContentChanged)
    );
    assert_eq!(serde_json::to_value(decoded).unwrap(), page);
    assert!(!page.to_string().contains("presentation"));
    assert!(!page.to_string().contains("answer"));
    assert_rejects_unknown_field::<OfflineReviewResultPageRequestV1>(&request);
    assert_rejects_wrong_version::<OfflineReviewResultPageV1>(&page);

    let mut boundary = request.clone();
    boundary["after"] = json!(i64::MAX as u64);
    assert_round_trip::<OfflineReviewResultPageRequestV1>(&boundary);
    boundary["after"] = json!(i64::MAX as u64 + 1);
    assert!(serde_json::from_value::<OfflineReviewResultPageRequestV1>(boundary).is_err());
    for limit in [0, 101] {
        let mut invalid = request.clone();
        invalid["limit"] = json!(limit);
        assert!(serde_json::from_value::<OfflineReviewResultPageRequestV1>(invalid).is_err());
    }
}

#[test]
fn client_failures_are_closed_and_machine_readable() {
    let value = json!({
        "version": 1,
        "code": "replica",
        "delivery": "not_sent",
        "operation_id": null,
        "api_error": null
    });
    let decoded: ClientFailureV1 = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(decoded.code, ClientFailureCodeV1::Replica);
    assert_eq!(decoded.delivery, ClientDeliveryV1::NotSent);
    assert_eq!(serde_json::to_value(decoded).unwrap(), value);
    assert_rejects_wrong_version::<ClientFailureV1>(&value);
    assert_rejects_unknown_field::<ClientFailureV1>(&value);

    let mut missing_null = value.clone();
    missing_null.as_object_mut().unwrap().remove("api_error");
    assert!(serde_json::from_value::<ClientFailureV1>(missing_null).is_err());

    let inconsistent = json!({
        "version": 1,
        "code": "server",
        "delivery": "unknown",
        "operation_id": null,
        "api_error": null
    });
    assert!(serde_json::from_value::<ClientFailureV1>(inconsistent).is_err());

    let uncertain_server = json!({
        "version": 1,
        "code": "server",
        "delivery": "unknown",
        "operation_id": "operation",
        "api_error": {"error": {
            "code": "request_timeout",
            "message": "request outcome is unknown",
            "meta": null
        }}
    });
    assert_round_trip::<ClientFailureV1>(&uncertain_server);

    for operation_id in ["", "x/invalid", "xφ"] {
        let mut invalid = value.clone();
        invalid["operation_id"] = json!(operation_id);
        assert!(serde_json::from_value::<ClientFailureV1>(invalid).is_err());
    }
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
        "answered_at_us": 2
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
            "hint": null,
            "response": {"kind": "text", "input_script": null},
            "source_guid": null,
            "review_image": null,
            "tags": []
        },
        "asset_paths": []
    })
}
