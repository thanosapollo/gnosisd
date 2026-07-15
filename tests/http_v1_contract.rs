use gnosis_core::protocol::http_v1::{
    ApiErrorCodeV1, ApiErrorResponseV1, AssetCapabilitiesV1, ContentCapabilitiesV1,
    ReplicaChangePageV1, ReplicaSnapshotPageV1, ReplicaStateV1, ReviewAttemptOperationResponseV1,
    ReviewAttemptStateResponseV1, SetAcceptedSuccessRequestV1, SetSuspensionRequestV1,
};
use serde_json::{Value, json};

fn fixture() -> Value {
    serde_json::from_str(include_str!("fixtures/protocol/replica-sync-v1.json")).unwrap()
}

#[test]
fn public_replica_dtos_preserve_the_v1_fixture() {
    let fixture = fixture();
    let state: ReplicaStateV1 = serde_json::from_value(fixture["state"].clone()).unwrap();
    let snapshot: ReplicaSnapshotPageV1 =
        serde_json::from_value(fixture["snapshot_partial"].clone()).unwrap();
    let changes: ReplicaChangePageV1 = serde_json::from_value(fixture["changes"].clone()).unwrap();

    assert_eq!(state.next_device_sequence, 3);
    assert_eq!(snapshot.items.len(), 2);
    assert_eq!(changes.items.len(), 3);
    assert_eq!(serde_json::to_value(state).unwrap(), fixture["state"]);
    assert_eq!(
        serde_json::to_value(snapshot).unwrap(),
        fixture["snapshot_partial"]
    );
    assert_eq!(serde_json::to_value(changes).unwrap(), fixture["changes"]);
}

#[test]
fn replica_projection_envelopes_reject_unknown_fields() {
    let mut snapshot = fixture()["snapshot_partial"].clone();
    snapshot["items"][0]["future"] = json!(true);

    assert!(serde_json::from_value::<ReplicaSnapshotPageV1>(snapshot).is_err());
}

#[test]
fn snapshot_pages_must_continue_or_finish_exclusively() {
    let fixture = fixture();
    let mut both = fixture["snapshot_partial"].clone();
    both["cursor"] = fixture["snapshot_terminal"]["cursor"].clone();
    assert!(serde_json::from_value::<ReplicaSnapshotPageV1>(both).is_err());

    let mut neither = fixture["snapshot_partial"].clone();
    neither["next"] = Value::Null;
    assert!(serde_json::from_value::<ReplicaSnapshotPageV1>(neither).is_err());

    let mut missing = fixture["snapshot_partial"].clone();
    missing.as_object_mut().unwrap().remove("cursor");
    assert!(serde_json::from_value::<ReplicaSnapshotPageV1>(missing).is_err());
}

#[test]
fn replica_state_accepts_equal_sequence_aliases_and_emits_legacy_wire_name() {
    let mut state = fixture()["state"].clone();
    state["next_device_sequence"] = json!(3);

    let state: ReplicaStateV1 = serde_json::from_value(state).unwrap();
    let wire = serde_json::to_value(state).unwrap();

    assert_eq!(wire["next_review_sequence"], 3);
    assert!(wire.get("next_device_sequence").is_none());
}

#[test]
fn replica_state_rejects_inconsistent_sequence_aliases() {
    let mut state = fixture()["state"].clone();
    state["next_device_sequence"] = json!(4);

    assert!(serde_json::from_value::<ReplicaStateV1>(state).is_err());
}

#[test]
fn public_capability_and_error_dtos_preserve_http_shapes() {
    let content_wire = json!({
        "version":1,
        "server_id":"a".repeat(32),
        "document_lifecycle":1
    });
    let asset_wire = json!({
        "version":1,
        "server_id":"a".repeat(32),
        "put_asset":1,
        "max_blob_bytes":1048576
    });
    let error_wire = json!({"error":{
        "code":"snapshot_required",
        "message":"replica cursor cannot be resumed from this server state",
        "meta":{"snapshot_url":"/v1/sync/snapshot"}
    }});

    let content: ContentCapabilitiesV1 = serde_json::from_value(content_wire.clone()).unwrap();
    let asset: AssetCapabilitiesV1 = serde_json::from_value(asset_wire.clone()).unwrap();
    let response: ApiErrorResponseV1 = serde_json::from_value(error_wire.clone()).unwrap();

    assert_eq!(response.error.code(), ApiErrorCodeV1::SnapshotRequired);
    assert_eq!(serde_json::to_value(content).unwrap(), content_wire);
    assert_eq!(serde_json::to_value(asset).unwrap(), asset_wire);
    assert_eq!(serde_json::to_value(response).unwrap(), error_wire);

    let mut future = error_wire;
    future["error"]["meta"]["future"] = json!(true);
    assert!(serde_json::from_value::<ApiErrorResponseV1>(future).is_err());

    let mismatched = json!({"error":{
        "code":"unauthorized",
        "message":"no",
        "meta":{"snapshot_url":"/v1/sync/snapshot"}
    }});
    assert!(serde_json::from_value::<ApiErrorResponseV1>(mismatched).is_err());
}

#[test]
fn public_review_operation_dtos_preserve_strict_v1_shapes() {
    let acceptance = json!({
        "version":1,
        "operation_id":"operation",
        "expected_attempt_revision":2,
        "accepted_success":false
    });
    let suspension = json!({
        "version":1,
        "operation_id":"suspension",
        "expected_attempt_revision":3,
        "desired_suspended":true
    });
    let result = review_attempt_result();

    let acceptance_dto: SetAcceptedSuccessRequestV1 =
        serde_json::from_value(acceptance.clone()).unwrap();
    let suspension_dto: SetSuspensionRequestV1 =
        serde_json::from_value(suspension.clone()).unwrap();
    let result_dto: ReviewAttemptOperationResponseV1 =
        serde_json::from_value(result.clone()).unwrap();

    assert_eq!(serde_json::to_value(acceptance_dto).unwrap(), acceptance);
    assert_eq!(serde_json::to_value(suspension_dto).unwrap(), suspension);
    assert_eq!(serde_json::to_value(result_dto).unwrap(), result);

    let mut future = acceptance;
    future["future"] = json!(true);
    assert!(serde_json::from_value::<SetAcceptedSuccessRequestV1>(future).is_err());
}

#[test]
fn public_review_state_dto_is_a_recovery_projection() {
    let operation = review_attempt_result();
    let state = json!({
        "version":1,
        "attempt_revision":operation["attempt_revision"].clone(),
        "attempt":operation["attempt"].clone()
    });

    let state_dto: ReviewAttemptStateResponseV1 = serde_json::from_value(state.clone()).unwrap();

    assert_eq!(serde_json::to_value(state_dto).unwrap(), state);
}

#[test]
fn public_review_dtos_reject_unknown_attempt_discriminants() {
    let mut unknown_kind = review_attempt_result();
    unknown_kind["attempt"]["presentation"]["kind"] = json!("future");
    assert!(serde_json::from_value::<ReviewAttemptOperationResponseV1>(unknown_kind).is_err());

    let mut unknown_status = review_attempt_result();
    unknown_status["attempt"]["status"] = json!("future");
    assert!(serde_json::from_value::<ReviewAttemptOperationResponseV1>(unknown_status).is_err());
}

fn review_attempt_result() -> Value {
    json!({
        "version":1,
        "operation_id":"operation",
        "changed":true,
        "attempt_revision":3,
        "attempt":{
            "attempt_id":"attempt",
            "session_id":"session",
            "thema_id":"thema",
            "presentation":{
                "kind":"basic",
                "keimenon":"Question",
                "hypothesis":[null],
                "source_guid":null,
                "review_image":null,
                "tags":[]
            },
            "revealed_answer":["answer"],
            "revealed_parathema":null,
            "matcher_version":1,
            "response":{"kind":"text","value":"answer"},
            "detected_success":true,
            "accepted_success":false,
            "override_count":1,
            "proposed_transition":null,
            "desired_suspended":false,
            "status":"graded"
        }
    })
}
