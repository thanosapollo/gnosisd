use gnosis_core::protocol::http_v1::{
    ActiveReviewPackEnvelopeV2, ApiErrorCodeV1, ApiErrorResponseV1, AssetCapabilitiesV1,
    AssetHeadV1, ContentCapabilitiesV1, ContentChangePageV1, DocumentHeadV1,
    DocumentMutationResultV1, DocumentRevisionPageV1, DuePageView, NodePageV1,
    NodeThemaAssociationPageV1, NodeV1, NoteRenameRequestV1, NoteWriteRequestV1,
    OfflineAssetOperationEnvelopeV1, OfflineAssetResultEnvelopeV1,
    OfflineContentOperationEnvelopeV1, OfflineContentResultEnvelopeV1,
    OfflineReviewOperationEnvelopeV1, OfflineReviewResultEnvelopeV2, ReplicaChangePageV1,
    ReplicaSnapshotPageV1, ReplicaStateV1, ReviewAttemptOperationResponseV1,
    ReviewAttemptStateResponseV1, ReviewAttemptV1, ReviewCommitResponseV1, ReviewPackEnvelopeV2,
    ReviewResponsePromptV1, ReviewSessionRequestV1, ReviewSessionV1, ReviewStateV1,
    ReviewTransitionV1, SetAcceptedSuccessRequestV1, SetSuspensionRequestV1, SettingsV1,
    StagedBlobV1, ThemaConceptRelationV1, ThemaPageV1, ThemaWriteRequestV1,
    UpdateSettingsRequestV1,
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
fn replica_state_rejects_retired_sequence_name() {
    let mut state = fixture()["state"].clone();
    let sequence = state
        .as_object_mut()
        .unwrap()
        .remove("next_device_sequence")
        .unwrap();
    state["next_review_sequence"] = sequence;

    assert!(serde_json::from_value::<ReplicaStateV1>(state).is_err());
}

#[test]
fn public_capability_and_error_dtos_preserve_http_shapes() {
    let content_wire = json!({
        "version":1,
        "server_id":"a".repeat(32),
        "document_lifecycle":1,
        "supported_content_types":["text/org; charset=utf-8"],
        "max_source_bytes":1048576,
        "max_projection_bytes":1048576
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
fn public_error_code_names_match_their_wire_values() {
    use ApiErrorCodeV1::*;

    let codes = [
        Unauthorized,
        Forbidden,
        NotFound,
        InvalidRequest,
        PreconditionRequired,
        Internal,
        InvalidContent,
        RequestTooLarge,
        InvalidBody,
        RequestTimeout,
        UnsupportedOfflineReviewVersion,
        InvalidOfflineReviewOperation,
        UnsupportedOfflineContentVersion,
        InvalidOfflineContentOperation,
        UnsupportedOfflineAssetVersion,
        InvalidOfflineAssetOperation,
        ReplicaProjectionTooLarge,
        SnapshotRequired,
        RevisionConflict,
        Busy,
        ActiveSession,
        StaleAttempt,
        ReviewStateConflict,
        IdempotencyConflict,
        DeviceSequenceGap,
        BlobRequired,
        BlobMetadataConflict,
        RenameRequired,
        InvalidBlobMetadata,
    ];
    for code in codes {
        assert_eq!(serde_json::to_value(code).unwrap(), json!(code.as_str()));
    }
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
            "content_type":"text/org; charset=utf-8",
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
    let path_conflict = json!({
        "status":"rejected",
        "format_version":1,
        "operation_id":"rename",
        "rejection":{"reason":"path_conflict"},
        "cursor":"cursor"
    });

    for wire in [applied, rejected, path_conflict] {
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
fn public_online_review_dtos_preserve_the_existing_v1_shapes() {
    let source = json!({
        "kind":"due",
        "tags":[],
        "exclude_tags":[],
        "topic_node_id":null,
        "topic_forward_depth":null,
        "topic_backward_depth":null,
        "manual_ids":[]
    });
    let request = json!({"source":source.clone(),"target_count":null});
    let session = json!({
        "session_id":"session",
        "owner_device_id":"device",
        "mode":"online",
        "source":source,
        "target_count":null,
        "status":"active",
        "current_position":0,
        "queue_length":1,
        "started_at_us":1
    });
    let attempt = review_attempt_result()["attempt"].clone();
    let committed = json!({"event_id":"event"});

    assert_round_trip::<ReviewSessionRequestV1>(request);
    assert_round_trip::<ReviewSessionV1>(session);
    assert_round_trip::<ReviewAttemptV1>(attempt);
    assert_round_trip::<ReviewCommitResponseV1>(committed);

    let mut future = review_attempt_result()["attempt"].clone();
    future["future"] = json!(true);
    assert!(serde_json::from_value::<ReviewAttemptV1>(future).is_err());

    let mut missing_null = review_attempt_result()["attempt"].clone();
    missing_null
        .as_object_mut()
        .unwrap()
        .remove("revealed_answer");
    assert!(serde_json::from_value::<ReviewAttemptV1>(missing_null).is_err());

    assert_round_trip::<ReviewStateV1>(review_state_wire());
    for field in ["stability", "difficulty"] {
        let mut missing_state = review_state_wire();
        missing_state.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<ReviewStateV1>(missing_state).is_err());
    }
}

#[test]
fn due_projection_contains_only_schedule_metadata() {
    let wire = json!({
        "items":[{"id":"thema","next_review_day":20260719,"review_count":0}],
        "next":null
    });
    let page: DuePageView = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(serde_json::to_value(page).unwrap(), wire);

    let mut answer_bearing = wire;
    answer_bearing["items"][0]["answer"] = json!(["concealed"]);
    assert!(serde_json::from_value::<DuePageView>(answer_bearing).is_err());
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
fn public_review_response_prompts_are_closed_and_explicit() {
    assert_round_trip::<ReviewResponsePromptV1>(json!({
        "kind":"text","input_script":null
    }));
    assert_round_trip::<ReviewResponsePromptV1>(json!({
        "kind":"texts","input_scripts":["greek",null]
    }));
    assert_round_trip::<ReviewResponsePromptV1>(json!({
        "kind":"choice","choices":["one","two"]
    }));

    assert!(
        serde_json::from_value::<ReviewResponsePromptV1>(json!({
            "kind":"text"
        }))
        .is_err()
    );
    assert!(
        serde_json::from_value::<ReviewResponsePromptV1>(json!({
            "kind":"choice","choices":["one","two"],"future":true
        }))
        .is_err()
    );
}

#[test]
fn public_offline_review_dtos_preserve_v1_requests_and_v2_responses() {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/protocol/offline-review-v1.json")).unwrap();

    assert_reencodes::<OfflineReviewOperationEnvelopeV1>(fixture["submission"].clone());
    for field in ["stability", "difficulty"] {
        let mut missing_state = fixture["submission"].clone();
        missing_state["operation"]["proposed_transition"]["before"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        assert!(serde_json::from_value::<OfflineReviewOperationEnvelopeV1>(missing_state).is_err());
    }
    assert_round_trip::<ReviewPackEnvelopeV2>(fixture["pack"].clone());
    assert_round_trip::<OfflineReviewResultEnvelopeV2>(fixture["result"].clone());
    assert_round_trip::<ActiveReviewPackEnvelopeV2>(json!({"version":2,"pack":null}));

    let mut unknown = fixture["pack"].clone();
    unknown["future"] = json!(true);
    assert!(serde_json::from_value::<ReviewPackEnvelopeV2>(unknown).is_err());
    assert!(serde_json::from_value::<ActiveReviewPackEnvelopeV2>(json!({"version":2})).is_err());
    let mut missing_nullable = fixture["result"].clone();
    missing_nullable["result"]
        .as_object_mut()
        .unwrap()
        .remove("conflict_reason");
    assert!(serde_json::from_value::<OfflineReviewResultEnvelopeV2>(missing_nullable).is_err());
    let mut unknown_status = fixture["pack"].clone();
    unknown_status["pack"]["status"] = json!("future");
    assert!(serde_json::from_value::<ReviewPackEnvelopeV2>(unknown_status).is_err());

    assert_offline_versions_are_exact(&fixture);
    assert_offline_thema_kind_is_closed(&fixture["pack"]);
    assert_zero_byte_pack_asset_round_trips(&fixture["pack"]);
    assert_invalid_offline_result_pairings(&fixture["result"]);
}

#[test]
fn public_offline_content_dtos_preserve_strict_versioned_shapes() {
    let operation = offline_content_operation();
    let accepted = offline_content_result("accepted");
    let conflict = offline_content_result("conflict");
    assert_round_trip::<OfflineContentOperationEnvelopeV1>(operation.clone());
    assert_round_trip::<OfflineContentResultEnvelopeV1>(accepted.clone());
    assert_round_trip::<OfflineContentResultEnvelopeV1>(conflict.clone());

    let mut wrong_version = operation;
    wrong_version["version"] = json!(2);
    assert!(serde_json::from_value::<OfflineContentOperationEnvelopeV1>(wrong_version).is_err());
    let mut wrong_result_version = accepted.clone();
    wrong_result_version["version"] = json!(2);
    assert!(
        serde_json::from_value::<OfflineContentResultEnvelopeV1>(wrong_result_version).is_err()
    );
    let mut unknown_kind = offline_content_operation();
    unknown_kind["operation"]["mutation"]["kind"] = json!("future");
    assert!(serde_json::from_value::<OfflineContentOperationEnvelopeV1>(unknown_kind).is_err());
    let mut unknown_base = offline_content_operation();
    unknown_base["operation"]["mutation"]["base"]["kind"] = json!("future");
    assert!(serde_json::from_value::<OfflineContentOperationEnvelopeV1>(unknown_base).is_err());
    assert_invalid_offline_content_results(accepted, conflict);
}

#[test]
fn public_offline_asset_dtos_preserve_strict_v1_shapes() {
    let operation = offline_asset_operation();
    let accepted = offline_asset_result("accepted");
    let conflict = offline_asset_result("conflict");
    assert_round_trip::<OfflineAssetOperationEnvelopeV1>(operation.clone());
    assert_round_trip::<OfflineAssetResultEnvelopeV1>(accepted.clone());
    assert_round_trip::<OfflineAssetResultEnvelopeV1>(conflict.clone());

    let mut wrong_version = operation;
    wrong_version["version"] = json!(2);
    assert!(serde_json::from_value::<OfflineAssetOperationEnvelopeV1>(wrong_version).is_err());
    let mut wrong_result_version = accepted.clone();
    wrong_result_version["version"] = json!(2);
    assert!(serde_json::from_value::<OfflineAssetResultEnvelopeV1>(wrong_result_version).is_err());
    let mut unknown_kind = offline_asset_operation();
    unknown_kind["operation"]["kind"] = json!("future");
    assert!(serde_json::from_value::<OfflineAssetOperationEnvelopeV1>(unknown_kind).is_err());
    let mut unknown_base = offline_asset_operation();
    unknown_base["operation"]["base"]["kind"] = json!("future");
    assert!(serde_json::from_value::<OfflineAssetOperationEnvelopeV1>(unknown_base).is_err());
    assert_invalid_offline_asset_results(accepted, conflict);
}

fn offline_asset_operation() -> Value {
    let digest = "a".repeat(64);
    json!({
        "version":1,
        "operation":{
            "operation_id":"operation","device_id":"device","device_sequence":1,
            "cursor":"cursor","authored_at_us":1,"kind":"put_asset",
            "logical_path":format!("assets/{digest}/image.png"),"original_name":"image.png",
            "content_sha256":digest,"size_bytes":4,"media_type":"image/png",
            "base":{"kind":"absent"}
        }
    })
}

fn offline_asset_result(status: &str) -> Value {
    let accepted = status == "accepted";
    json!({
        "version":1,
        "result":{
            "operation_id":"operation","status":status,
            "conflict_reason":(!accepted).then_some("invalid_content"),
            "authoritative":accepted.then(|| json!({
                "logical_path":format!("assets/{}/image.png", "a".repeat(64)),"revision":1,
                "content_sha256":"a".repeat(64),"size_bytes":4,
                "media_type":"image/png","active":true
            })),
            "cursor":"cursor"
        }
    })
}

fn assert_invalid_offline_asset_results(mut accepted: Value, mut conflict: Value) {
    let mut missing = accepted.clone();
    missing["result"]
        .as_object_mut()
        .unwrap()
        .remove("conflict_reason");
    assert!(serde_json::from_value::<OfflineAssetResultEnvelopeV1>(missing).is_err());
    let mut missing_authority = accepted.clone();
    missing_authority["result"]
        .as_object_mut()
        .unwrap()
        .remove("authoritative");
    assert!(serde_json::from_value::<OfflineAssetResultEnvelopeV1>(missing_authority).is_err());
    accepted["result"]["conflict_reason"] = json!("base_changed");
    assert!(serde_json::from_value::<OfflineAssetResultEnvelopeV1>(accepted).is_err());
    let mut inactive = offline_asset_result("accepted");
    inactive["result"]["authoritative"]["active"] = json!(false);
    assert!(serde_json::from_value::<OfflineAssetResultEnvelopeV1>(inactive).is_err());
    conflict["result"]["conflict_reason"] = Value::Null;
    assert!(serde_json::from_value::<OfflineAssetResultEnvelopeV1>(conflict.clone()).is_err());
    conflict["result"]["conflict_reason"] = json!("future");
    assert!(serde_json::from_value::<OfflineAssetResultEnvelopeV1>(conflict.clone()).is_err());
    conflict["result"]["conflict_reason"] = json!("invalid_content");
    conflict["result"]
        .as_object_mut()
        .unwrap()
        .remove("authoritative");
    assert!(serde_json::from_value::<OfflineAssetResultEnvelopeV1>(conflict).is_err());
}

fn offline_content_operation() -> Value {
    json!({
        "version":1,
        "operation":{
            "operation_id":"operation","device_id":"device","device_sequence":1,
            "cursor":"cursor","authored_at_us":1,
            "mutation":{"kind":"put_document","document_id":"document",
                "logical_path":"note.org","content_sha256":"a".repeat(64),
                "content_type":"text/org; charset=utf-8","source":"#+title: Note\n",
                "base":{"kind":"absent"}}
        }
    })
}

fn offline_content_result(status: &str) -> Value {
    let accepted = status == "accepted";
    json!({
        "version":1,
        "result":{
            "operation_id":"operation","status":status,
            "conflict_reason":(!accepted).then_some("invalid_content"),
            "authoritative":accepted.then(|| json!({
                "document_id":"document","logical_path":"note.org",
                "content_type":"text/org; charset=utf-8","revision":1,
                "content_sha256":"a".repeat(64),"active":true
            })),
            "cursor":"cursor"
        }
    })
}

fn assert_invalid_offline_content_results(mut accepted: Value, mut conflict: Value) {
    let mut missing_conflict_reason = accepted.clone();
    missing_conflict_reason["result"]
        .as_object_mut()
        .unwrap()
        .remove("conflict_reason");
    assert!(
        serde_json::from_value::<OfflineContentResultEnvelopeV1>(missing_conflict_reason).is_err()
    );

    let mut tombstone = accepted.clone();
    tombstone["result"]["authoritative"]["content_sha256"] = Value::Null;
    tombstone["result"]["authoritative"]["active"] = json!(false);
    assert_round_trip::<OfflineContentResultEnvelopeV1>(tombstone.clone());
    tombstone["result"]["authoritative"]
        .as_object_mut()
        .unwrap()
        .remove("content_sha256");
    assert!(serde_json::from_value::<OfflineContentResultEnvelopeV1>(tombstone).is_err());

    accepted["result"]["conflict_reason"] = json!("base_changed");
    assert!(serde_json::from_value::<OfflineContentResultEnvelopeV1>(accepted).is_err());
    conflict["result"]["conflict_reason"] = Value::Null;
    assert!(serde_json::from_value::<OfflineContentResultEnvelopeV1>(conflict.clone()).is_err());
    conflict["result"]["conflict_reason"] = json!("future");
    assert!(serde_json::from_value::<OfflineContentResultEnvelopeV1>(conflict.clone()).is_err());
    conflict["result"]["conflict_reason"] = json!("invalid_content");
    conflict["result"]
        .as_object_mut()
        .unwrap()
        .remove("authoritative");
    assert!(serde_json::from_value::<OfflineContentResultEnvelopeV1>(conflict.clone()).is_err());
    conflict["result"]["status"] = json!("accepted");
    assert!(serde_json::from_value::<OfflineContentResultEnvelopeV1>(conflict).is_err());
}

fn assert_offline_versions_are_exact(fixture: &Value) {
    let mut submission = fixture["submission"].clone();
    submission["version"] = json!(2);
    assert!(serde_json::from_value::<OfflineReviewOperationEnvelopeV1>(submission).is_err());

    let mut pack = fixture["pack"].clone();
    pack["version"] = json!(1);
    assert!(serde_json::from_value::<ReviewPackEnvelopeV2>(pack).is_err());

    let mut result = fixture["result"].clone();
    result["version"] = json!(1);
    assert!(serde_json::from_value::<OfflineReviewResultEnvelopeV2>(result).is_err());

    let active = json!({"version":1,"pack":null});
    assert!(serde_json::from_value::<ActiveReviewPackEnvelopeV2>(active).is_err());
}

fn assert_offline_thema_kind_is_closed(pack: &Value) {
    let mut unknown = pack.clone();
    unknown["pack"]["items"][0]["presentation"]["kind"] = json!("future");
    assert!(serde_json::from_value::<ReviewPackEnvelopeV2>(unknown).is_err());
}

fn assert_zero_byte_pack_asset_round_trips(pack: &Value) {
    let mut zero_byte_asset = pack.clone();
    let digest = "a".repeat(64);
    zero_byte_asset["pack"]["assets"] = json!([{
        "logical_path":format!("assets/{digest}/empty.bin"),
        "content_sha256":digest,
        "size_bytes":0,
        "media_type":"application/octet-stream"
    }]);
    assert_round_trip::<ReviewPackEnvelopeV2>(zero_byte_asset);
}

fn assert_invalid_offline_result_pairings(result: &Value) {
    let mut accepted_without_event = result.clone();
    accepted_without_event["result"]["event_id"] = Value::Null;
    assert!(
        serde_json::from_value::<OfflineReviewResultEnvelopeV2>(accepted_without_event).is_err()
    );

    let mut accepted_with_conflict = result.clone();
    accepted_with_conflict["result"]["conflict_reason"] = json!("content_changed");
    assert!(
        serde_json::from_value::<OfflineReviewResultEnvelopeV2>(accepted_with_conflict).is_err()
    );

    let mut conflict = result.clone();
    conflict["result"]["status"] = json!("conflict");
    conflict["result"]["conflict_reason"] = json!("content_changed");
    assert!(serde_json::from_value::<OfflineReviewResultEnvelopeV2>(conflict.clone()).is_err());
    conflict["result"]["event_id"] = Value::Null;
    assert_round_trip::<OfflineReviewResultEnvelopeV2>(conflict.clone());
    conflict["result"]["conflict_reason"] = Value::Null;
    assert!(serde_json::from_value::<OfflineReviewResultEnvelopeV2>(conflict).is_err());
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
        "document_id":"document","logical_path":"notes/example.org",
        "content_type":"text/org; charset=utf-8","revision":2,
        "content_sha256":null,"active":false
    }));
    assert_round_trip::<DocumentRevisionPageV1>(json!({
        "items":[{"document_id":"document","revision":2,
            "logical_path":"notes/example.org","content_type":"text/org; charset=utf-8",
            "content_sha256":null,
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
            "tags":[],"level":2,"document_revision":4,
            "content_sha256":"a".repeat(64),"start_byte":10,"end_byte":20,
            "active":true}],
        "next":"node"
    }));
    assert_round_trip::<NodePageV1>(json!({
        "items":[{"node_id":"legacy","document_id":"document",
            "logical_path":"notes/example.org","title":"Legacy","title_path":["Legacy"],
            "tags":[],"level":1,"document_revision":1,
            "content_sha256":"a".repeat(64),"start_byte":null,"end_byte":null,
            "active":false}],
        "next":"legacy"
    }));
    let missing_end = json!({
        "node_id":"node","document_id":"document","logical_path":"notes/example.org",
        "title":"Node","title_path":["Node"],"tags":[],"level":1,
        "document_revision":1,"content_sha256":"a".repeat(64),
        "start_byte":0,"active":true
    });
    assert!(serde_json::from_value::<NodeV1>(missing_end).is_err());
    assert_round_trip::<NodeThemaAssociationPageV1>(json!({
        "items":[
            {"thema_id":"thema-a","relation":"primary"},
            {"thema_id":"thema-b","relation":"supporting"},
            {"thema_id":"thema-c","relation":"reference"}
        ],
        "next":"thema-c"
    }));
    assert!(
        serde_json::from_value::<NodeV1>(json!({
            "node_id":"node","document_id":"document","logical_path":"notes/example.org",
            "title":"Node","title_path":["Parent"],"tags":[],"level":2,
            "document_revision":4,"content_sha256":"a".repeat(64),
            "start_byte":10,"end_byte":20,"active":true,
            "thema_ids":["thema"]
        }))
        .is_err()
    );
    assert_reencodes::<SettingsV1>(settings_wire(true));
}

#[test]
fn public_content_requests_are_strict_and_preserve_defaults() {
    let note = json!({
        "logical_path":"notes/example.org",
        "content_type":"text/org; charset=utf-8",
        "source":"#+title: Example\n"
    });
    assert_round_trip::<NoteWriteRequestV1>(note.clone());
    let mut future = note;
    future["future"] = json!(true);
    assert!(serde_json::from_value::<NoteWriteRequestV1>(future).is_err());
    assert_round_trip::<NoteRenameRequestV1>(json!({
        "logical_path":"notes/renamed.org"
    }));

    let thema: ThemaWriteRequestV1 = serde_json::from_value(json!({
        "kind":"basic","keimenon":"Question","answer":["Answer"],
        "concepts":[
            {"node_id":"primary","relation":"primary"},
            {"node_id":"supporting","relation":"supporting"},
            {"node_id":"reference","relation":"reference"}
        ]
    }))
    .unwrap();
    assert!(thema.hypothesis.is_empty());
    assert!(thema.tags.is_empty());
    assert_eq!(
        thema
            .concepts
            .iter()
            .map(|concept| concept.relation)
            .collect::<Vec<_>>(),
        [
            ThemaConceptRelationV1::Primary,
            ThemaConceptRelationV1::Supporting,
            ThemaConceptRelationV1::Reference,
        ]
    );
    assert!(thema.asset_paths.is_empty());
    for relation in ["other", "Primary", ""] {
        assert!(
            serde_json::from_value::<ThemaWriteRequestV1>(json!({
                "kind":"basic","keimenon":"Question","answer":["Answer"],
                "concepts":[{"node_id":"node","relation":relation}]
            }))
            .is_err(),
            "{relation}"
        );
    }
    assert!(
        serde_json::from_value::<ThemaWriteRequestV1>(json!({
            "kind":"basic","keimenon":"Question","answer":["Answer"],
            "node_ids":["legacy"]
        }))
        .is_err()
    );

    let settings: UpdateSettingsRequestV1 = serde_json::from_value(settings_wire(false)).unwrap();
    assert_eq!(settings.desired_retention, 0.9);
}

#[test]
fn public_content_responses_require_explicit_nullable_fields() {
    let mut head = json!({
        "document_id":"document","logical_path":"notes/example.org",
        "content_type":"text/org; charset=utf-8","revision":2,
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

fn assert_reencodes<T>(wire: Value)
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let value: T = serde_json::from_value(wire).unwrap();
    let encoded = serde_json::to_value(value).unwrap();
    serde_json::from_value::<T>(encoded).unwrap();
}

fn thema_wire() -> Value {
    json!({
        "thema_id":"thema","revision":1,"kind":"basic","keimenon":"Question",
        "hypothesis":[null],"answer":["Answer"],"source_guid":null,"parathema":null,
        "review_image":null,"tags":[],"concepts":[
            {"node_id":"primary","relation":"primary"},
            {"node_id":"supporting","relation":"supporting"},
            {"node_id":"reference","relation":"reference"}
        ],"asset_paths":[],
        "content_sha256":"a".repeat(64),"archived":false
    })
}

fn settings_wire(include_output_fields: bool) -> Value {
    if include_output_fields {
        json!({
            "revision":1,"timezone":"Europe/Athens","day_boundary_hour":0,
            "scheduler":{"algorithm":"fsrs","model":"fsrs6","implementation":"fsrs-6.6.1",
                "desired_retention":0.9,"parameters":[0.0,0.0,0.0,0.0,0.0,0.0,0.0,
                    0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0,0.0]}
        })
    } else {
        json!({"timezone":"Europe/Athens","day_boundary_hour":0,"desired_retention":0.9})
    }
}

fn review_state_wire() -> Value {
    json!({
        "state_revision":0,"review_count":0,"last_reviewed_at_us":null,
        "last_review_day":null,"next_review_day":20260715,"interval_days":null,
        "total_successes":0,"total_failures":0,"suspended":false,
        "stability":null,"difficulty":null
    })
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
                "question":"Question",
                "response":{"kind":"text","input_script":null},
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
            "proposed_transition":review_transition_wire(),
            "desired_suspended":false,
            "status":"graded"
        }
    })
}

fn review_transition_wire() -> Value {
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/protocol/offline-review-v1.json")).unwrap();
    let transition: ReviewTransitionV1 =
        serde_json::from_value(fixture["submission"]["operation"]["proposed_transition"].clone())
            .unwrap();
    serde_json::to_value(transition).unwrap()
}

#[test]
fn review_attempt_status_controls_reveal_and_grading_fields() {
    let graded = review_attempt_result()["attempt"].clone();
    assert!(serde_json::from_value::<ReviewAttemptV1>(graded.clone()).is_ok());

    let mut committed = graded.clone();
    committed["status"] = json!("committed");
    assert!(serde_json::from_value::<ReviewAttemptV1>(committed).is_ok());

    let mut pending = graded.clone();
    pending["status"] = json!("pending");
    for field in [
        "revealed_answer",
        "revealed_parathema",
        "response",
        "detected_success",
        "accepted_success",
        "proposed_transition",
    ] {
        pending[field] = Value::Null;
    }
    pending["override_count"] = json!(0);
    assert!(serde_json::from_value::<ReviewAttemptV1>(pending.clone()).is_ok());

    for field in [
        "revealed_answer",
        "response",
        "detected_success",
        "accepted_success",
        "proposed_transition",
    ] {
        let mut invalid = graded.clone();
        invalid[field] = Value::Null;
        assert!(
            serde_json::from_value::<ReviewAttemptV1>(invalid).is_err(),
            "graded attempt accepted null {field}"
        );
    }

    for (field, value) in [
        ("revealed_answer", json!(["answer"])),
        ("revealed_parathema", json!("explanation")),
        ("response", json!({"kind":"text","value":"answer"})),
        ("detected_success", json!(true)),
        ("accepted_success", json!(true)),
        ("proposed_transition", review_transition_wire()),
        ("override_count", json!(1)),
    ] {
        let mut invalid = pending.clone();
        invalid[field] = value;
        assert!(
            serde_json::from_value::<ReviewAttemptV1>(invalid).is_err(),
            "pending attempt accepted grading field {field}"
        );
    }
}
