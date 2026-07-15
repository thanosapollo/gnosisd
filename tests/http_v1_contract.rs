use gnosis_core::protocol::http_v1::{
    ApiErrorCodeV1, ApiErrorResponseV1, AssetCapabilitiesV1, ContentCapabilitiesV1,
    ReplicaChangePageV1, ReplicaSnapshotPageV1, ReplicaStateV1,
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
