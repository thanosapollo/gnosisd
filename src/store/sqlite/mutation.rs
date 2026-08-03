use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::review::set_thema_suspended_in;
use super::{
    AssetWriteDecision, NoteWriteDecision, SqliteStore, StoreError, asset_write_decision,
    current_content_cursor, load_document_head, note_write_decision, put_asset_in, put_thema_in,
    rename_document_in, replace_note_in, replica_cursor_token, server_id, set_thema_archived_in,
    tombstone_asset_in, tombstone_document_in, update_settings_in,
};
use crate::{
    application::{
        DeviceDesiredStateMutation, DeviceDocumentLifecycleMutation, DeviceMutationCursor,
        DeviceMutationIdentity, DeviceMutationKind, DeviceMutationProjection,
        DeviceMutationRejection, DeviceMutationResult, PreparedDeviceDesiredStateMutation,
        PreparedDeviceDocumentLifecycleMutation, PreparedDeviceDocumentMutation,
        ThemaMutationProjection,
    },
    blob::BlobMeta,
};

const REPLAY_SQL: &str = "SELECT request_sha256, request_json, result_kind, result_json
     FROM device_mutation_receipt WHERE device_id = ?1 AND operation_id = ?2";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DeviceMutationExecution {
    pub(crate) result: DeviceMutationResult,
    pub(crate) replayed: bool,
}

impl SqliteStore {
    pub(crate) fn apply_device_document_mutation(
        &mut self,
        operation: &PreparedDeviceDocumentMutation,
        blob: Option<&BlobMeta>,
        created_at_us: i64,
    ) -> Result<DeviceMutationExecution, StoreError> {
        validate_document_operation(operation, blob)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        if let Some(replay) = replayed_device_mutation(&transaction, &operation.identity)? {
            transaction.commit().map_err(StoreError::storage)?;
            return Ok(replay);
        }
        let result = apply_new_document_mutation(&transaction, operation, blob, created_at_us)?;
        insert_receipt(&transaction, &operation.identity, &result, created_at_us)?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(DeviceMutationExecution {
            result,
            replayed: false,
        })
    }

    pub(crate) fn device_document_mutation_needs_blob(
        &self,
        operation: &PreparedDeviceDocumentMutation,
    ) -> Result<bool, StoreError> {
        validate_document_operation(operation, None)?;
        if replayed_device_mutation(&self.connection, &operation.identity)?.is_some() {
            return Ok(false);
        }
        Ok(matches!(
            document_mutation_preflight(&self.connection, operation)?,
            DocumentMutationPreflight::Write
        ))
    }

    pub(crate) fn apply_device_document_lifecycle_mutation(
        &mut self,
        operation: &PreparedDeviceDocumentLifecycleMutation,
        created_at_us: i64,
    ) -> Result<DeviceMutationExecution, StoreError> {
        operation.validate().map_err(|_| StoreError::CorruptData {
            field: "device document lifecycle mutation",
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        if let Some(replay) = replayed_device_mutation(&transaction, &operation.identity)? {
            transaction.commit().map_err(StoreError::storage)?;
            return Ok(replay);
        }
        let result = apply_new_document_lifecycle(&transaction, operation, created_at_us)?;
        insert_receipt(&transaction, &operation.identity, &result, created_at_us)?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(DeviceMutationExecution {
            result,
            replayed: false,
        })
    }

    pub(crate) fn apply_device_desired_state_mutation(
        &mut self,
        operation: &PreparedDeviceDesiredStateMutation,
        created_at_us: i64,
    ) -> Result<DeviceMutationExecution, StoreError> {
        operation.validate().map_err(|_| StoreError::CorruptData {
            field: "device desired-state mutation",
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        if let Some(replay) = replayed_device_mutation(&transaction, &operation.identity)? {
            transaction.commit().map_err(StoreError::storage)?;
            return Ok(replay);
        }
        transaction
            .execute_batch("SAVEPOINT device_mutation_apply")
            .map_err(StoreError::storage)?;
        let result = match apply_desired_state_mutation(&transaction, operation, created_at_us) {
            Ok((changed, authoritative)) => {
                transaction
                    .execute_batch("RELEASE device_mutation_apply")
                    .map_err(StoreError::storage)?;
                DeviceMutationResult::applied(
                    &operation.identity,
                    changed,
                    authoritative,
                    mutation_cursor(&transaction)?,
                )
            }
            Err(error) => {
                let Some(rejection) = desired_state_rejection(&error) else {
                    return Err(error);
                };
                transaction
                    .execute_batch(
                        "ROLLBACK TO device_mutation_apply; RELEASE device_mutation_apply",
                    )
                    .map_err(StoreError::storage)?;
                rejected_result(&transaction, &operation.identity, rejection)?
            }
        };
        insert_receipt(&transaction, &operation.identity, &result, created_at_us)?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(DeviceMutationExecution {
            result,
            replayed: false,
        })
    }

    pub(crate) fn device_desired_state_mutation_needs_blob(
        &self,
        operation: &PreparedDeviceDesiredStateMutation,
    ) -> Result<bool, StoreError> {
        operation.validate().map_err(|_| StoreError::CorruptData {
            field: "device desired-state mutation",
        })?;
        if replayed_device_mutation(&self.connection, &operation.identity)?.is_some() {
            return Ok(false);
        }
        let DeviceDesiredStateMutation::PutAsset {
            prepared,
            digest,
            body_sha256: _,
            size_bytes,
            media_type,
        } = &operation.mutation
        else {
            return Ok(false);
        };
        let blob = BlobMeta {
            sha256: digest.clone(),
            size_bytes: *size_bytes,
            media_type: media_type.clone(),
        };
        match asset_write_decision(&self.connection, prepared, &blob, true) {
            Ok(AssetWriteDecision::Write) => Ok(true),
            Ok(AssetWriteDecision::Unchanged(_)) => Ok(false),
            Err(error) if desired_state_rejection(&error).is_some() => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn apply_desired_state_mutation(
    transaction: &Transaction<'_>,
    operation: &PreparedDeviceDesiredStateMutation,
    created_at_us: i64,
) -> Result<(bool, DeviceMutationProjection), StoreError> {
    let author = Some(operation.identity.device_id.as_str());
    match &operation.mutation {
        DeviceDesiredStateMutation::PutAsset {
            prepared,
            digest,
            body_sha256: _,
            size_bytes,
            media_type,
        } => {
            let blob = BlobMeta {
                sha256: digest.clone(),
                size_bytes: *size_bytes,
                media_type: media_type.clone(),
            };
            let (head, changed) =
                put_asset_in(transaction, prepared, &blob, created_at_us, author, true)?;
            Ok((changed, DeviceMutationProjection::Asset(head)))
        }
        DeviceDesiredStateMutation::TombstoneAsset {
            asset_path,
            expected_revision,
        } => Ok((
            true,
            DeviceMutationProjection::Asset(tombstone_asset_in(
                transaction,
                asset_path,
                *expected_revision,
                created_at_us,
                author,
            )?),
        )),
        DeviceDesiredStateMutation::PutThema {
            prepared,
            initial_review_day,
        } => {
            let (record, changed) = put_thema_in(
                transaction,
                prepared,
                *initial_review_day,
                created_at_us,
                author,
                true,
            )?;
            Ok((
                changed,
                DeviceMutationProjection::Thema(thema_projection(record)),
            ))
        }
        DeviceDesiredStateMutation::SetThemaArchived {
            thema_id,
            expected_revision,
            archived,
        } => Ok((
            true,
            DeviceMutationProjection::Thema(thema_projection(set_thema_archived_in(
                transaction,
                thema_id.as_str(),
                *expected_revision,
                *archived,
                created_at_us,
                author,
            )?)),
        )),
        DeviceDesiredStateMutation::SetThemaSuspended {
            thema_id,
            expected_state_revision,
            suspended,
        } => {
            let (state, changed) = set_thema_suspended_in(
                transaction,
                thema_id.as_str(),
                *expected_state_revision,
                *suspended,
                created_at_us,
            )?;
            Ok((
                changed,
                DeviceMutationProjection::ReviewState {
                    thema_id: thema_id.to_string(),
                    state,
                },
            ))
        }
        DeviceDesiredStateMutation::ReplaceSettings {
            input,
            expected_revision,
        } => Ok((
            true,
            DeviceMutationProjection::Settings(update_settings_in(
                transaction,
                input,
                *expected_revision,
                created_at_us,
                author,
            )?),
        )),
    }
}

fn thema_projection(record: crate::application::ThemaRecord) -> ThemaMutationProjection {
    ThemaMutationProjection::from(&record)
}

fn desired_state_rejection(error: &StoreError) -> Option<DeviceMutationRejection> {
    match error {
        StoreError::Conflict { expected, actual } => {
            Some(DeviceMutationRejection::RevisionConflict {
                expected: Some(*expected),
                actual: *actual,
            })
        }
        StoreError::ExpectedAbsent { actual } => Some(DeviceMutationRejection::RevisionConflict {
            expected: None,
            actual: Some(*actual),
        }),
        StoreError::NotFound => Some(DeviceMutationRejection::NotFound),
        StoreError::PathConflict => Some(DeviceMutationRejection::PathConflict),
        StoreError::BlobMetadata
        | StoreError::InactiveReference { .. }
        | StoreError::InvalidProjection { .. } => Some(DeviceMutationRejection::InvalidContent),
        _ => None,
    }
}

fn validate_document_operation(
    operation: &PreparedDeviceDocumentMutation,
    blob: Option<&BlobMeta>,
) -> Result<(), StoreError> {
    let prepared = &operation.prepared;
    let digest = format!("{:x}", Sha256::digest(prepared.source.as_bytes()));
    let blob_matches = blob.is_none_or(|blob| {
        blob.sha256 == digest
            && blob.size_bytes == prepared.source.len() as u64
            && blob.media_type == prepared.content_type
    });
    if operation.validate().is_ok() && prepared.blob_sha256 == digest && blob_matches {
        Ok(())
    } else {
        Err(StoreError::CorruptData {
            field: "device document mutation",
        })
    }
}

fn replayed_device_mutation(
    connection: &Connection,
    identity: &DeviceMutationIdentity,
) -> Result<Option<DeviceMutationExecution>, StoreError> {
    let stored = connection
        .query_row(
            REPLAY_SQL,
            params![identity.device_id, identity.operation_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    let Some((digest, request_json, result_kind, result_json)) = stored else {
        return Ok(None);
    };
    verify_replayed_request(identity, &digest, &request_json)?;
    let result = decode_replayed_result(identity, &result_kind, &result_json)?;
    Ok(Some(DeviceMutationExecution {
        result,
        replayed: true,
    }))
}

fn verify_replayed_request(
    identity: &DeviceMutationIdentity,
    stored_digest: &str,
    stored_json: &str,
) -> Result<(), StoreError> {
    let actual = format!("{:x}", Sha256::digest(stored_json.as_bytes()));
    if actual != stored_digest {
        return Err(StoreError::CorruptData {
            field: "device mutation request",
        });
    }
    if stored_digest == identity.request_sha256 && stored_json == identity.request_json {
        Ok(())
    } else {
        Err(StoreError::IdempotencyConflict)
    }
}

fn decode_replayed_result(
    identity: &DeviceMutationIdentity,
    stored_kind: &str,
    stored_json: &str,
) -> Result<DeviceMutationResult, StoreError> {
    let result: DeviceMutationResult =
        serde_json::from_str(stored_json).map_err(|_| StoreError::CorruptData {
            field: "device mutation result",
        })?;
    if result.result_kind() != stored_kind || result.validate_for(identity).is_err() {
        Err(StoreError::CorruptData {
            field: "device mutation result",
        })
    } else {
        Ok(result)
    }
}

fn apply_new_document_mutation(
    transaction: &Transaction<'_>,
    operation: &PreparedDeviceDocumentMutation,
    blob: Option<&BlobMeta>,
    created_at_us: i64,
) -> Result<DeviceMutationResult, StoreError> {
    let before = load_document_head(transaction, &operation.prepared.document_id)?;
    match document_mutation_preflight(transaction, operation)? {
        DocumentMutationPreflight::Rejected(rejection) => {
            return rejected_result(transaction, &operation.identity, rejection);
        }
        DocumentMutationPreflight::Unchanged => {
            return unchanged_document_result(transaction, operation);
        }
        DocumentMutationPreflight::Write => {}
    }
    let blob = blob.ok_or(StoreError::CorruptData {
        field: "device document blob",
    })?;
    transaction
        .execute_batch("SAVEPOINT device_mutation_apply")
        .map_err(StoreError::storage)?;
    match replace_note_in(
        transaction,
        &operation.prepared,
        blob,
        operation.prepared.expected_revision,
        created_at_us,
        Some(&operation.identity.device_id),
        true,
    ) {
        Ok(_) => applied_document_result(transaction, operation, before),
        Err(error) => rejected_document_result(transaction, operation, error),
    }
}

enum DocumentMutationPreflight {
    Rejected(DeviceMutationRejection),
    Unchanged,
    Write,
}

fn document_mutation_preflight(
    connection: &Connection,
    operation: &PreparedDeviceDocumentMutation,
) -> Result<DocumentMutationPreflight, StoreError> {
    let before = load_document_head(connection, &operation.prepared.document_id)?;
    if operation.identity.mutation_kind == DeviceMutationKind::Create && before.is_some() {
        return Ok(DocumentMutationPreflight::Rejected(
            DeviceMutationRejection::RevisionConflict {
                expected: None,
                actual: before.map(|head| head.revision),
            },
        ));
    }
    match note_write_decision(
        connection,
        &operation.prepared,
        operation.prepared.expected_revision,
        true,
    ) {
        Ok(NoteWriteDecision::Unchanged(_)) => Ok(DocumentMutationPreflight::Unchanged),
        Ok(NoteWriteDecision::Write(_)) => Ok(DocumentMutationPreflight::Write),
        Err(error) => match document_rejection(&error) {
            Some(rejection) => Ok(DocumentMutationPreflight::Rejected(rejection)),
            None => Err(error),
        },
    }
}

fn unchanged_document_result(
    connection: &Connection,
    operation: &PreparedDeviceDocumentMutation,
) -> Result<DeviceMutationResult, StoreError> {
    let head = load_document_head(connection, &operation.prepared.document_id)?
        .ok_or(StoreError::NotFound)?;
    Ok(DeviceMutationResult::applied(
        &operation.identity,
        false,
        DeviceMutationProjection::Document(head),
        mutation_cursor(connection)?,
    ))
}

fn applied_document_result(
    transaction: &Transaction<'_>,
    operation: &PreparedDeviceDocumentMutation,
    before: Option<crate::application::DocumentHead>,
) -> Result<DeviceMutationResult, StoreError> {
    transaction
        .execute_batch("RELEASE device_mutation_apply")
        .map_err(StoreError::storage)?;
    let head = load_document_head(transaction, &operation.prepared.document_id)?
        .ok_or(StoreError::NotFound)?;
    let changed = before.as_ref() != Some(&head);
    Ok(DeviceMutationResult::applied(
        &operation.identity,
        changed,
        DeviceMutationProjection::Document(head),
        mutation_cursor(transaction)?,
    ))
}

fn rejected_document_result(
    transaction: &Transaction<'_>,
    operation: &PreparedDeviceDocumentMutation,
    error: StoreError,
) -> Result<DeviceMutationResult, StoreError> {
    let Some(rejection) = document_rejection(&error) else {
        return Err(error);
    };
    transaction
        .execute_batch("ROLLBACK TO device_mutation_apply; RELEASE device_mutation_apply")
        .map_err(StoreError::storage)?;
    rejected_result(transaction, &operation.identity, rejection)
}

fn document_rejection(error: &StoreError) -> Option<DeviceMutationRejection> {
    match error {
        StoreError::Conflict { expected, actual } => {
            Some(DeviceMutationRejection::RevisionConflict {
                expected: Some(*expected),
                actual: *actual,
            })
        }
        StoreError::NotFound => Some(DeviceMutationRejection::InvalidContent),
        StoreError::InactiveReference { .. } | StoreError::InvalidProjection { .. } => {
            Some(DeviceMutationRejection::InvalidContent)
        }
        StoreError::NodeIdConflict { node_id } => Some(DeviceMutationRejection::NodeIdConflict {
            node_id: node_id.clone(),
        }),
        StoreError::PathConflict => Some(DeviceMutationRejection::PathConflict),
        StoreError::PathChange => Some(DeviceMutationRejection::RenameRequired),
        _ => None,
    }
}

fn apply_new_document_lifecycle(
    transaction: &Transaction<'_>,
    operation: &PreparedDeviceDocumentLifecycleMutation,
    created_at_us: i64,
) -> Result<DeviceMutationResult, StoreError> {
    match document_lifecycle_preflight(transaction, operation)? {
        DocumentLifecyclePreflight::Rejected(rejection) => {
            return rejected_result(transaction, &operation.identity, rejection);
        }
        DocumentLifecyclePreflight::Unchanged(head) => {
            return Ok(DeviceMutationResult::applied(
                &operation.identity,
                false,
                DeviceMutationProjection::Document(head),
                mutation_cursor(transaction)?,
            ));
        }
        DocumentLifecyclePreflight::Write => {}
    }
    apply_document_lifecycle_write(transaction, operation, created_at_us)
}

enum DocumentLifecyclePreflight {
    Rejected(DeviceMutationRejection),
    Unchanged(crate::application::DocumentHead),
    Write,
}

fn document_lifecycle_preflight(
    connection: &Connection,
    operation: &PreparedDeviceDocumentLifecycleMutation,
) -> Result<DocumentLifecyclePreflight, StoreError> {
    let Some(head) = load_document_head(connection, &operation.document_id)? else {
        return Ok(DocumentLifecyclePreflight::Rejected(
            DeviceMutationRejection::NotFound,
        ));
    };
    if head.revision != operation.expected_revision {
        return Ok(DocumentLifecyclePreflight::Rejected(
            DeviceMutationRejection::RevisionConflict {
                expected: Some(operation.expected_revision),
                actual: Some(head.revision),
            },
        ));
    }
    Ok(lifecycle_state_preflight(operation, head))
}

fn lifecycle_state_preflight(
    operation: &PreparedDeviceDocumentLifecycleMutation,
    head: crate::application::DocumentHead,
) -> DocumentLifecyclePreflight {
    match &operation.mutation {
        DeviceDocumentLifecycleMutation::Rename { .. } if !head.active => {
            DocumentLifecyclePreflight::Rejected(DeviceMutationRejection::NotFound)
        }
        DeviceDocumentLifecycleMutation::Rename { logical_path }
            if logical_path == &head.logical_path =>
        {
            DocumentLifecyclePreflight::Unchanged(head)
        }
        DeviceDocumentLifecycleMutation::Tombstone if !head.active => {
            DocumentLifecyclePreflight::Unchanged(head)
        }
        DeviceDocumentLifecycleMutation::Rename { .. }
        | DeviceDocumentLifecycleMutation::Tombstone => DocumentLifecyclePreflight::Write,
    }
}

fn apply_document_lifecycle_write(
    transaction: &Transaction<'_>,
    operation: &PreparedDeviceDocumentLifecycleMutation,
    created_at_us: i64,
) -> Result<DeviceMutationResult, StoreError> {
    transaction
        .execute_batch("SAVEPOINT device_mutation_apply")
        .map_err(StoreError::storage)?;
    let result = match &operation.mutation {
        DeviceDocumentLifecycleMutation::Rename { logical_path } => rename_document_in(
            transaction,
            &operation.document_id,
            logical_path,
            operation.expected_revision,
            created_at_us,
            Some(&operation.identity.device_id),
        ),
        DeviceDocumentLifecycleMutation::Tombstone => tombstone_document_in(
            transaction,
            &operation.document_id,
            operation.expected_revision,
            created_at_us,
            Some(&operation.identity.device_id),
        ),
    };
    finish_document_lifecycle_write(transaction, operation, result)
}

fn finish_document_lifecycle_write(
    transaction: &Transaction<'_>,
    operation: &PreparedDeviceDocumentLifecycleMutation,
    result: Result<crate::application::DocumentHead, StoreError>,
) -> Result<DeviceMutationResult, StoreError> {
    match result {
        Ok(head) => {
            transaction
                .execute_batch("RELEASE device_mutation_apply")
                .map_err(StoreError::storage)?;
            Ok(DeviceMutationResult::applied(
                &operation.identity,
                true,
                DeviceMutationProjection::Document(head),
                mutation_cursor(transaction)?,
            ))
        }
        Err(error) => rejected_document_lifecycle(transaction, &operation.identity, error),
    }
}

fn rejected_document_lifecycle(
    transaction: &Transaction<'_>,
    identity: &DeviceMutationIdentity,
    error: StoreError,
) -> Result<DeviceMutationResult, StoreError> {
    let Some(rejection) = lifecycle_rejection(&error) else {
        return Err(error);
    };
    transaction
        .execute_batch("ROLLBACK TO device_mutation_apply; RELEASE device_mutation_apply")
        .map_err(StoreError::storage)?;
    rejected_result(transaction, identity, rejection)
}

fn lifecycle_rejection(error: &StoreError) -> Option<DeviceMutationRejection> {
    match error {
        StoreError::Conflict { expected, actual } => {
            Some(DeviceMutationRejection::RevisionConflict {
                expected: Some(*expected),
                actual: *actual,
            })
        }
        StoreError::NotFound => Some(DeviceMutationRejection::NotFound),
        StoreError::PathConflict => Some(DeviceMutationRejection::PathConflict),
        StoreError::InactiveReference { .. } | StoreError::InvalidProjection { .. } => {
            Some(DeviceMutationRejection::InvalidContent)
        }
        _ => None,
    }
}

fn rejected_result(
    connection: &Connection,
    identity: &DeviceMutationIdentity,
    rejection: DeviceMutationRejection,
) -> Result<DeviceMutationResult, StoreError> {
    Ok(DeviceMutationResult::rejected(
        identity,
        rejection,
        mutation_cursor(connection)?,
    ))
}

fn mutation_cursor(connection: &Connection) -> Result<DeviceMutationCursor, StoreError> {
    let position = current_content_cursor(connection)?;
    Ok(DeviceMutationCursor {
        server_id: server_id(connection)?,
        position,
        token: replica_cursor_token(connection, position)?,
    })
}

fn insert_receipt(
    transaction: &Transaction<'_>,
    identity: &DeviceMutationIdentity,
    result: &DeviceMutationResult,
    created_at_us: i64,
) -> Result<(), StoreError> {
    let result_json = serde_json::to_string(result).map_err(StoreError::storage)?;
    transaction
        .execute(
            "INSERT INTO device_mutation_receipt(
                 device_id, operation_id, format_version, mutation_kind, request_sha256,
                 request_json, result_kind, result_json, created_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                identity.device_id,
                identity.operation_id,
                identity.format_version,
                identity.mutation_kind.as_str(),
                identity.request_sha256,
                identity.request_json,
                result.result_kind(),
                result_json,
                created_at_us,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};
    use tempfile::tempdir;

    use super::super::{SqliteStore, StoreError};
    use crate::{
        application::{
            DeviceDesiredStateMutation, DeviceMutationProjection, DeviceMutationRejection,
            DeviceMutationResult, PreparedDeviceDocumentMutation, SettingsInput, generate_device,
            prepare_asset, prepare_device_desired_state_mutation, prepare_device_document_mutation,
            prepare_device_document_rename, prepare_device_document_tombstone, prepare_note,
            prepare_thema, thema_content,
        },
        blob::BlobMeta,
        domain::DocumentId,
    };

    #[test]
    fn expected_store_failures_keep_typed_mutation_rejections() {
        assert!(matches!(
            super::document_rejection(&StoreError::NodeIdConflict {
                node_id: "shared".into()
            }),
            Some(DeviceMutationRejection::NodeIdConflict { node_id }) if node_id == "shared"
        ));
        assert!(matches!(
            super::desired_state_rejection(&StoreError::ExpectedAbsent { actual: 7 }),
            Some(DeviceMutationRejection::RevisionConflict {
                expected: None,
                actual: Some(7)
            })
        ));
        assert!(matches!(
            super::desired_state_rejection(&StoreError::PathConflict),
            Some(DeviceMutationRejection::PathConflict)
        ));
        assert!(matches!(
            super::desired_state_rejection(&StoreError::InactiveReference { kind: "asset" }),
            Some(DeviceMutationRejection::InvalidContent)
        ));
        assert!(matches!(
            super::lifecycle_rejection(&StoreError::InactiveReference { kind: "asset" }),
            Some(DeviceMutationRejection::InvalidContent)
        ));
    }

    #[test]
    fn desired_state_mutations_replay_assets_themata_and_settings() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gnosis.sqlite");
        let mut store = SqliteStore::open(&path).unwrap();
        let device = add_device(&mut store, "desired");
        let asset_digest = format!("{:x}", Sha256::digest(b"asset"));

        let asset = prepare_device_desired_state_mutation(
            1,
            "asset-op".into(),
            device.clone(),
            DeviceDesiredStateMutation::PutAsset {
                prepared: prepare_asset(
                    format!("assets/{asset_digest}/a.png"),
                    "a.png".into(),
                    None,
                )
                .unwrap(),
                digest: asset_digest.clone(),
                body_sha256: asset_digest.clone(),
                size_bytes: 5,
                media_type: "image/png".into(),
            },
        )
        .unwrap();
        let first_asset = store
            .apply_device_desired_state_mutation(&asset, 1)
            .unwrap();

        let thema = prepare_device_desired_state_mutation(
            1,
            "thema-op".into(),
            device.clone(),
            DeviceDesiredStateMutation::PutThema {
                prepared: prepare_thema(
                    Some("thema".into()),
                    thema_content(
                        "basic",
                        "Question".into(),
                        vec![],
                        vec!["Answer".into()],
                        None,
                        None,
                        None,
                    )
                    .unwrap(),
                    vec![],
                    vec![],
                    vec![],
                    None,
                )
                .unwrap(),
                initial_review_day: 20260719,
            },
        )
        .unwrap();
        let first_thema = store
            .apply_device_desired_state_mutation(&thema, 3)
            .unwrap();
        let mut thema_retry = thema.clone();
        let DeviceDesiredStateMutation::PutThema {
            initial_review_day, ..
        } = &mut thema_retry.mutation
        else {
            unreachable!()
        };
        *initial_review_day += 1;

        let settings = prepare_device_desired_state_mutation(
            1,
            "settings-op".into(),
            device.clone(),
            DeviceDesiredStateMutation::ReplaceSettings {
                input: SettingsInput {
                    timezone: "UTC".into(),
                    day_boundary_hour: 4,
                    desired_retention: 0.9,
                }
                .validate()
                .unwrap(),
                expected_revision: 1,
            },
        )
        .unwrap();
        let first_settings = store
            .apply_device_desired_state_mutation(&settings, 5)
            .unwrap();
        drop(store);

        let mut store = SqliteStore::open(path).unwrap();
        let replay = store
            .apply_device_desired_state_mutation(&asset, 6)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(first_asset.result, replay.result);
        let replay = store
            .apply_device_desired_state_mutation(&thema_retry, 7)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(first_thema.result, replay.result);
        let replay = store
            .apply_device_desired_state_mutation(&settings, 8)
            .unwrap();
        assert!(replay.replayed);
        assert_eq!(first_settings.result, replay.result);
        let changed_asset = prepare_device_desired_state_mutation(
            1,
            "asset-op".into(),
            device.clone(),
            DeviceDesiredStateMutation::PutAsset {
                prepared: prepare_asset(
                    format!("assets/{asset_digest}/a.png"),
                    "a.png".into(),
                    None,
                )
                .unwrap(),
                digest: asset_digest.clone(),
                body_sha256: asset_digest,
                size_bytes: 5,
                media_type: "image/jpeg".into(),
            },
        )
        .unwrap();
        let changed_thema = prepare_device_desired_state_mutation(
            1,
            "thema-op".into(),
            device.clone(),
            DeviceDesiredStateMutation::PutThema {
                prepared: prepare_thema(
                    Some("thema".into()),
                    thema_content(
                        "basic",
                        "Changed".into(),
                        vec![],
                        vec!["Answer".into()],
                        None,
                        None,
                        None,
                    )
                    .unwrap(),
                    vec![],
                    vec![],
                    vec![],
                    None,
                )
                .unwrap(),
                initial_review_day: 20260719,
            },
        )
        .unwrap();
        let changed_settings = prepare_device_desired_state_mutation(
            1,
            "settings-op".into(),
            device,
            DeviceDesiredStateMutation::ReplaceSettings {
                input: SettingsInput {
                    timezone: "Europe/Athens".into(),
                    day_boundary_hour: 4,
                    desired_retention: 0.9,
                }
                .validate()
                .unwrap(),
                expected_revision: 1,
            },
        )
        .unwrap();
        for changed in [&changed_asset, &changed_thema, &changed_settings] {
            assert!(matches!(
                store.apply_device_desired_state_mutation(changed, 9),
                Err(StoreError::IdempotencyConflict)
            ));
        }
        assert_eq!(row_count(&store, "device_mutation_receipt"), 3);
    }

    #[test]
    fn durable_thema_create_requires_absence_and_replays_conflict() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gnosis.sqlite");
        let mut store = SqliteStore::open(&path).unwrap();
        let device = add_device(&mut store, "thema-create-conflict");
        let prepared = |question: &str| {
            prepare_thema(
                Some("thema".into()),
                thema_content(
                    "basic",
                    question.into(),
                    vec![],
                    vec!["Answer".into()],
                    None,
                    None,
                    None,
                )
                .unwrap(),
                vec![],
                vec![],
                vec![],
                None,
            )
            .unwrap()
        };
        store
            .put_thema(&prepared("Question"), 20260719, 1, None)
            .unwrap();
        let operation = |operation_id: &str, question: &str| {
            prepare_device_desired_state_mutation(
                1,
                operation_id.into(),
                device.clone(),
                DeviceDesiredStateMutation::PutThema {
                    prepared: prepared(question),
                    initial_review_day: 20260719,
                },
            )
            .unwrap()
        };
        let same = operation("same", "Question");
        let changed = operation("changed", "Changed");
        let before = store.get_thema("thema").unwrap().unwrap();
        let content_changes = row_count(&store, "content_change");
        let history = row_count(&store, "thema_revision");

        let same_result = store.apply_device_desired_state_mutation(&same, 2).unwrap();
        let changed_result = store
            .apply_device_desired_state_mutation(&changed, 3)
            .unwrap();
        for result in [&same_result, &changed_result] {
            assert!(!result.replayed);
            assert!(matches!(
                result.result,
                DeviceMutationResult::Rejected {
                    rejection: DeviceMutationRejection::RevisionConflict {
                        expected: None,
                        actual: Some(1),
                    },
                    ..
                }
            ));
        }
        assert_eq!(store.get_thema("thema").unwrap().unwrap(), before);
        assert_eq!(row_count(&store, "content_change"), content_changes);
        assert_eq!(row_count(&store, "thema_revision"), history);
        drop(store);

        let mut store = SqliteStore::open(path).unwrap();
        for (operation, retained) in [(&same, &same_result), (&changed, &changed_result)] {
            let replay = store
                .apply_device_desired_state_mutation(operation, 4)
                .unwrap();
            assert!(replay.replayed);
            assert_eq!(replay.result, retained.result);
        }
        assert_eq!(store.get_thema("thema").unwrap().unwrap(), before);
        assert_eq!(row_count(&store, "content_change"), content_changes);
        assert_eq!(row_count(&store, "thema_revision"), history);
    }

    #[test]
    fn desired_state_asset_rejects_existing_blob_metadata_mismatch() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = add_device(&mut store, "asset-metadata");
        let digest = format!("{:x}", Sha256::digest(b"asset"));
        let logical_path = format!("assets/{digest}/a.png");
        let operation = |operation_id: &str, media_type: &str, expected_revision| {
            prepare_device_desired_state_mutation(
                1,
                operation_id.into(),
                device.clone(),
                DeviceDesiredStateMutation::PutAsset {
                    prepared: prepare_asset(
                        logical_path.clone(),
                        "a.png".into(),
                        expected_revision,
                    )
                    .unwrap(),
                    digest: digest.clone(),
                    body_sha256: digest.clone(),
                    size_bytes: 5,
                    media_type: media_type.into(),
                },
            )
            .unwrap()
        };
        store
            .apply_device_desired_state_mutation(&operation("first", "image/png", None), 1)
            .unwrap();

        let result = store
            .apply_device_desired_state_mutation(&operation("second", "image/jpeg", Some(1)), 2)
            .unwrap();

        assert!(matches!(
            result.result,
            DeviceMutationResult::Rejected {
                rejection: DeviceMutationRejection::InvalidContent,
                ..
            }
        ));
        assert_eq!(
            store.asset_head(&logical_path).unwrap().unwrap().media_type,
            Some("image/png".into())
        );
        assert_eq!(row_count(&store, "device_mutation_receipt"), 2);
    }

    #[test]
    fn document_mutation_replays_and_rejects_changed_key_reuse() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = add_device(&mut store, "first");
        let first = generated_operation(&device, "operation", "note.org", "First");
        let first_document_id = first.prepared.document_id.to_string();
        let applied = store
            .apply_device_document_mutation(&first, Some(&blob(&first)), 2)
            .unwrap();
        let retry = generated_operation(&device, "operation", "note.org", "First");
        let replay = store
            .apply_device_document_mutation(&retry, Some(&blob(&retry)), 3)
            .unwrap();
        let changed = generated_operation(&device, "operation", "note.org", "Changed");

        assert!(!applied.replayed);
        assert!(replay.replayed);
        assert_eq!(replay.result, applied.result);
        assert_applied_document(&applied.result, &first_document_id, 1, true);
        assert!(matches!(
            store.apply_device_document_mutation(&changed, Some(&blob(&changed)), 4),
            Err(StoreError::IdempotencyConflict)
        ));
        assert_eq!(row_count(&store, "document_revision"), 1);
        assert_eq!(row_count(&store, "device_mutation_receipt"), 1);
    }

    #[test]
    fn document_mutation_identity_is_scoped_to_the_device() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let first_device = add_device(&mut store, "first");
        let second_device = add_device(&mut store, "second");
        let first = operation(
            &first_device,
            "shared",
            "first-document",
            "first.org",
            "First",
            None,
        );
        let second = operation(
            &second_device,
            "shared",
            "second-document",
            "second.org",
            "Second",
            None,
        );

        store
            .apply_device_document_mutation(&first, Some(&blob(&first)), 3)
            .unwrap();
        let result = store
            .apply_device_document_mutation(&second, Some(&blob(&second)), 4)
            .unwrap();

        assert_applied_document(&result.result, "second-document", 1, true);
        assert_eq!(row_count(&store, "device_mutation_receipt"), 2);
    }

    #[test]
    fn document_create_retains_an_explicit_path_conflict() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = add_device(&mut store, "device");
        let first = operation(&device, "first", "first", "note.org", "First", None);
        let second = operation(&device, "second", "second", "note.org", "Second", None);
        store
            .apply_device_document_mutation(&first, Some(&blob(&first)), 2)
            .unwrap();

        let result = store
            .apply_device_document_mutation(&second, None, 3)
            .unwrap();

        assert!(matches!(
            result.result,
            DeviceMutationResult::Rejected {
                rejection: DeviceMutationRejection::PathConflict,
                ..
            }
        ));
        assert_eq!(row_count(&store, "document_revision"), 1);
        assert_eq!(row_count(&store, "content_change"), 1);
        assert_eq!(row_count(&store, "device_mutation_receipt"), 2);
    }

    #[test]
    fn document_mutation_replay_survives_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gnosis.sqlite");
        let (operation, first) = {
            let mut store = SqliteStore::open(&path).unwrap();
            let device = add_device(&mut store, "device");
            let operation = operation(&device, "restart", "document", "note.org", "Note", None);
            let first = store
                .apply_device_document_mutation(&operation, Some(&blob(&operation)), 2)
                .unwrap();
            (operation, first)
        };
        let mut reopened = SqliteStore::open(path).unwrap();

        let replay = reopened
            .apply_device_document_mutation(&operation, Some(&blob(&operation)), 3)
            .unwrap();

        assert!(replay.replayed);
        assert_eq!(replay.result, first.result);
        assert_eq!(row_count(&reopened, "document_revision"), 1);
    }

    #[test]
    fn document_rename_replays_without_advancing_twice() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = add_device(&mut store, "device");
        let create = operation(&device, "create", "document", "note.org", "Note", None);
        store
            .apply_device_document_mutation(&create, Some(&blob(&create)), 2)
            .unwrap();
        let rename = prepare_device_document_rename(
            1,
            "rename".into(),
            device,
            DocumentId::new("document").unwrap(),
            "renamed.org".into(),
            1,
        )
        .unwrap();

        let first = store
            .apply_device_document_lifecycle_mutation(&rename, 3)
            .unwrap();
        let rejected = prepare_device_document_rename(
            1,
            "rejected-rename".into(),
            rename.identity.device_id.clone(),
            DocumentId::new("document").unwrap(),
            "other.org".into(),
            1,
        )
        .unwrap();
        let rejected_first = store
            .apply_device_document_lifecycle_mutation(&rejected, 4)
            .unwrap();
        let tombstone = prepare_device_document_tombstone(
            1,
            "tombstone-after-rename".into(),
            rename.identity.device_id.clone(),
            DocumentId::new("document").unwrap(),
            2,
        )
        .unwrap();
        store
            .apply_device_document_lifecycle_mutation(&tombstone, 5)
            .unwrap();
        let retry = store
            .apply_device_document_lifecycle_mutation(&rename, 6)
            .unwrap();
        let rejected_retry = store
            .apply_device_document_lifecycle_mutation(&rejected, 7)
            .unwrap();

        assert!(!first.replayed);
        assert!(retry.replayed);
        assert_eq!(retry.result, first.result);
        assert!(!rejected_first.replayed);
        assert!(rejected_retry.replayed);
        assert_eq!(rejected_retry.result, rejected_first.result);
        assert_applied_document(&first.result, "document", 2, true);
        assert_eq!(row_count(&store, "document_revision"), 3);
        assert_eq!(row_count(&store, "content_change"), 3);
        assert_eq!(row_count(&store, "device_mutation_receipt"), 4);
    }

    #[test]
    fn document_tombstone_replays_without_advancing_twice() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = add_device(&mut store, "device");
        let create = operation(&device, "create", "document", "note.org", "Note", None);
        store
            .apply_device_document_mutation(&create, Some(&blob(&create)), 2)
            .unwrap();
        let tombstone = prepare_device_document_tombstone(
            1,
            "tombstone".into(),
            device,
            DocumentId::new("document").unwrap(),
            1,
        )
        .unwrap();

        let first = store
            .apply_device_document_lifecycle_mutation(&tombstone, 3)
            .unwrap();
        let retry = store
            .apply_device_document_lifecycle_mutation(&tombstone, 4)
            .unwrap();

        assert!(!first.replayed);
        assert!(retry.replayed);
        assert_eq!(retry.result, first.result);
        let DeviceMutationResult::Applied { authoritative, .. } = &first.result else {
            panic!("expected applied tombstone");
        };
        let DeviceMutationProjection::Document(head) = authoritative else {
            panic!("expected document projection")
        };
        assert_eq!(head.revision, 2);
        assert!(!head.active);
        assert_eq!(row_count(&store, "document_revision"), 2);
        assert_eq!(row_count(&store, "content_change"), 2);
    }

    #[test]
    fn document_tombstone_noop_retains_the_current_projection_and_cursor() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = add_device(&mut store, "device");
        let create = operation(&device, "create", "document", "note.org", "Note", None);
        store
            .apply_device_document_mutation(&create, Some(&blob(&create)), 2)
            .unwrap();
        let first = prepare_device_document_tombstone(
            1,
            "first-tombstone".into(),
            device.clone(),
            DocumentId::new("document").unwrap(),
            1,
        )
        .unwrap();
        store
            .apply_device_document_lifecycle_mutation(&first, 3)
            .unwrap();
        let noop = prepare_device_document_tombstone(
            1,
            "tombstone-noop".into(),
            device,
            DocumentId::new("document").unwrap(),
            2,
        )
        .unwrap();

        let result = store
            .apply_device_document_lifecycle_mutation(&noop, 4)
            .unwrap();
        let retry = store
            .apply_device_document_lifecycle_mutation(&noop, 5)
            .unwrap();

        assert!(!result.replayed);
        assert!(retry.replayed);
        assert_eq!(retry.result, result.result);
        assert_applied_document(&result.result, "document", 2, false);
        let DeviceMutationResult::Applied { cursor, .. } = result.result else {
            panic!("expected applied no-op");
        };
        assert_eq!(cursor.position, 2);
        assert_eq!(row_count(&store, "document_revision"), 2);
        assert_eq!(row_count(&store, "content_change"), 2);
        assert_eq!(row_count(&store, "device_mutation_receipt"), 3);
    }

    #[test]
    fn document_rename_retains_an_explicit_path_conflict() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = add_device(&mut store, "device");
        for (operation_id, document_id, path) in [
            ("first", "first-document", "first.org"),
            ("second", "second-document", "second.org"),
        ] {
            let create = operation(&device, operation_id, document_id, path, operation_id, None);
            store
                .apply_device_document_mutation(&create, Some(&blob(&create)), 2)
                .unwrap();
        }
        let rename = prepare_device_document_rename(
            1,
            "collision".into(),
            device,
            DocumentId::new("first-document").unwrap(),
            "second.org".into(),
            1,
        )
        .unwrap();

        let result = store
            .apply_device_document_lifecycle_mutation(&rename, 3)
            .unwrap();

        assert!(matches!(
            result.result,
            DeviceMutationResult::Rejected {
                rejection: DeviceMutationRejection::PathConflict,
                ..
            }
        ));
        assert_eq!(row_count(&store, "document_revision"), 2);
        assert_eq!(row_count(&store, "content_change"), 2);
        assert_eq!(row_count(&store, "device_mutation_receipt"), 3);
    }

    #[test]
    fn document_rename_noop_retains_the_current_projection_and_cursor() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = add_device(&mut store, "device");
        let create = operation(&device, "create", "document", "note.org", "Note", None);
        store
            .apply_device_document_mutation(&create, Some(&blob(&create)), 2)
            .unwrap();
        let rename = prepare_device_document_rename(
            1,
            "rename-noop".into(),
            device,
            DocumentId::new("document").unwrap(),
            "note.org".into(),
            1,
        )
        .unwrap();

        let first = store
            .apply_device_document_lifecycle_mutation(&rename, 3)
            .unwrap();
        let retry = store
            .apply_device_document_lifecycle_mutation(&rename, 4)
            .unwrap();

        assert!(retry.replayed);
        assert_eq!(retry.result, first.result);
        assert_applied_document(&first.result, "document", 1, false);
        let DeviceMutationResult::Applied { cursor, .. } = first.result else {
            panic!("expected applied no-op");
        };
        assert_eq!(cursor.position, 1);
        assert_eq!(row_count(&store, "document_revision"), 1);
        assert_eq!(row_count(&store, "content_change"), 1);
        assert_eq!(row_count(&store, "device_mutation_receipt"), 2);
    }

    #[test]
    fn stale_document_result_is_retained_after_later_canonical_change() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device = add_device(&mut store, "device");
        let create = operation(&device, "create", "document", "note.org", "First", None);
        store
            .apply_device_document_mutation(&create, Some(&blob(&create)), 2)
            .unwrap();
        let stale = operation(&device, "stale", "document", "note.org", "Stale", Some(0));
        let conflict = store
            .apply_device_document_mutation(&stale, Some(&blob(&stale)), 3)
            .unwrap();
        let update = operation(
            &device,
            "update",
            "document",
            "note.org",
            "Current",
            Some(1),
        );
        store
            .apply_device_document_mutation(&update, Some(&blob(&update)), 4)
            .unwrap();
        let replay = store
            .apply_device_document_mutation(&stale, Some(&blob(&stale)), 5)
            .unwrap();

        assert_rejected_revision(&conflict.result, Some(0), Some(1), 1);
        assert!(replay.replayed);
        assert_eq!(replay.result, conflict.result);
        assert_eq!(
            store
                .document_head(&update.prepared.document_id)
                .unwrap()
                .unwrap()
                .revision,
            2
        );
    }

    #[test]
    fn failed_receipt_insert_rolls_back_document_and_feed() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let operation = operation(
            "missing-device",
            "rollback",
            "document",
            "note.org",
            "Note",
            None,
        );

        assert!(matches!(
            store.apply_device_document_mutation(&operation, Some(&blob(&operation)), 1),
            Err(StoreError::Storage(_))
        ));
        assert_eq!(row_count(&store, "document"), 0);
        assert_eq!(row_count(&store, "document_revision"), 0);
        assert_eq!(row_count(&store, "content_change"), 0);
        assert_eq!(row_count(&store, "blob"), 0);
    }

    fn add_device(store: &mut SqliteStore, name: &str) -> String {
        let device = generate_device(name.into(), vec!["content:write".into()]).unwrap();
        let id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        id
    }

    fn operation(
        device_id: &str,
        operation_id: &str,
        document_id: &str,
        logical_path: &str,
        title: &str,
        expected_revision: Option<u64>,
    ) -> PreparedDeviceDocumentMutation {
        let note = prepare_note(
            Some(document_id.into()),
            logical_path.into(),
            format!("#+title: {title}\n"),
            expected_revision,
        )
        .unwrap();
        prepare_device_document_mutation(1, operation_id.into(), device_id.into(), note).unwrap()
    }

    fn generated_operation(
        device_id: &str,
        operation_id: &str,
        logical_path: &str,
        title: &str,
    ) -> PreparedDeviceDocumentMutation {
        let note = prepare_note(
            None,
            logical_path.into(),
            format!("#+title: {title}\n"),
            None,
        )
        .unwrap();
        prepare_device_document_mutation(1, operation_id.into(), device_id.into(), note).unwrap()
    }

    fn blob(operation: &PreparedDeviceDocumentMutation) -> BlobMeta {
        BlobMeta {
            sha256: format!("{:x}", Sha256::digest(operation.prepared.source.as_bytes())),
            size_bytes: operation.prepared.source.len() as u64,
            media_type: "text/org; charset=utf-8".into(),
        }
    }

    fn assert_applied_document(
        result: &DeviceMutationResult,
        document_id: &str,
        revision: u64,
        changed: bool,
    ) {
        let DeviceMutationResult::Applied {
            changed: actual_changed,
            authoritative: DeviceMutationProjection::Document(document),
            ..
        } = result
        else {
            panic!("expected applied document result");
        };
        assert_eq!(document.document_id, document_id);
        assert_eq!(document.revision, revision);
        assert_eq!(*actual_changed, changed);
    }

    fn assert_rejected_revision(
        result: &DeviceMutationResult,
        expected: Option<u64>,
        actual: Option<u64>,
        cursor: u64,
    ) {
        let DeviceMutationResult::Rejected {
            rejection:
                DeviceMutationRejection::RevisionConflict {
                    expected: stored_expected,
                    actual: stored_actual,
                },
            cursor: stored_cursor,
            ..
        } = result
        else {
            panic!("expected revision conflict");
        };
        assert_eq!((*stored_expected, *stored_actual), (expected, actual));
        assert_eq!(stored_cursor.position, cursor);
    }

    fn row_count(store: &SqliteStore, table: &str) -> u64 {
        store
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }
}
