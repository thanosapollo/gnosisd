use gnosis_core::protocol::client_v1::OfflineReviewQuestionV1;
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
