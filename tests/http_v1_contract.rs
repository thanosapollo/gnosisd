use gnosis_core::protocol::http_v1::{
    ApiErrorCodeV1, ApiErrorResponseV1, AssetCapabilitiesV1, AssetHeadV1, ContentCapabilitiesV1,
    ContentChangePageV1, DocumentHeadV1, DocumentMutationResultV1, DocumentRevisionPageV1,
    NodePageV1, NoteRenameRequestV1, NoteWriteRequestV1, ReplicaChangePageV1,
    ReplicaSnapshotPageV1, ReplicaStateV1, ReviewAttemptOperationResponseV1,
    ReviewAttemptStateResponseV1, SetAcceptedSuccessRequestV1, SetSuspensionRequestV1, SettingsV1,
    StagedBlobV1, ThemaPageV1, ThemaWriteRequestV1, UpdateSettingsRequestV1,
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
fn public_document_mutation_results_are_exclusive_and_strict() {
    let applied = json!({
        "status":"applied",
        "format_version":1,
        "operation_id":"operation",
        "changed":true,
        "authoritative":{
            "document_id":"document",
            "logical_path":"note.org",
            "revision":2,
            "content_sha256":"a".repeat(64),
            "active":true
        },
        "cursor":"cursor"
    });
    let rejected = json!({
        "status":"rejected",
        "format_version":1,
        "operation_id":"operation",
        "rejection":{
            "reason":"revision_conflict",
            "expected_revision":1,
            "current_revision":2
        },
        "cursor":"cursor"
    });

    for wire in [applied, rejected] {
        let result: DocumentMutationResultV1 = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(serde_json::to_value(result).unwrap(), wire);
    }
    let inconsistent = json!({
        "status":"rejected",
        "format_version":1,
        "operation_id":"operation",
        "changed":false,
        "rejection":{"reason":"invalid_content"},
        "cursor":"cursor"
    });
    assert!(serde_json::from_value::<DocumentMutationResultV1>(inconsistent).is_err());
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

#[test]
fn public_content_dtos_preserve_existing_resource_shapes() {
    assert_round_trip::<ContentChangePageV1>(json!({
        "items":[{"cursor":1,"entity_kind":"document","entity_id":"document",
            "revision":2,"operation":"update","created_at_us":3}],
        "next":1
    }));
    assert_round_trip::<ContentChangePageV1>(json!({
        "items":[{"cursor":2,"entity_kind":"review","entity_id":"thema",
            "revision":3,"operation":"commit","created_at_us":4}],
        "next":2
    }));
    assert_round_trip::<DocumentHeadV1>(json!({
        "document_id":"document","logical_path":"notes/example.org","revision":2,
        "content_sha256":null,"active":false
    }));
    assert_round_trip::<DocumentRevisionPageV1>(json!({
        "items":[{"document_id":"document","revision":2,
            "logical_path":"notes/example.org","content_sha256":null,
            "operation":"tombstone","created_at_us":3,"author_device_id":null}],
        "next":2
    }));
    assert_round_trip::<AssetHeadV1>(json!({
        "logical_path":format!("assets/{}/image.png", "a".repeat(64)),"revision":2,
        "content_sha256":null,"media_type":null,"active":false
    }));
    assert_round_trip::<StagedBlobV1>(json!({
        "content_sha256":"a".repeat(64),"size_bytes":12
    }));
    assert_round_trip::<ThemaPageV1>(json!({"items":[thema_wire()],"next":"thema"}));
    assert_round_trip::<NodePageV1>(json!({
        "items":[{"node_id":"node","document_id":"document",
            "logical_path":"notes/example.org","title":"Node","title_path":["Parent"],
            "tags":[],"level":2,"active":true,"thema_ids":["thema"]}],
        "next":"node"
    }));
    assert_round_trip::<SettingsV1>(settings_wire(true));
}

#[test]
fn public_content_requests_are_strict_and_preserve_defaults() {
    let note = json!({"logical_path":"notes/example.org","org":"#+title: Example\n"});
    assert_round_trip::<NoteWriteRequestV1>(note.clone());
    let mut future = note;
    future["future"] = json!(true);
    assert!(serde_json::from_value::<NoteWriteRequestV1>(future).is_err());
    assert_round_trip::<NoteRenameRequestV1>(json!({
        "logical_path":"notes/renamed.org"
    }));

    let thema: ThemaWriteRequestV1 = serde_json::from_value(json!({
        "kind":"basic","keimenon":"Question","answer":["Answer"]
    }))
    .unwrap();
    assert!(thema.hypothesis.is_empty());
    assert!(thema.tags.is_empty());
    assert!(thema.node_ids.is_empty());
    assert!(thema.asset_paths.is_empty());

    let settings: UpdateSettingsRequestV1 = serde_json::from_value(settings_wire(false)).unwrap();
    assert_eq!(settings.scheduler.scheduler_version, 0);
    assert!(settings.tag_rules.is_empty());
}

#[test]
fn public_content_responses_require_explicit_nullable_fields() {
    let mut head = json!({
        "document_id":"document","logical_path":"notes/example.org","revision":2,
        "content_sha256":null,"active":false
    });
    head.as_object_mut().unwrap().remove("content_sha256");
    assert!(serde_json::from_value::<DocumentHeadV1>(head).is_err());

    let mut thema = thema_wire();
    thema.as_object_mut().unwrap().remove("source_guid");
    assert!(
        serde_json::from_value::<ThemaPageV1>(json!({
            "items":[thema],"next":null
        }))
        .is_err()
    );
}

fn assert_round_trip<T>(wire: Value)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let value: T = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(value).unwrap(), wire);
}

fn thema_wire() -> Value {
    json!({
        "thema_id":"thema","revision":1,"kind":"basic","keimenon":"Question",
        "hypothesis":[null],"answer":["Answer"],"source_guid":null,"parathema":null,
        "review_image":null,"tags":[],"node_ids":[],"asset_paths":[],
        "content_sha256":"a".repeat(64),"archived":false
    })
}

fn settings_wire(include_output_fields: bool) -> Value {
    let mut value = json!({
        "timezone":"UTC","day_boundary_hour":0,
        "scheduler":{"amnesia":0.5,"epignosis":0.1,"agnoia":0.2,"anagnosis":3,
            "lethe":2,"synolon_max":3.0,"interval_fuzz":0.1,"proto":[0,1,2]}
    });
    if include_output_fields {
        value["revision"] = json!(1);
        value["scheduler"]["scheduler_version"] = json!(1);
        value["tag_rules"] = json!({"tag":{"amnesia":null,"epignosis":null,"agnoia":null,
            "anagnosis":null,"lethe":null,"proto":null}});
    }
    value
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
