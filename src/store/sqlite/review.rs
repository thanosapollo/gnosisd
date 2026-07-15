use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use super::{
    DeviceOperationInsert, SqliteStore, StoreError, advance_device_sequence,
    insert_device_operation, load_thema, replayed_device_operation, require_device_sequence,
    to_u32, to_u64, u64_to_i64,
};
use crate::{
    application::{
        AnswerInput, MAX_REVIEW_PACK_BYTES, OfflineConflictReason, OfflineReviewOperation,
        OfflineReviewResult, ParametersSnapshot, PresentationSnapshot, ReconciliationStatus,
        ReviewAttemptView, ReviewPackAsset, ReviewPackItem, ReviewPackView, ReviewSessionMode,
        ReviewSessionView, ReviewSource, ReviewSourceKind, ReviewStateSnapshot, TransitionSnapshot,
        presentation_content, review_day_at, validate_offline_thema_assets,
    },
    domain::{GradeError, MATCHER_VERSION, compute_transition, fuzz_delta, grade},
};

const MAX_REVIEW_PACK_ITEMS: usize = 500;

impl SqliteStore {
    pub(crate) fn create_review_session(
        &mut self,
        source: &ReviewSource,
        target_count: Option<u32>,
        owner_device_id: Option<&str>,
        started_at_us: u64,
    ) -> Result<ReviewSessionView, StoreError> {
        require_no_active_session(&self.connection)?;
        let captured = captured_settings(&self.connection)?;
        let review_day = review_day_at(
            started_at_us,
            &captured.timezone,
            captured.day_boundary_hour,
        )
        .map_err(StoreError::storage)?;
        let mut queue = select_queue(&self.connection, source, review_day)?;
        if let Some(target) = target_count {
            queue.truncate(target as usize);
        }
        if queue.is_empty() {
            return Err(StoreError::NotFound);
        }
        let session_id = Uuid::now_v7().to_string();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        insert_session(
            &transaction,
            &session_id,
            source,
            target_count,
            &captured,
            SessionAccess::Online(owner_device_id),
            started_at_us,
        )?;
        insert_session_items(&transaction, &session_id, &queue)?;
        transaction.commit().map_err(StoreError::storage)?;
        self.review_session(&session_id)?
            .ok_or(StoreError::NotFound)
    }

    pub(crate) fn review_session(
        &self,
        session_id: &str,
    ) -> Result<Option<ReviewSessionView>, StoreError> {
        load_session(&self.connection, session_id)
    }

    pub(crate) fn create_review_pack(
        &mut self,
        format_version: u32,
        source: &ReviewSource,
        target_count: Option<u32>,
        device_id: &str,
        issued_at_us: u64,
    ) -> Result<ReviewPackView, StoreError> {
        if format_version != 1 {
            return Err(StoreError::InvalidReviewState);
        }
        let target_count = Some(target_count.unwrap_or(100));
        if !target_count.is_some_and(|count| (1..=MAX_REVIEW_PACK_ITEMS as u32).contains(&count))
            || source.kind == ReviewSourceKind::Manual
                && source.manual_ids.len() > MAX_REVIEW_PACK_ITEMS
        {
            return Err(StoreError::InvalidReviewState);
        }
        require_review_device(&self.connection, device_id)?;
        if let Some(pack) = self.active_review_pack(Some(device_id))? {
            let session =
                load_session(&self.connection, &pack.session_id)?.ok_or(StoreError::NotFound)?;
            if session.source == *source && session.target_count == target_count {
                return Ok(pack);
            }
            return Err(StoreError::ActiveReviewSession);
        }
        require_no_active_session(&self.connection)?;
        let captured = captured_settings(&self.connection)?;
        let review_day =
            review_day_at(issued_at_us, &captured.timezone, captured.day_boundary_hour)
                .map_err(StoreError::storage)?;
        let queue = bounded_queue(&self.connection, source, target_count, review_day)?;
        let ids = PackIds::new();
        let transaction = self.write_transaction()?;
        let cursor = latest_content_cursor(&transaction)?;
        let (cursor_server_id, cursor_token) = cursor_identity(&transaction, cursor)?;
        insert_offline_session(
            &transaction,
            &ids,
            source,
            target_count,
            &captured,
            device_id,
            issued_at_us,
        )?;
        insert_session_items(&transaction, &ids.session_id, &queue)?;
        insert_review_pack(
            &transaction,
            &ids,
            format_version,
            device_id,
            (cursor, &cursor_server_id, &cursor_token),
            issued_at_us,
        )?;
        capture_review_pack(&transaction, &ids, &queue)?;
        let pack = load_review_pack(&transaction, &ids.pack_id)?.ok_or(StoreError::NotFound)?;
        if serde_json::to_vec(&pack)
            .map_err(StoreError::storage)?
            .len()
            > MAX_REVIEW_PACK_BYTES
        {
            return Err(StoreError::InvalidReviewState);
        }
        transaction.commit().map_err(StoreError::storage)?;
        Ok(pack)
    }

    pub(crate) fn review_pack(
        &self,
        pack_id: &str,
        device_id: Option<&str>,
    ) -> Result<Option<ReviewPackView>, StoreError> {
        let pack = load_review_pack(&self.connection, pack_id)?;
        if let (Some(pack), Some(device_id)) = (&pack, device_id)
            && pack.device_id != device_id
        {
            return Err(StoreError::ReviewOwnership);
        }
        Ok(pack)
    }

    pub(crate) fn active_review_pack(
        &self,
        device_id: Option<&str>,
    ) -> Result<Option<ReviewPackView>, StoreError> {
        let pack_id = self
            .connection
            .query_row(
                "SELECT pack_id FROM review_pack
                 WHERE status = 'active' AND (?1 IS NULL OR device_id = ?1)",
                [device_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StoreError::storage)?;
        pack_id
            .map(|pack_id| load_review_pack(&self.connection, &pack_id))
            .transpose()
            .map(Option::flatten)
    }

    pub(crate) fn abandon_review_pack(
        &mut self,
        pack_id: &str,
        device_id: Option<&str>,
        ended_at_us: u64,
    ) -> Result<ReviewPackView, StoreError> {
        let transaction = self.write_transaction()?;
        let pack = load_review_pack(&transaction, pack_id)?.ok_or(StoreError::NotFound)?;
        require_pack_owner(&pack, device_id)?;
        if pack.status != "active" {
            return Err(StoreError::InvalidReviewState);
        }
        transaction
            .execute(
                "UPDATE review_pack SET status = 'abandoned' WHERE pack_id = ?1",
                [pack_id],
            )
            .map_err(StoreError::storage)?;
        transaction
            .execute(
                "UPDATE review_session SET status = 'aborted', ended_at_us = ?2
                 WHERE session_id = ?1 AND status = 'active'",
                params![pack.session_id, u64_to_i64(ended_at_us, "session end")?],
            )
            .map_err(StoreError::storage)?;
        transaction.commit().map_err(StoreError::storage)?;
        load_review_pack(&self.connection, pack_id)?.ok_or(StoreError::NotFound)
    }

    pub(crate) fn active_review_session(
        &self,
        actor_device_id: Option<&str>,
    ) -> Result<Option<ReviewSessionView>, StoreError> {
        let id = self
            .connection
            .query_row(
                "SELECT session_id FROM review_session
                 WHERE status = 'active'
                   AND (?1 IS NULL OR owner_device_id = ?1)",
                [actor_device_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(StoreError::storage)?;
        match id {
            Some(id) => load_session(&self.connection, &id),
            None => Ok(None),
        }
    }

    pub(crate) fn resume_review_session(
        &mut self,
        session_id: &str,
        actor_device_id: Option<&str>,
    ) -> Result<ReviewSessionView, StoreError> {
        require_session_actor(
            &self.connection,
            session_id,
            actor_device_id,
            ReviewSessionMode::Online,
        )?;
        require_no_active_other(&self.connection, session_id)?;
        let changed = self
            .connection
            .execute(
                "UPDATE review_session SET status = 'active', ended_at_us = NULL
             WHERE session_id = ?1 AND status = 'quit'",
                [session_id],
            )
            .map_err(StoreError::storage)?;
        let current = self
            .review_session(session_id)?
            .ok_or(StoreError::NotFound)?;
        if changed == 0 && current.status != "active" {
            return Err(StoreError::InvalidReviewState);
        }
        self.review_session(session_id)?.ok_or(StoreError::NotFound)
    }

    pub(crate) fn stop_review_session(
        &mut self,
        session_id: &str,
        actor_device_id: Option<&str>,
        abort: bool,
        ended_at_us: u64,
    ) -> Result<ReviewSessionView, StoreError> {
        require_session_actor(
            &self.connection,
            session_id,
            actor_device_id,
            ReviewSessionMode::Online,
        )?;
        let status = if abort { "aborted" } else { "quit" };
        let changed = self
            .connection
            .execute(
                "UPDATE review_session SET status = ?2, ended_at_us = ?3
             WHERE session_id = ?1 AND status = 'active'",
                params![session_id, status, u64_to_i64(ended_at_us, "session end")?],
            )
            .map_err(StoreError::storage)?;
        if changed == 0 {
            return Err(StoreError::InvalidReviewState);
        }
        self.review_session(session_id)?.ok_or(StoreError::NotFound)
    }

    pub(crate) fn begin_review_attempt(
        &mut self,
        session_id: &str,
        actor_device_id: Option<&str>,
        started_at_us: u64,
    ) -> Result<ReviewAttemptView, StoreError> {
        require_session_actor(
            &self.connection,
            session_id,
            actor_device_id,
            ReviewSessionMode::Online,
        )?;
        if let Some(attempt) = current_attempt(&self.connection, session_id)? {
            return Ok(attempt);
        }
        let (position, thema_id) = current_session_item(&self.connection, session_id)?;
        let thema = load_thema(&self.connection, &thema_id)?.ok_or(StoreError::NotFound)?;
        if thema.archived {
            return Err(StoreError::InvalidReviewState);
        }
        let state = load_review_state(&self.connection, &thema_id)?;
        let presentation = presentation(&thema);
        let parameters = resolved_parameters(&self.connection, session_id, &thema.tags)?;
        let attempt_id = Uuid::now_v7().to_string();
        self.connection
            .execute(
                "INSERT INTO review_attempt(
                 attempt_id, session_id, session_position, thema_id, thema_revision, state_revision,
                 content_sha256, presentation_json, parameters_json, matcher_version,
                 desired_suspended, status, started_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, 'pending', ?11)",
                params![
                    attempt_id,
                    session_id,
                    position,
                    thema_id,
                    u64_to_i64(thema.revision, "thema revision")?,
                    u64_to_i64(state.state_revision, "state revision")?,
                    thema.content_sha256,
                    serde_json::to_string(&presentation).map_err(StoreError::storage)?,
                    serde_json::to_string(&ParametersSnapshot::from(&parameters))
                        .map_err(StoreError::storage)?,
                    i64::from(state.suspended),
                    u64_to_i64(started_at_us, "attempt start")?,
                ],
            )
            .map_err(StoreError::storage)?;
        load_attempt(&self.connection, &attempt_id)?.ok_or(StoreError::NotFound)
    }

    pub(crate) fn grade_review_attempt(
        &mut self,
        attempt_id: &str,
        actor_device_id: Option<&str>,
        response: &AnswerInput,
        answered_at_us: u64,
        fuzz_sample: f64,
    ) -> Result<ReviewAttemptView, StoreError> {
        let row = load_attempt_row(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)?;
        require_session_actor(
            &self.connection,
            &row.session_id,
            actor_device_id,
            ReviewSessionMode::Online,
        )?;
        if row.status == "graded" || row.status == "committed" {
            return if row.response.as_ref() == Some(response) {
                load_attempt(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)
            } else {
                Err(StoreError::IdempotencyConflict)
            };
        }
        verify_live_attempt(&self.connection, &row)?;
        let content = presentation_content(&row.presentation).map_err(StoreError::storage)?;
        let result =
            grade(row.matcher_version, &content, &response.clone().into()).map_err(|error| {
                match error {
                    GradeError::ResponseShape | GradeError::InvalidChoice => {
                        StoreError::InvalidReviewResponse
                    }
                    GradeError::MatcherVersion => StoreError::CorruptData {
                        field: "review matcher version",
                    },
                }
            })?;
        let state = load_review_state(&self.connection, &row.thema_id)?;
        let parameters = row.parameters.clone().into();
        let (timezone, boundary) = session_clock(&self.connection, &row.session_id)?;
        let review_day =
            review_day_at(answered_at_us, &timezone, boundary).map_err(StoreError::storage)?;
        let transition = transition_with_sample(
            &state,
            &parameters,
            result.success,
            review_day,
            answered_at_us,
            fuzz_sample,
        )?;
        self.connection
            .execute(
                "UPDATE review_attempt SET response_json = ?2, detected_success = ?3,
                 accepted_success = ?3, proposed_transition_json = ?4, fuzz_sample = ?5,
                 status = 'graded', answered_at_us = ?6 WHERE attempt_id = ?1",
                params![
                    attempt_id,
                    serde_json::to_string(response).map_err(StoreError::storage)?,
                    i64::from(result.success),
                    serde_json::to_string(&TransitionSnapshot::from(&transition))
                        .map_err(StoreError::storage)?,
                    fuzz_sample,
                    u64_to_i64(answered_at_us, "answer timestamp")?,
                ],
            )
            .map_err(StoreError::storage)?;
        load_attempt(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)
    }

    pub(crate) fn override_review_attempt(
        &mut self,
        attempt_id: &str,
        actor_device_id: Option<&str>,
    ) -> Result<ReviewAttemptView, StoreError> {
        override_review_attempt_in(&self.connection, attempt_id, actor_device_id)
    }

    pub(crate) fn override_local_review_attempt(
        &mut self,
        attempt_id: &str,
        actor_device_id: Option<&str>,
        request_id: &str,
        request_sha256: &str,
        created_at_us: u64,
    ) -> Result<ReviewAttemptView, StoreError> {
        self.local_review_action(
            attempt_id,
            actor_device_id,
            request_id,
            request_sha256,
            "override",
            created_at_us,
            override_review_attempt_in,
        )
    }

    pub(crate) fn toggle_attempt_suspension(
        &mut self,
        attempt_id: &str,
        actor_device_id: Option<&str>,
    ) -> Result<ReviewAttemptView, StoreError> {
        toggle_attempt_suspension_in(&self.connection, attempt_id, actor_device_id)
    }

    pub(crate) fn toggle_local_attempt_suspension(
        &mut self,
        attempt_id: &str,
        actor_device_id: Option<&str>,
        request_id: &str,
        request_sha256: &str,
        created_at_us: u64,
    ) -> Result<ReviewAttemptView, StoreError> {
        self.local_review_action(
            attempt_id,
            actor_device_id,
            request_id,
            request_sha256,
            "suspension_toggle",
            created_at_us,
            toggle_attempt_suspension_in,
        )
    }

    fn local_review_action(
        &mut self,
        attempt_id: &str,
        actor_device_id: Option<&str>,
        request_id: &str,
        request_sha256: &str,
        action: &str,
        created_at_us: u64,
        apply: fn(&Connection, &str, Option<&str>) -> Result<ReviewAttemptView, StoreError>,
    ) -> Result<ReviewAttemptView, StoreError> {
        let transaction = self.write_transaction()?;
        if let Some(attempt) = replayed_local_review_action(
            &transaction,
            request_id,
            request_sha256,
            attempt_id,
            action,
        )? {
            transaction.commit().map_err(StoreError::storage)?;
            return Ok(attempt);
        }
        let attempt = apply(&transaction, attempt_id, actor_device_id)?;
        insert_local_review_replay(
            &transaction,
            request_id,
            request_sha256,
            attempt_id,
            action,
            &attempt,
            created_at_us,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(attempt)
    }

    pub(crate) fn commit_review_attempt(
        &mut self,
        attempt_id: &str,
        actor_device_id: Option<&str>,
        committed_at_us: u64,
    ) -> Result<String, StoreError> {
        let row = load_attempt_row(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)?;
        require_session_actor(
            &self.connection,
            &row.session_id,
            actor_device_id,
            ReviewSessionMode::Online,
        )?;
        if let Some(event_id) = committed_event(&self.connection, attempt_id)? {
            return Ok(event_id);
        }
        if row.status != "graded" {
            return Err(StoreError::InvalidReviewState);
        }
        verify_live_attempt(&self.connection, &row)?;
        let mut transition = row
            .transition
            .clone()
            .ok_or(StoreError::InvalidReviewState)?;
        transition.after.suspended = row.desired_suspended;
        let event_id = Uuid::now_v7().to_string();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(StoreError::storage)?;
        commit_state(&transaction, &row, &transition)?;
        insert_review_event(&transaction, &row, &transition, &event_id)?;
        insert_review_state_history(
            &transaction,
            &row.thema_id,
            &transition.after,
            &event_id,
            committed_at_us,
        )?;
        finish_attempt(&transaction, &row, &event_id, committed_at_us)?;
        update_review_day(&transaction, &transition, row.override_count)?;
        super::insert_content_change(
            &transaction,
            "review",
            &row.thema_id,
            transition.after.state_revision,
            "commit",
            u64_to_i64(committed_at_us, "commit timestamp")?,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(event_id)
    }

    pub(crate) fn reconcile_offline_operation(
        &mut self,
        format_version: u32,
        operation: &OfflineReviewOperation,
        canonical_payload: &str,
        actor_device_id: Option<&str>,
        received_at_us: u64,
    ) -> Result<OfflineReviewResult, StoreError> {
        if format_version != 1 {
            return Err(StoreError::InvalidReviewState);
        }
        require_operation_actor(operation, actor_device_id)?;
        let digest = format!("{:x}", Sha256::digest(canonical_payload.as_bytes()));
        let transaction = self.write_transaction()?;
        if replayed_device_operation(
            &transaction,
            &operation.attempt_id,
            "review",
            &digest,
            actor_device_id,
        )? {
            let result = load_offline_result(&transaction, &operation.attempt_id)?.ok_or(
                StoreError::CorruptData {
                    field: "offline review result",
                },
            )?;
            transaction.commit().map_err(StoreError::storage)?;
            return Ok(result);
        }
        let pack =
            load_review_pack(&transaction, &operation.pack_id)?.ok_or(StoreError::NotFound)?;
        require_pack_owner(&pack, Some(&operation.device_id))?;
        if pack.format_version != format_version {
            return Err(StoreError::InvalidReviewState);
        }
        require_device_sequence(
            &transaction,
            &operation.device_id,
            operation.device_sequence,
        )?;
        insert_device_operation(
            &transaction,
            DeviceOperationInsert {
                operation_id: &operation.attempt_id,
                device_id: &operation.device_id,
                device_sequence: operation.device_sequence,
                operation_kind: "review",
                format_version,
                payload_sha256: &digest,
                payload_json: canonical_payload,
                received_at_us,
            },
        )?;
        insert_offline_operation(
            &transaction,
            format_version,
            operation,
            canonical_payload,
            &digest,
            received_at_us,
        )?;
        let result = reconcile_new_operation(&transaction, operation, &pack, received_at_us)?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(result)
    }
}

fn require_operation_actor(
    operation: &OfflineReviewOperation,
    actor_device_id: Option<&str>,
) -> Result<(), StoreError> {
    if actor_device_id.is_none_or(|actor| actor == operation.device_id) {
        Ok(())
    } else {
        Err(StoreError::ReviewOwnership)
    }
}

fn insert_offline_operation(
    transaction: &Transaction<'_>,
    format_version: u32,
    operation: &OfflineReviewOperation,
    payload: &str,
    digest: &str,
    received_at_us: u64,
) -> Result<(), StoreError> {
    let reused = transaction
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM offline_review_operation
                 WHERE device_id = ?1 AND device_sequence = ?2
             )",
            params![
                operation.device_id,
                u64_to_i64(operation.device_sequence, "device sequence")?
            ],
            |row| row.get::<_, bool>(0),
        )
        .map_err(StoreError::storage)?;
    if reused {
        return Err(StoreError::IdempotencyConflict);
    }
    transaction
        .execute(
            "INSERT INTO offline_review_operation(
                 attempt_id, pack_id, device_id, device_sequence, format_version,
                 payload_sha256, payload_json, received_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                operation.attempt_id,
                operation.pack_id,
                operation.device_id,
                u64_to_i64(operation.device_sequence, "device sequence")?,
                i64::from(format_version),
                digest,
                payload,
                u64_to_i64(received_at_us, "operation receipt timestamp")?
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn reconcile_new_operation(
    transaction: &Transaction<'_>,
    operation: &OfflineReviewOperation,
    pack: &ReviewPackView,
    reconciled_at_us: u64,
) -> Result<OfflineReviewResult, StoreError> {
    let session = load_session(transaction, &pack.session_id)?.ok_or(StoreError::NotFound)?;
    let state_item = pack
        .items
        .iter()
        .find(|item| item.position == operation.session_position)
        .or_else(|| pack.items.first())
        .ok_or(StoreError::InvalidReviewState)?;
    let state = load_review_state(transaction, &state_item.thema_id)?;
    let reason = offline_conflict_reason(transaction, operation, pack, &session, &state)?;
    let result = match reason {
        Some(reason) => conflict_result(
            transaction,
            pack.format_version,
            operation,
            session,
            state,
            reason,
        )?,
        None => accept_offline_operation(
            transaction,
            pack.format_version,
            operation,
            state,
            reconciled_at_us,
        )?,
    };
    advance_pack_sequence(transaction, operation, &result)?;
    insert_offline_result(transaction, &result, reconciled_at_us)?;
    Ok(result)
}

fn offline_conflict_reason(
    connection: &Connection,
    operation: &OfflineReviewOperation,
    pack: &ReviewPackView,
    session: &ReviewSessionView,
    state: &crate::domain::ReviewState,
) -> Result<Option<OfflineConflictReason>, StoreError> {
    if pack.status != "active" || session.status != "active" {
        return Ok(Some(OfflineConflictReason::LeaseEnded));
    }
    if operation.session_id != pack.session_id
        || operation.lease_id != pack.lease_id
        || operation.device_id != pack.device_id
    {
        return Ok(Some(OfflineConflictReason::CaptureMismatch));
    }
    if session.current_position != operation.session_position {
        return Ok(Some(OfflineConflictReason::SessionAdvanced));
    }
    let Some(item) = pack
        .items
        .iter()
        .find(|item| item.position == operation.session_position)
    else {
        return Ok(Some(OfflineConflictReason::CaptureMismatch));
    };
    if !operation_matches_item(operation, item) || !operation_transition_is_valid(operation, pack) {
        return Ok(Some(OfflineConflictReason::CaptureMismatch));
    }
    let thema = load_thema(connection, &operation.thema_id)?;
    if thema.as_ref().is_none_or(|thema| {
        thema.archived
            || thema.revision != operation.thema_revision
            || thema.content_sha256 != operation.content_sha256
    }) {
        return Ok(Some(OfflineConflictReason::ContentChanged));
    }
    if state.state_revision != operation.base_state_revision {
        return Ok(Some(OfflineConflictReason::SchedulingChanged));
    }
    Ok(None)
}

fn operation_matches_item(operation: &OfflineReviewOperation, item: &ReviewPackItem) -> bool {
    operation.thema_id == item.thema_id
        && operation.thema_revision == item.thema_revision
        && operation.base_state_revision == item.review_state.state_revision
        && operation.content_sha256 == item.content_sha256
        && operation.presentation == item.presentation
        && operation.parameters == item.parameters
        && operation.matcher_version == item.matcher_version
        && operation.proposed_transition.before == item.review_state
        && operation.proposed_transition.parameters == item.parameters
}

fn operation_transition_is_valid(
    operation: &OfflineReviewOperation,
    pack: &ReviewPackView,
) -> bool {
    if operation.answered_at_us < pack.issued_at_us {
        return false;
    }
    let Ok(review_day) = review_day_at(
        operation.answered_at_us,
        &pack.timezone,
        pack.day_boundary_hour,
    ) else {
        return false;
    };
    if review_day != operation.proposed_transition.review_day {
        return false;
    }
    let Ok(content) = presentation_content(&operation.presentation) else {
        return false;
    };
    let Ok(grade) = grade(
        operation.matcher_version,
        &content,
        &operation.response.clone().into(),
    ) else {
        return false;
    };
    let parity = (operation.detected_success == operation.accepted_success)
        == operation.override_count.is_multiple_of(2);
    let before: crate::domain::ReviewState = operation.proposed_transition.before.clone().into();
    let parameters = operation.parameters.clone().into();
    let Ok(expected) = transition_with_sample(
        &before,
        &parameters,
        operation.accepted_success,
        operation.proposed_transition.review_day,
        operation.answered_at_us,
        operation.fuzz_sample,
    ) else {
        return false;
    };
    grade.success == operation.detected_success
        && parity
        && TransitionSnapshot::from(&expected) == operation.proposed_transition
}

fn conflict_result(
    connection: &Connection,
    format_version: u32,
    operation: &OfflineReviewOperation,
    session: ReviewSessionView,
    state: crate::domain::ReviewState,
    reason: OfflineConflictReason,
) -> Result<OfflineReviewResult, StoreError> {
    let (cursor, cursor_server_id, cursor_token) = cursor_identity_at_head(connection)?;
    Ok(OfflineReviewResult {
        format_version,
        attempt_id: operation.attempt_id.clone(),
        status: ReconciliationStatus::Conflict,
        conflict_reason: Some(reason),
        event_id: None,
        authoritative_state: (&state).into(),
        session,
        cursor,
        cursor_server_id,
        cursor_token,
    })
}

fn accept_offline_operation(
    transaction: &Transaction<'_>,
    format_version: u32,
    operation: &OfflineReviewOperation,
    state: crate::domain::ReviewState,
    reconciled_at_us: u64,
) -> Result<OfflineReviewResult, StoreError> {
    let mut transition = authoritative_transition(operation, &state)?;
    transition.after.suspended = operation.desired_suspended;
    let row = offline_attempt_row(operation, transition.clone());
    insert_accepted_attempt(transaction, operation, &transition)?;
    let event_id = Uuid::now_v7().to_string();
    commit_state(transaction, &row, &transition)?;
    insert_review_event(transaction, &row, &transition, &event_id)?;
    insert_review_state_history(
        transaction,
        &row.thema_id,
        &transition.after,
        &event_id,
        reconciled_at_us,
    )?;
    finish_attempt(transaction, &row, &event_id, reconciled_at_us)?;
    update_review_day(transaction, &transition, operation.override_count)?;
    super::insert_content_change(
        transaction,
        "review",
        &operation.thema_id,
        transition.after.state_revision,
        "commit",
        u64_to_i64(reconciled_at_us, "reconciliation timestamp")?,
    )?;
    let session = load_session(transaction, &operation.session_id)?.ok_or(StoreError::NotFound)?;
    let (cursor, cursor_server_id, cursor_token) = cursor_identity_at_head(transaction)?;
    Ok(OfflineReviewResult {
        format_version,
        attempt_id: operation.attempt_id.clone(),
        status: ReconciliationStatus::Accepted,
        conflict_reason: None,
        event_id: Some(event_id),
        authoritative_state: transition.after,
        session,
        cursor,
        cursor_server_id,
        cursor_token,
    })
}

fn authoritative_transition(
    operation: &OfflineReviewOperation,
    state: &crate::domain::ReviewState,
) -> Result<TransitionSnapshot, StoreError> {
    let parameters = operation.parameters.clone().into();
    let transition = transition_with_sample(
        state,
        &parameters,
        operation.accepted_success,
        operation.proposed_transition.review_day,
        operation.answered_at_us,
        operation.fuzz_sample,
    )?;
    Ok((&transition).into())
}

fn offline_attempt_row(
    operation: &OfflineReviewOperation,
    transition: TransitionSnapshot,
) -> AttemptRow {
    AttemptRow {
        attempt_id: operation.attempt_id.clone(),
        session_id: operation.session_id.clone(),
        thema_id: operation.thema_id.clone(),
        thema_revision: operation.thema_revision,
        state_revision: operation.base_state_revision,
        content_sha256: operation.content_sha256.clone(),
        presentation: operation.presentation.clone(),
        parameters: operation.parameters.clone(),
        matcher_version: operation.matcher_version,
        response: Some(operation.response.clone()),
        detected_success: Some(operation.detected_success),
        accepted_success: Some(operation.accepted_success),
        override_count: operation.override_count,
        fuzz_sample: Some(operation.fuzz_sample),
        transition: Some(transition),
        desired_suspended: operation.desired_suspended,
        status: "graded".into(),
    }
}

fn insert_accepted_attempt(
    transaction: &Transaction<'_>,
    operation: &OfflineReviewOperation,
    transition: &TransitionSnapshot,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO review_attempt(
                 attempt_id, session_id, session_position, thema_id, thema_revision,
                 state_revision, content_sha256, presentation_json, parameters_json,
                 matcher_version, response_json, detected_success, accepted_success,
                 override_count, fuzz_sample, proposed_transition_json, desired_suspended,
                 status, started_at_us, answered_at_us
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, ?17, 'graded', ?18, ?18
             )",
            params![
                operation.attempt_id,
                operation.session_id,
                i64::from(operation.session_position),
                operation.thema_id,
                u64_to_i64(operation.thema_revision, "thema revision")?,
                u64_to_i64(operation.base_state_revision, "state revision")?,
                operation.content_sha256,
                serde_json::to_string(&operation.presentation).map_err(StoreError::storage)?,
                serde_json::to_string(&operation.parameters).map_err(StoreError::storage)?,
                i64::from(operation.matcher_version),
                serde_json::to_string(&operation.response).map_err(StoreError::storage)?,
                i64::from(operation.detected_success),
                i64::from(operation.accepted_success),
                i64::from(operation.override_count),
                operation.fuzz_sample,
                serde_json::to_string(transition).map_err(StoreError::storage)?,
                i64::from(operation.desired_suspended),
                u64_to_i64(operation.answered_at_us, "answer timestamp")?
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn advance_pack_sequence(
    transaction: &Transaction<'_>,
    operation: &OfflineReviewOperation,
    result: &OfflineReviewResult,
) -> Result<(), StoreError> {
    advance_device_sequence(transaction, &operation.device_id, operation.device_sequence)?;
    transaction
        .execute(
            "UPDATE review_pack SET
                 status = CASE WHEN ?2 = 'completed' THEN 'completed' ELSE status END
             WHERE pack_id = ?1",
            params![operation.pack_id, result.session.status],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_offline_result(
    transaction: &Transaction<'_>,
    result: &OfflineReviewResult,
    reconciled_at_us: u64,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO offline_review_result(
                 attempt_id, format_version, status, conflict_reason, event_id,
                 authoritative_state_json, session_json, cursor, reconciled_at_us,
                 cursor_server_id, cursor_token
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                result.attempt_id,
                i64::from(result.format_version),
                result.status.as_str(),
                result.conflict_reason.map(OfflineConflictReason::as_str),
                result.event_id,
                serde_json::to_string(&result.authoritative_state).map_err(StoreError::storage)?,
                serde_json::to_string(&result.session).map_err(StoreError::storage)?,
                u64_to_i64(result.cursor, "reconciliation cursor")?,
                u64_to_i64(reconciled_at_us, "reconciliation timestamp")?,
                result.cursor_server_id,
                result.cursor_token,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn load_offline_result(
    connection: &Connection,
    attempt_id: &str,
) -> Result<Option<OfflineReviewResult>, StoreError> {
    let row = connection
        .query_row(
            "SELECT format_version, status, conflict_reason, event_id,
                    authoritative_state_json, session_json, cursor,
                    cursor_server_id, cursor_token
             FROM offline_review_result WHERE attempt_id = ?1",
            [attempt_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    row.map(|row| materialize_offline_result(attempt_id, row))
        .transpose()
}

type OfflineResultRow = (
    i64,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    i64,
    String,
    String,
);

fn materialize_offline_result(
    attempt_id: &str,
    row: OfflineResultRow,
) -> Result<OfflineReviewResult, StoreError> {
    Ok(OfflineReviewResult {
        format_version: to_u32(row.0, "offline review format version")?,
        attempt_id: attempt_id.to_owned(),
        status: parse_reconciliation_status(&row.1)?,
        conflict_reason: row.2.as_deref().map(parse_conflict_reason).transpose()?,
        event_id: row.3,
        authoritative_state: serde_json::from_str(&row.4).map_err(StoreError::storage)?,
        session: serde_json::from_str(&row.5).map_err(StoreError::storage)?,
        cursor: to_u64(row.6, "reconciliation cursor")?,
        cursor_server_id: row.7,
        cursor_token: row.8,
    })
}

fn parse_reconciliation_status(value: &str) -> Result<ReconciliationStatus, StoreError> {
    match value {
        "accepted" => Ok(ReconciliationStatus::Accepted),
        "conflict" => Ok(ReconciliationStatus::Conflict),
        _ => Err(StoreError::CorruptData {
            field: "reconciliation status",
        }),
    }
}

fn parse_conflict_reason(value: &str) -> Result<OfflineConflictReason, StoreError> {
    match value {
        "lease_ended" => Ok(OfflineConflictReason::LeaseEnded),
        "session_advanced" => Ok(OfflineConflictReason::SessionAdvanced),
        "content_changed" => Ok(OfflineConflictReason::ContentChanged),
        "scheduling_changed" => Ok(OfflineConflictReason::SchedulingChanged),
        "capture_mismatch" => Ok(OfflineConflictReason::CaptureMismatch),
        _ => Err(StoreError::CorruptData {
            field: "offline conflict reason",
        }),
    }
}

fn latest_content_cursor(connection: &Connection) -> Result<u64, StoreError> {
    super::current_content_cursor(connection)
}

fn cursor_identity(connection: &Connection, cursor: u64) -> Result<(String, String), StoreError> {
    Ok((
        super::server_id(connection)?,
        super::replica_cursor_token(connection, cursor)?,
    ))
}

fn cursor_identity_at_head(connection: &Connection) -> Result<(u64, String, String), StoreError> {
    let cursor = latest_content_cursor(connection)?;
    let (server_id, token) = cursor_identity(connection, cursor)?;
    Ok((cursor, server_id, token))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct CapturedSettings {
    timezone: String,
    day_boundary_hour: u8,
    global: ParametersSnapshot,
    rules: BTreeMap<String, Rule>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Rule {
    amnesia: Option<f64>,
    epignosis: Option<f64>,
    agnoia: Option<f64>,
    anagnosis: Option<u64>,
    lethe: Option<u64>,
    proto: Option<Vec<u32>>,
}

struct PackIds {
    pack_id: String,
    session_id: String,
    lease_id: String,
}

impl PackIds {
    fn new() -> Self {
        Self {
            pack_id: Uuid::now_v7().to_string(),
            session_id: Uuid::now_v7().to_string(),
            lease_id: Uuid::now_v7().to_string(),
        }
    }
}

fn require_review_device(connection: &Connection, device_id: &str) -> Result<(), StoreError> {
    let active: bool = connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM device WHERE device_id = ?1 AND revoked_at_us IS NULL
             )",
            [device_id],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)?;
    if active {
        Ok(())
    } else {
        Err(StoreError::ReviewOwnership)
    }
}

fn bounded_queue(
    connection: &Connection,
    source: &ReviewSource,
    target_count: Option<u32>,
    review_day: u32,
) -> Result<Vec<String>, StoreError> {
    let mut queue = select_queue(connection, source, review_day)?;
    if let Some(target) = target_count {
        queue.truncate(target as usize);
    }
    if queue.is_empty() {
        Err(StoreError::NotFound)
    } else {
        Ok(queue)
    }
}

fn insert_offline_session(
    transaction: &Transaction<'_>,
    ids: &PackIds,
    source: &ReviewSource,
    target_count: Option<u32>,
    captured: &CapturedSettings,
    device_id: &str,
    issued_at_us: u64,
) -> Result<(), StoreError> {
    insert_session(
        transaction,
        &ids.session_id,
        source,
        target_count,
        captured,
        SessionAccess::Offline(device_id),
        issued_at_us,
    )
}

fn insert_review_pack(
    transaction: &Transaction<'_>,
    ids: &PackIds,
    format_version: u32,
    device_id: &str,
    cursor: (u64, &str, &str),
    issued_at_us: u64,
) -> Result<(), StoreError> {
    let (position, server_id, token) = cursor;
    transaction
        .execute(
            "INSERT INTO review_pack(
                 pack_id, format_version, session_id, device_id, lease_id,
                 status, cursor, issued_at_us, cursor_server_id, cursor_token
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6, ?7, ?8, ?9)",
            params![
                ids.pack_id,
                i64::from(format_version),
                ids.session_id,
                device_id,
                ids.lease_id,
                u64_to_i64(position, "review pack cursor")?,
                u64_to_i64(issued_at_us, "review pack issue timestamp")?,
                server_id,
                token,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn capture_review_pack(
    transaction: &Transaction<'_>,
    ids: &PackIds,
    queue: &[String],
) -> Result<(), StoreError> {
    let mut captured_bytes = 0;
    for (position, thema_id) in queue.iter().enumerate() {
        let item = capture_pack_item(transaction, &ids.session_id, position, thema_id)?;
        insert_pack_item(transaction, &ids.pack_id, &item, &mut captured_bytes)?;
        insert_pack_assets(transaction, &ids.pack_id, &item.asset_paths)?;
    }
    Ok(())
}

fn capture_pack_item(
    connection: &Connection,
    session_id: &str,
    position: usize,
    thema_id: &str,
) -> Result<ReviewPackItem, StoreError> {
    let thema = load_thema(connection, thema_id)?.ok_or(StoreError::NotFound)?;
    let state = load_review_state(connection, thema_id)?;
    let parameters = resolved_parameters(connection, session_id, &thema.tags)?;
    validate_offline_thema_assets(&thema.content).map_err(|_| StoreError::InvalidReviewState)?;
    let presentation = presentation(&thema);
    Ok(ReviewPackItem {
        position: u32::try_from(position).map_err(StoreError::storage)?,
        thema_id: thema.thema_id,
        thema_revision: thema.revision,
        content_sha256: thema.content_sha256,
        presentation,
        review_state: (&state).into(),
        parameters: (&parameters).into(),
        matcher_version: MATCHER_VERSION,
        asset_paths: thema.asset_paths,
    })
}

fn insert_pack_item(
    transaction: &Transaction<'_>,
    pack_id: &str,
    item: &ReviewPackItem,
    captured_bytes: &mut usize,
) -> Result<(), StoreError> {
    let item_json = serde_json::to_string(item).map_err(StoreError::storage)?;
    *captured_bytes = captured_bytes
        .checked_add(item_json.len())
        .ok_or(StoreError::InvalidReviewState)?;
    if *captured_bytes > MAX_REVIEW_PACK_BYTES {
        return Err(StoreError::InvalidReviewState);
    }
    transaction
        .execute(
            "INSERT INTO review_pack_item(pack_id, position, thema_id, item_json)
             VALUES (?1, ?2, ?3, ?4)",
            params![pack_id, i64::from(item.position), item.thema_id, item_json],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_pack_assets(
    transaction: &Transaction<'_>,
    pack_id: &str,
    asset_paths: &[String],
) -> Result<(), StoreError> {
    for path in asset_paths {
        transaction
            .execute(
                "INSERT OR IGNORE INTO review_pack_asset(
                     pack_id, asset_path, content_sha256, size_bytes, media_type
                 )
                 SELECT ?1, asset.asset_path, asset.blob_sha256, blob.size_bytes, blob.media_type
                 FROM managed_asset AS asset JOIN blob ON blob.content_sha256 = asset.blob_sha256
                 WHERE asset.asset_path = ?2 AND asset.active = 1",
                params![pack_id, path],
            )
            .map_err(StoreError::storage)?;
        let captured: bool = transaction
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM review_pack_asset
                     WHERE pack_id = ?1 AND asset_path = ?2
                 )",
                params![pack_id, path],
                |row| row.get(0),
            )
            .map_err(StoreError::storage)?;
        if !captured {
            return Err(StoreError::InvalidReviewState);
        }
    }
    Ok(())
}

fn captured_settings(connection: &Connection) -> Result<CapturedSettings, StoreError> {
    let (timezone, boundary, version, scheduler): (String, i64, i64, String) = connection.query_row(
        "SELECT timezone, day_boundary_hour, scheduler_version, scheduler_json FROM settings WHERE id = 1",
        [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    ).map_err(StoreError::storage)?;
    let mut global: ParametersSnapshot =
        serde_json::from_str(&scheduler).map_err(StoreError::storage)?;
    global.scheduler_version = to_u32(version, "scheduler version")?;
    let mut statement = connection
        .prepare("SELECT tag, parameters_json FROM tag_rule ORDER BY tag")
        .map_err(StoreError::storage)?;
    let rules = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(StoreError::storage)?
        .map(|row| {
            let (tag, value) = row.map_err(StoreError::storage)?;
            Ok((
                tag,
                serde_json::from_str(&value).map_err(StoreError::storage)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, StoreError>>()?;
    Ok(CapturedSettings {
        timezone,
        day_boundary_hour: u8::try_from(boundary).map_err(|_| StoreError::CorruptData {
            field: "day boundary",
        })?,
        global,
        rules,
    })
}

fn require_no_active_session(connection: &Connection) -> Result<(), StoreError> {
    let active: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM review_session WHERE status = 'active')",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)?;
    if active {
        Err(StoreError::ActiveReviewSession)
    } else {
        Ok(())
    }
}

fn require_no_active_other(connection: &Connection, session_id: &str) -> Result<(), StoreError> {
    let active: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM review_session WHERE status = 'active' AND session_id <> ?1)",
        [session_id], |row| row.get(0),
    ).map_err(StoreError::storage)?;
    if active {
        Err(StoreError::ActiveReviewSession)
    } else {
        Ok(())
    }
}

fn select_queue(
    connection: &Connection,
    source: &ReviewSource,
    review_day: u32,
) -> Result<Vec<String>, StoreError> {
    if source.kind == ReviewSourceKind::Manual {
        return select_manual(connection, &source.manual_ids);
    }
    if source.kind == ReviewSourceKind::Topic {
        return select_topic(
            connection,
            source.topic_node_id.as_deref().unwrap_or_default(),
            source.topic_forward_depth.unwrap_or_default(),
            source.topic_backward_depth.unwrap_or_default(),
        );
    }
    select_filtered(connection, source, review_day)
}

fn select_manual(connection: &Connection, ids: &[String]) -> Result<Vec<String>, StoreError> {
    let mut selected = Vec::new();
    for id in ids {
        let available: bool = connection
            .query_row(
                "SELECT EXISTS(
                 SELECT 1 FROM thema JOIN review_state USING(thema_id)
                 WHERE thema_id = ?1 AND archived_at_us IS NULL AND suspended = 0
             )",
                [id],
                |row| row.get(0),
            )
            .map_err(StoreError::storage)?;
        if available && !selected.contains(id) {
            selected.push(id.clone());
        }
    }
    Ok(selected)
}

fn select_topic(
    connection: &Connection,
    node_id: &str,
    forward_depth: u32,
    backward_depth: u32,
) -> Result<Vec<String>, StoreError> {
    let mut statement = connection
        .prepare(
            "WITH RECURSIVE
             forward(node_id, depth) AS (
                 VALUES (?1, 0)
                 UNION
                 SELECT edge.target_id, forward.depth + 1
                 FROM node_edge AS edge JOIN forward ON edge.source_id = forward.node_id
                 WHERE forward.depth < ?2
             ),
             backward(node_id, depth) AS (
                 VALUES (?1, 0)
                 UNION
                 SELECT edge.source_id, backward.depth + 1
                 FROM node_edge AS edge JOIN backward ON edge.target_id = backward.node_id
                 WHERE backward.depth < ?3
             ),
             graph(node_id) AS (
                 SELECT node_id FROM forward UNION SELECT node_id FROM backward
             )
         SELECT DISTINCT thema.thema_id FROM graph
         JOIN thema_node_link AS link ON link.node_id = graph.node_id
         JOIN thema ON thema.thema_id = link.thema_id
         JOIN review_state AS state ON state.thema_id = thema.thema_id
         WHERE thema.archived_at_us IS NULL AND state.suspended = 0
         ORDER BY (state.review_count > 0), state.next_review_day,
                  coalesce(thema.legacy_id, 9223372036854775807), thema.thema_id",
        )
        .map_err(StoreError::storage)?;
    statement
        .query_map(
            params![node_id, i64::from(forward_depth), i64::from(backward_depth)],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)
}

fn select_filtered(
    connection: &Connection,
    source: &ReviewSource,
    review_day: u32,
) -> Result<Vec<String>, StoreError> {
    let day_clause = match source.kind {
        ReviewSourceKind::Due | ReviewSourceKind::DueTag => "state.next_review_day <= ?1",
        ReviewSourceKind::Overdue => "state.next_review_day < ?1",
        ReviewSourceKind::DueWithoutOverdue => "state.next_review_day = ?1",
        ReviewSourceKind::Tag => "1",
        _ => return Err(StoreError::InvalidReviewState),
    };
    let include = serde_json::to_string(&source.tags).map_err(StoreError::storage)?;
    let exclude = serde_json::to_string(&source.exclude_tags).map_err(StoreError::storage)?;
    let query = format!(
        "SELECT thema.thema_id FROM thema JOIN review_state AS state USING(thema_id)
         WHERE thema.archived_at_us IS NULL AND state.suspended = 0
           AND {day_clause}
           AND (json_array_length(?2) = 0 OR EXISTS(
               SELECT 1 FROM thema_tag
               WHERE thema_id = thema.thema_id AND tag IN (SELECT value FROM json_each(?2))
           ))
           AND NOT EXISTS(
               SELECT 1 FROM thema_tag
               WHERE thema_id = thema.thema_id AND tag IN (SELECT value FROM json_each(?3))
           )
         ORDER BY (state.review_count > 0), state.next_review_day,
                  coalesce(thema.legacy_id, 9223372036854775807), thema.thema_id"
    );
    let mut statement = connection.prepare(&query).map_err(StoreError::storage)?;
    statement
        .query_map(params![i64::from(review_day), include, exclude], |row| {
            row.get(0)
        })
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)
}

enum SessionAccess<'a> {
    Online(Option<&'a str>),
    Offline(&'a str),
}

impl SessionAccess<'_> {
    fn columns(&self) -> (Option<&str>, ReviewSessionMode) {
        match self {
            Self::Online(owner) => (*owner, ReviewSessionMode::Online),
            Self::Offline(device) => (Some(*device), ReviewSessionMode::Offline),
        }
    }
}

fn insert_session(
    transaction: &Transaction<'_>,
    session_id: &str,
    source: &ReviewSource,
    target_count: Option<u32>,
    captured: &CapturedSettings,
    access: SessionAccess<'_>,
    started_at_us: u64,
) -> Result<(), StoreError> {
    let (owner_device_id, mode) = access.columns();
    transaction
        .execute(
            "INSERT INTO review_session(
             session_id, source_kind, source_json, target_count, timezone,
             day_boundary_hour, scheduler_json, status, started_at_us,
             owner_device_id, review_mode
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8, ?9, ?10)",
            params![
                session_id,
                source.kind.as_str(),
                serde_json::to_string(source).map_err(StoreError::storage)?,
                target_count.map(i64::from),
                captured.timezone,
                i64::from(captured.day_boundary_hour),
                serde_json::to_string(captured).map_err(StoreError::storage)?,
                u64_to_i64(started_at_us, "session start")?,
                owner_device_id,
                mode.as_str(),
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_session_items(
    transaction: &Transaction<'_>,
    session_id: &str,
    queue: &[String],
) -> Result<(), StoreError> {
    let mut insert = transaction
        .prepare_cached(
            "INSERT INTO review_session_item(session_id, position, thema_id) VALUES (?1, ?2, ?3)",
        )
        .map_err(StoreError::storage)?;
    for (position, thema_id) in queue.iter().enumerate() {
        insert
            .execute(params![
                session_id,
                i64::try_from(position).map_err(StoreError::storage)?,
                thema_id
            ])
            .map_err(StoreError::storage)?;
    }
    Ok(())
}

fn load_session(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<ReviewSessionView>, StoreError> {
    connection
        .query_row(
            "SELECT session_id, source_json, target_count, status, current_position,
                started_at_us, (SELECT count(*) FROM review_session_item WHERE session_id = ?1),
                owner_device_id, review_mode
         FROM review_session WHERE session_id = ?1",
            [session_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, String>(8)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?
        .map(|row| {
            Ok(ReviewSessionView {
                session_id: row.0,
                owner_device_id: row.7,
                mode: parse_session_mode(&row.8)?,
                source: serde_json::from_str(&row.1).map_err(StoreError::storage)?,
                target_count: row
                    .2
                    .map(|value| to_u32(value, "target count"))
                    .transpose()?,
                status: row.3,
                current_position: to_u32(row.4, "session position")?,
                started_at_us: to_u64(row.5, "session start")?,
                queue_length: to_u32(row.6, "queue length")?,
            })
        })
        .transpose()
}

fn parse_session_mode(value: &str) -> Result<ReviewSessionMode, StoreError> {
    match value {
        "online" => Ok(ReviewSessionMode::Online),
        "offline" => Ok(ReviewSessionMode::Offline),
        _ => Err(StoreError::CorruptData {
            field: "review session mode",
        }),
    }
}

type PackRow = (
    String,
    i64,
    String,
    String,
    String,
    String,
    i64,
    i64,
    i64,
    String,
    i64,
    String,
    String,
);

fn load_review_pack(
    connection: &Connection,
    pack_id: &str,
) -> Result<Option<ReviewPackView>, StoreError> {
    let row = connection
        .query_row(
            "SELECT pack.pack_id, pack.format_version, pack.session_id, pack.device_id,
                    pack.lease_id, pack.status, pack.cursor, pack.issued_at_us, session.current_position,
                    session.timezone, session.day_boundary_hour,
                    pack.cursor_server_id, pack.cursor_token
             FROM review_pack AS pack
             JOIN review_session AS session USING(session_id)
             WHERE pack.pack_id = ?1",
            [pack_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                    row.get(9)?,
                    row.get(10)?,
                    row.get(11)?,
                    row.get(12)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    row.map(|row| materialize_review_pack(connection, row))
        .transpose()
}

fn materialize_review_pack(
    connection: &Connection,
    row: PackRow,
) -> Result<ReviewPackView, StoreError> {
    Ok(ReviewPackView {
        items: load_pack_items(connection, &row.0)?,
        assets: load_pack_assets(connection, &row.0)?,
        format_version: to_u32(row.1, "review pack format version")?,
        pack_id: row.0,
        session_id: row.2,
        device_id: row.3,
        lease_id: row.4,
        status: row.5,
        cursor: to_u64(row.6, "review pack cursor")?,
        cursor_server_id: row.11,
        cursor_token: row.12,
        issued_at_us: to_u64(row.7, "review pack issue timestamp")?,
        current_position: to_u32(row.8, "review pack position")?,
        timezone: row.9,
        day_boundary_hour: u8::try_from(row.10).map_err(|_| StoreError::CorruptData {
            field: "review pack day boundary",
        })?,
    })
}

fn load_pack_items(
    connection: &Connection,
    pack_id: &str,
) -> Result<Vec<ReviewPackItem>, StoreError> {
    let mut statement = connection
        .prepare("SELECT item_json FROM review_pack_item WHERE pack_id = ?1 ORDER BY position")
        .map_err(StoreError::storage)?;
    statement
        .query_map([pack_id], |row| row.get::<_, String>(0))
        .map_err(StoreError::storage)?
        .map(|row| {
            let value = row.map_err(StoreError::storage)?;
            serde_json::from_str(&value).map_err(StoreError::storage)
        })
        .collect()
}

fn load_pack_assets(
    connection: &Connection,
    pack_id: &str,
) -> Result<Vec<ReviewPackAsset>, StoreError> {
    let mut statement = connection
        .prepare(
            "SELECT asset_path, content_sha256, size_bytes, media_type
             FROM review_pack_asset WHERE pack_id = ?1 ORDER BY asset_path",
        )
        .map_err(StoreError::storage)?;
    statement
        .query_map([pack_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .map_err(StoreError::storage)?
        .map(|row| {
            let (logical_path, content_sha256, size_bytes, media_type) =
                row.map_err(StoreError::storage)?;
            Ok(ReviewPackAsset {
                logical_path,
                content_sha256,
                size_bytes: to_u64(size_bytes, "review pack asset size")?,
                media_type,
            })
        })
        .collect()
}

fn require_pack_owner(pack: &ReviewPackView, device_id: Option<&str>) -> Result<(), StoreError> {
    if device_id.is_none_or(|device_id| device_id == pack.device_id) {
        Ok(())
    } else {
        Err(StoreError::ReviewOwnership)
    }
}

fn require_session_actor(
    connection: &Connection,
    session_id: &str,
    actor_device_id: Option<&str>,
    mode: ReviewSessionMode,
) -> Result<(), StoreError> {
    let session = load_session(connection, session_id)?.ok_or(StoreError::NotFound)?;
    if session.mode != mode {
        return Err(StoreError::InvalidReviewState);
    }
    if actor_device_id.is_some_and(|actor| session.owner_device_id.as_deref() != Some(actor)) {
        Err(StoreError::ReviewOwnership)
    } else {
        Ok(())
    }
}

fn current_attempt(
    connection: &Connection,
    session_id: &str,
) -> Result<Option<ReviewAttemptView>, StoreError> {
    let id = connection
        .query_row(
            "SELECT attempt.attempt_id FROM review_attempt AS attempt
         JOIN review_session AS session USING(session_id)
         WHERE attempt.session_id = ?1 AND attempt.session_position = session.current_position
           AND attempt.status <> 'committed'",
            [session_id],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(StoreError::storage)?;
    id.map(|id| load_attempt(connection, &id).map(Option::unwrap))
        .transpose()
}

fn current_session_item(
    connection: &Connection,
    session_id: &str,
) -> Result<(i64, String), StoreError> {
    connection
        .query_row(
            "SELECT item.position, item.thema_id FROM review_session AS session
         JOIN review_session_item AS item ON item.session_id = session.session_id
                                         AND item.position = session.current_position
         WHERE session.session_id = ?1 AND session.status = 'active'",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(StoreError::storage)?
        .ok_or(StoreError::InvalidReviewState)
}

fn presentation(thema: &crate::application::ThemaRecord) -> PresentationSnapshot {
    PresentationSnapshot {
        kind: thema.content.kind.as_str().to_owned(),
        keimenon: thema.content.keimenon.clone(),
        hypothesis: thema.content.hypothesis.clone(),
        answer: thema.content.answer.clone(),
        source_guid: thema.content.source_guid.clone(),
        parathema: thema.content.parathema.clone(),
        review_image: thema.content.review_image.clone(),
        tags: thema.tags.clone(),
    }
}

pub(super) fn load_review_state(
    connection: &Connection,
    thema_id: &str,
) -> Result<crate::domain::ReviewState, StoreError> {
    let snapshot = connection
        .query_row(
            "SELECT state_revision, review_count, legacy_review_count, legacy_unclassified_count,
                last_reviewed_at_us, last_review_day, next_review_day, interval_days,
                consecutive_successes, total_successes, consecutive_failures, total_failures,
                suspended, gnosis_plus, gnosis_minus, gnosis_synolon
         FROM review_state WHERE thema_id = ?1",
            [thema_id],
            |row| {
                Ok(ReviewStateSnapshot {
                    state_revision: super::row_u64(row, 0)?,
                    review_count: super::row_u64(row, 1)?,
                    legacy_review_count: super::row_u64(row, 2)?,
                    legacy_unclassified_count: super::row_u64(row, 3)?,
                    last_reviewed_at_us: row
                        .get::<_, Option<i64>>(4)?
                        .map(u64::try_from)
                        .transpose()
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                4,
                                rusqlite::types::Type::Integer,
                                Box::new(e),
                            )
                        })?,
                    last_review_day: row
                        .get::<_, Option<i64>>(5)?
                        .map(u32::try_from)
                        .transpose()
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                5,
                                rusqlite::types::Type::Integer,
                                Box::new(e),
                            )
                        })?,
                    next_review_day: u32::try_from(row.get::<_, i64>(6)?).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            6,
                            rusqlite::types::Type::Integer,
                            Box::new(e),
                        )
                    })?,
                    interval_days: row
                        .get::<_, Option<i64>>(7)?
                        .map(u32::try_from)
                        .transpose()
                        .map_err(|e| {
                            rusqlite::Error::FromSqlConversionFailure(
                                7,
                                rusqlite::types::Type::Integer,
                                Box::new(e),
                            )
                        })?,
                    consecutive_successes: super::row_u64(row, 8)?,
                    total_successes: super::row_u64(row, 9)?,
                    consecutive_failures: super::row_u64(row, 10)?,
                    total_failures: super::row_u64(row, 11)?,
                    suspended: row.get::<_, bool>(12)?,
                    gnosis_plus: row.get(13)?,
                    gnosis_minus: row.get(14)?,
                    gnosis_synolon: row.get(15)?,
                })
            },
        )
        .optional()
        .map_err(StoreError::storage)?
        .ok_or(StoreError::NotFound)?;
    Ok(snapshot.into())
}

fn resolved_parameters(
    connection: &Connection,
    session_id: &str,
    tags: &[String],
) -> Result<crate::domain::SchedulerParameters, StoreError> {
    let captured: String = connection
        .query_row(
            "SELECT scheduler_json FROM review_session WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)?;
    let captured: CapturedSettings =
        serde_json::from_str(&captured).map_err(StoreError::storage)?;
    let rules = tags
        .iter()
        .filter_map(|tag| captured.rules.get(tag))
        .collect::<Vec<_>>();
    let mut parameters = captured.global;
    parameters.amnesia = rule_max(&rules, |rule| rule.amnesia).unwrap_or(parameters.amnesia);
    parameters.epignosis = rule_max(&rules, |rule| rule.epignosis).unwrap_or(parameters.epignosis);
    parameters.agnoia = rule_max(&rules, |rule| rule.agnoia).unwrap_or(parameters.agnoia);
    parameters.anagnosis = rule_min(&rules, |rule| rule.anagnosis).unwrap_or(parameters.anagnosis);
    parameters.lethe = rule_min(&rules, |rule| rule.lethe).unwrap_or(parameters.lethe);
    let proto = rules
        .iter()
        .filter_map(|rule| rule.proto.as_deref())
        .collect::<Vec<_>>();
    if !proto.is_empty() {
        parameters.proto.clear();
        for values in proto {
            merge_proto(&mut parameters.proto, values);
        }
    }
    Ok(parameters.into())
}

fn rule_max(rules: &[&Rule], value: impl Fn(&Rule) -> Option<f64>) -> Option<f64> {
    rules.iter().filter_map(|rule| value(rule)).reduce(f64::max)
}

fn rule_min(rules: &[&Rule], value: impl Fn(&Rule) -> Option<u64>) -> Option<u64> {
    rules.iter().filter_map(|rule| value(rule)).min()
}

fn merge_proto(current: &mut Vec<u32>, override_values: &[u32]) {
    current.resize(current.len().max(override_values.len()), 0);
    for (index, value) in override_values.iter().enumerate() {
        current[index] = current[index].max(*value);
    }
}

#[derive(Clone)]
struct AttemptRow {
    attempt_id: String,
    session_id: String,
    thema_id: String,
    thema_revision: u64,
    state_revision: u64,
    content_sha256: String,
    presentation: PresentationSnapshot,
    parameters: ParametersSnapshot,
    matcher_version: u32,
    response: Option<AnswerInput>,
    detected_success: Option<bool>,
    accepted_success: Option<bool>,
    override_count: u32,
    fuzz_sample: Option<f64>,
    transition: Option<TransitionSnapshot>,
    desired_suspended: bool,
    status: String,
}

fn load_attempt_row(
    connection: &Connection,
    attempt_id: &str,
) -> Result<Option<AttemptRow>, StoreError> {
    let row = connection
        .query_row(
            "SELECT attempt_id, session_id, thema_id, thema_revision, state_revision, content_sha256,
                presentation_json, parameters_json, matcher_version, response_json, detected_success,
                accepted_success, override_count, fuzz_sample, proposed_transition_json,
                desired_suspended, status
         FROM review_attempt WHERE attempt_id = ?1",
            [attempt_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, Option<String>>(9)?,
                    row.get::<_, Option<bool>>(10)?,
                    row.get::<_, Option<bool>>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, Option<f64>>(13)?,
                    row.get::<_, Option<String>>(14)?,
                    row.get::<_, bool>(15)?,
                    row.get::<_, String>(16)?,
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?;
    row.map(materialize_attempt_row).transpose()
}

type RawAttemptRow = (
    String,
    String,
    String,
    i64,
    i64,
    String,
    String,
    String,
    i64,
    Option<String>,
    Option<bool>,
    Option<bool>,
    i64,
    Option<f64>,
    Option<String>,
    bool,
    String,
);

fn materialize_attempt_row(row: RawAttemptRow) -> Result<AttemptRow, StoreError> {
    Ok(AttemptRow {
        attempt_id: row.0,
        session_id: row.1,
        thema_id: row.2,
        thema_revision: to_u64(row.3, "attempt thema revision")?,
        state_revision: to_u64(row.4, "attempt state revision")?,
        content_sha256: row.5,
        presentation: serde_json::from_str(&row.6).map_err(StoreError::storage)?,
        parameters: serde_json::from_str(&row.7).map_err(StoreError::storage)?,
        matcher_version: to_u32(row.8, "attempt matcher version")?,
        response: row
            .9
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(StoreError::storage)?,
        detected_success: row.10,
        accepted_success: row.11,
        override_count: to_u32(row.12, "override count")?,
        fuzz_sample: row.13,
        transition: row
            .14
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(StoreError::storage)?,
        desired_suspended: row.15,
        status: row.16,
    })
}

fn override_review_attempt_in(
    connection: &Connection,
    attempt_id: &str,
    actor_device_id: Option<&str>,
) -> Result<ReviewAttemptView, StoreError> {
    let row = load_attempt_row(connection, attempt_id)?.ok_or(StoreError::NotFound)?;
    require_session_actor(
        connection,
        &row.session_id,
        actor_device_id,
        ReviewSessionMode::Online,
    )?;
    if row.status != "graded" {
        return Err(StoreError::InvalidReviewState);
    }
    verify_live_attempt(connection, &row)?;
    let accepted = !row.accepted_success.ok_or(StoreError::InvalidReviewState)?;
    let previous = row
        .transition
        .as_ref()
        .ok_or(StoreError::InvalidReviewState)?;
    let state = load_review_state(connection, &row.thema_id)?;
    let parameters = row.parameters.clone().into();
    let transition = transition_with_sample(
        &state,
        &parameters,
        accepted,
        previous.review_day,
        previous.reviewed_at_us,
        row.fuzz_sample.ok_or(StoreError::InvalidReviewState)?,
    )?;
    connection
        .execute(
            "UPDATE review_attempt SET accepted_success = ?2,
             override_count = override_count + 1, proposed_transition_json = ?3
             WHERE attempt_id = ?1",
            params![
                attempt_id,
                i64::from(accepted),
                serde_json::to_string(&TransitionSnapshot::from(&transition))
                    .map_err(StoreError::storage)?,
            ],
        )
        .map_err(StoreError::storage)?;
    load_attempt(connection, attempt_id)?.ok_or(StoreError::NotFound)
}

fn toggle_attempt_suspension_in(
    connection: &Connection,
    attempt_id: &str,
    actor_device_id: Option<&str>,
) -> Result<ReviewAttemptView, StoreError> {
    let row = load_attempt_row(connection, attempt_id)?.ok_or(StoreError::NotFound)?;
    require_session_actor(
        connection,
        &row.session_id,
        actor_device_id,
        ReviewSessionMode::Online,
    )?;
    let changed = connection
        .execute(
            "UPDATE review_attempt SET desired_suspended = 1 - desired_suspended
             WHERE attempt_id = ?1 AND status IN ('pending', 'graded')",
            [attempt_id],
        )
        .map_err(StoreError::storage)?;
    if changed == 0 {
        return Err(StoreError::InvalidReviewState);
    }
    load_attempt(connection, attempt_id)?.ok_or(StoreError::NotFound)
}

fn replayed_local_review_action(
    connection: &Connection,
    request_id: &str,
    request_sha256: &str,
    attempt_id: &str,
    action: &str,
) -> Result<Option<ReviewAttemptView>, StoreError> {
    let replay = connection
        .query_row(
            "SELECT request_sha256, attempt_id, action, result_json
             FROM local_review_replay WHERE request_id = ?1",
            [request_id],
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
    let Some((stored_sha256, stored_attempt, stored_action, result_json)) = replay else {
        return Ok(None);
    };
    if stored_sha256 != request_sha256 || stored_attempt != attempt_id || stored_action != action {
        return Err(StoreError::IdempotencyConflict);
    }
    serde_json::from_str(&result_json)
        .map(Some)
        .map_err(StoreError::storage)
}

fn insert_local_review_replay(
    connection: &Connection,
    request_id: &str,
    request_sha256: &str,
    attempt_id: &str,
    action: &str,
    attempt: &ReviewAttemptView,
    created_at_us: u64,
) -> Result<(), StoreError> {
    connection
        .execute(
            "INSERT INTO local_review_replay(
             request_id, request_sha256, attempt_id, action, result_json, created_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                request_id,
                request_sha256,
                attempt_id,
                action,
                serde_json::to_string(attempt).map_err(StoreError::storage)?,
                u64_to_i64(created_at_us, "local replay timestamp")?,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn load_attempt(
    connection: &Connection,
    attempt_id: &str,
) -> Result<Option<ReviewAttemptView>, StoreError> {
    load_attempt_row(connection, attempt_id).map(|row| {
        row.map(|row| ReviewAttemptView {
            attempt_id: row.attempt_id,
            session_id: row.session_id,
            thema_id: row.thema_id,
            presentation: row.presentation,
            matcher_version: row.matcher_version,
            response: row.response,
            detected_success: row.detected_success,
            accepted_success: row.accepted_success,
            override_count: row.override_count,
            proposed_transition: row.transition,
            desired_suspended: row.desired_suspended,
            status: row.status,
        })
    })
}

fn verify_live_attempt(connection: &Connection, row: &AttemptRow) -> Result<(), StoreError> {
    let active: bool = connection
        .query_row(
            "SELECT status = 'active' FROM review_session WHERE session_id = ?1",
            [&row.session_id],
            |result| result.get(0),
        )
        .optional()
        .map_err(StoreError::storage)?
        .ok_or(StoreError::InvalidReviewState)?;
    if !active {
        return Err(StoreError::InvalidReviewState);
    }
    let thema = load_thema(connection, &row.thema_id)?.ok_or(StoreError::StaleAttempt)?;
    let state = load_review_state(connection, &row.thema_id)?;
    if !thema.archived
        && thema.revision == row.thema_revision
        && thema.content_sha256 == row.content_sha256
        && state.state_revision == row.state_revision
    {
        Ok(())
    } else {
        Err(StoreError::StaleAttempt)
    }
}

fn session_clock(connection: &Connection, session_id: &str) -> Result<(String, u8), StoreError> {
    let (timezone, boundary): (String, i64) = connection
        .query_row(
            "SELECT timezone, day_boundary_hour FROM review_session WHERE session_id = ?1",
            [session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(StoreError::storage)?;
    Ok((
        timezone,
        u8::try_from(boundary).map_err(|_| StoreError::CorruptData {
            field: "day boundary",
        })?,
    ))
}

fn transition_with_sample(
    state: &crate::domain::ReviewState,
    parameters: &crate::domain::SchedulerParameters,
    success: bool,
    review_day: u32,
    reviewed_at_us: u64,
    sample: f64,
) -> Result<crate::domain::Transition, StoreError> {
    let probe = compute_transition(state, parameters, success, review_day, reviewed_at_us, 0.0)
        .map_err(StoreError::storage)?;
    let delta = fuzz_delta(
        probe.algorithm_interval_days,
        parameters.interval_fuzz,
        sample,
    )
    .map_err(StoreError::storage)?;
    compute_transition(
        state,
        parameters,
        success,
        review_day,
        reviewed_at_us,
        delta,
    )
    .map_err(StoreError::storage)
}

fn committed_event(
    connection: &Connection,
    attempt_id: &str,
) -> Result<Option<String>, StoreError> {
    connection
        .query_row(
            "SELECT event_id FROM review_event WHERE attempt_id = ?1",
            [attempt_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::storage)
}

fn commit_state(
    transaction: &Transaction<'_>,
    row: &AttemptRow,
    transition: &TransitionSnapshot,
) -> Result<(), StoreError> {
    let after = &transition.after;
    let changed = transaction
        .execute(
            "UPDATE review_state SET state_revision = ?2, review_count = ?3,
             last_reviewed_at_us = ?4, last_review_day = ?5, next_review_day = ?6,
             interval_days = ?7, consecutive_successes = ?8, total_successes = ?9,
             consecutive_failures = ?10, total_failures = ?11, suspended = ?12,
             gnosis_plus = ?13, gnosis_minus = ?14, gnosis_synolon = ?15
         WHERE thema_id = ?1 AND state_revision = ?16",
            params![
                row.thema_id,
                after.state_revision,
                after.review_count,
                after.last_reviewed_at_us,
                after.last_review_day,
                after.next_review_day,
                after.interval_days,
                after.consecutive_successes,
                after.total_successes,
                after.consecutive_failures,
                after.total_failures,
                i64::from(after.suspended),
                after.gnosis_plus,
                after.gnosis_minus,
                after.gnosis_synolon,
                row.state_revision,
            ],
        )
        .map_err(StoreError::storage)?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::StaleAttempt)
    }
}

fn insert_review_event(
    transaction: &Transaction<'_>,
    row: &AttemptRow,
    transition: &TransitionSnapshot,
    event_id: &str,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO review_event(
             event_id, attempt_id, thema_id, session_id, reviewed_at_us, review_day,
             matcher_version, response_json, detected_success, accepted_success,
             override_count, scheduler_version, transition_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
            params![
                event_id,
                row.attempt_id,
                row.thema_id,
                row.session_id,
                transition.reviewed_at_us,
                transition.review_day,
                row.matcher_version,
                serde_json::to_string(&row.response).map_err(StoreError::storage)?,
                i64::from(row.detected_success.ok_or(StoreError::InvalidReviewState)?),
                i64::from(row.accepted_success.ok_or(StoreError::InvalidReviewState)?),
                row.override_count,
                transition.parameters.scheduler_version,
                serde_json::to_string(transition).map_err(StoreError::storage)?,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn insert_review_state_history(
    transaction: &Transaction<'_>,
    thema_id: &str,
    state: &ReviewStateSnapshot,
    event_id: &str,
    created_at_us: u64,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO review_state_revision(
                 thema_id, state_revision, state_json, event_id, created_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                thema_id,
                u64_to_i64(state.state_revision, "state revision")?,
                serde_json::to_string(state).map_err(StoreError::storage)?,
                event_id,
                u64_to_i64(created_at_us, "review state timestamp")?,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn finish_attempt(
    transaction: &Transaction<'_>,
    row: &AttemptRow,
    _event_id: &str,
    committed_at_us: u64,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE review_attempt SET status = 'committed', committed_at_us = ?2 WHERE attempt_id = ?1",
        params![row.attempt_id, u64_to_i64(committed_at_us, "commit timestamp")?],
    ).map_err(StoreError::storage)?;
    transaction
        .execute(
            "UPDATE review_session_item SET completed_attempt_id = ?3
         WHERE session_id = ?1 AND position = (
             SELECT session_position FROM review_attempt WHERE attempt_id = ?2
         )",
            params![row.session_id, row.attempt_id, row.attempt_id],
        )
        .map_err(StoreError::storage)?;
    transaction
        .execute(
            "UPDATE review_session SET
             current_position = current_position + 1,
             status = CASE WHEN current_position + 1 >= (
                 SELECT count(*) FROM review_session_item WHERE session_id = ?1
             ) THEN 'completed' ELSE status END,
             ended_at_us = CASE WHEN current_position + 1 >= (
                 SELECT count(*) FROM review_session_item WHERE session_id = ?1
             ) THEN ?2 ELSE ended_at_us END
         WHERE session_id = ?1",
            params![
                row.session_id,
                u64_to_i64(committed_at_us, "commit timestamp")?
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

fn update_review_day(
    transaction: &Transaction<'_>,
    transition: &TransitionSnapshot,
    override_count: u32,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO review_day(
             review_day, review_count, new_count, success_count, failure_count, override_count
         ) VALUES (?1, 1, ?2, ?3, ?4, ?5)
         ON CONFLICT(review_day) DO UPDATE SET
             review_count = review_count + 1, new_count = new_count + excluded.new_count,
             success_count = success_count + excluded.success_count,
             failure_count = failure_count + excluded.failure_count,
             override_count = override_count + excluded.override_count",
            params![
                transition.review_day,
                i64::from(transition.first_review),
                i64::from(transition.success),
                i64::from(!transition.success),
                override_count,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use serde_json::json;
    use tempfile::tempdir;

    use super::{MAX_REVIEW_PACK_ITEMS, SqliteStore, load_offline_result, load_review_pack};
    use crate::application::{
        AnswerInput, OfflineConflictReason, OfflineReviewOperation, ReconciliationStatus,
        ReviewSource, ReviewSourceKind, generate_device, prepare_thema, presentation_content,
        review_day_at, thema_content,
    };
    use crate::store::sqlite::{
        INITIAL_MIGRATION, OFFLINE_REVIEW_MIGRATION, REMOTE_REVIEW_MIGRATION, SERVER_MIGRATION,
        migrate,
    };

    fn add_thema_with_tags(
        store: &mut SqliteStore,
        id: &str,
        question: &str,
        tags: Vec<String>,
        revision: Option<u64>,
    ) {
        let content = thema_content(
            "basic",
            question.into(),
            vec![None],
            vec!["answer".into()],
            None,
            None,
            None,
        )
        .unwrap();
        let prepared =
            prepare_thema(Some(id.into()), content, tags, vec![], vec![], revision).unwrap();
        store.put_thema(&prepared, 20260715, 1, None).unwrap();
    }

    #[test]
    fn v4_cursor_zero_review_records_upgrade_to_expired_durable_cursors() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .unwrap();
        connection.execute_batch(INITIAL_MIGRATION).unwrap();
        connection.execute_batch(SERVER_MIGRATION).unwrap();
        connection.execute_batch(REMOTE_REVIEW_MIGRATION).unwrap();
        connection.execute_batch(OFFLINE_REVIEW_MIGRATION).unwrap();
        connection
            .execute(
                "INSERT INTO device(
                     device_id, name, token_digest, scopes_json, created_at_us
                 ) VALUES ('device', 'Device', ?1, '[\"review\"]', 1)",
                ["a".repeat(64)],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO review_session(
                     session_id, source_kind, source_json, target_count, timezone,
                     day_boundary_hour, scheduler_json, status, started_at_us,
                     owner_device_id, review_mode
                 ) VALUES (
                     'session', 'manual', ?1, 1, 'UTC', 0, '{}', 'completed', 1,
                     'device', 'offline'
                 )",
                [json!({
                    "kind":"manual", "tags":[], "exclude_tags":[],
                    "topic_node_id":null, "topic_forward_depth":null,
                    "topic_backward_depth":null, "manual_ids":["thema"]
                })
                .to_string()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO review_pack(
                     pack_id, format_version, session_id, device_id, lease_id,
                     status, cursor, issued_at_us
                 ) VALUES ('pack', 1, 'session', 'device', 'lease', 'completed', 0, 1)",
                [],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO offline_review_operation(
                     attempt_id, pack_id, device_id, device_sequence, format_version,
                     payload_sha256, payload_json, received_at_us
                 ) VALUES ('attempt', 'pack', 'device', 1, 1, ?1, '{}', 1)",
                ["b".repeat(64)],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO offline_review_result(
                     attempt_id, format_version, status, conflict_reason, event_id,
                     authoritative_state_json, session_json, cursor, reconciled_at_us
                 ) VALUES ('attempt', 1, 'conflict', 'lease_ended', NULL, ?1, ?2, 0, 1)",
                [review_state_json(), review_session_json()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO content_change(
                     entity_kind, entity_id, revision, operation, created_at_us
                 ) VALUES ('settings', 'canonical', 1, 'update', 2)",
                [],
            )
            .unwrap();

        migrate(&connection).unwrap();

        let pack = load_review_pack(&connection, "pack").unwrap().unwrap();
        let result = load_offline_result(&connection, "attempt")
            .unwrap()
            .unwrap();
        assert_eq!(pack.cursor, 0);
        assert_eq!(result.cursor, 0);
        assert_eq!(pack.cursor_server_id, result.cursor_server_id);
        assert_eq!(pack.cursor_token, result.cursor_token);
        assert_eq!(pack.cursor_token.len(), 32);
        assert_eq!(
            connection
                .query_row(
                    "SELECT last_device_sequence FROM device WHERE device_id = 'device'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
        assert!(
            connection
                .execute(
                    "UPDATE review_pack SET cursor_token = NULL WHERE pack_id = 'pack'",
                    [],
                )
                .is_err()
        );
    }

    fn review_state_json() -> String {
        json!({
            "state_revision":0, "review_count":0, "legacy_review_count":0,
            "legacy_unclassified_count":0, "last_reviewed_at_us":null,
            "last_review_day":null, "next_review_day":20260715,
            "interval_days":null, "consecutive_successes":0, "total_successes":0,
            "consecutive_failures":0, "total_failures":0, "suspended":false,
            "gnosis_plus":0.35, "gnosis_minus":0.3, "gnosis_synolon":1.3
        })
        .to_string()
    }

    fn review_session_json() -> String {
        json!({
            "session_id":"session", "owner_device_id":"device", "mode":"offline",
            "source":{"kind":"manual", "tags":[], "exclude_tags":[],
                "topic_node_id":null, "topic_forward_depth":null,
                "topic_backward_depth":null, "manual_ids":["thema"]},
            "target_count":1, "status":"completed", "current_position":0,
            "queue_length":0, "started_at_us":1
        })
        .to_string()
    }

    fn add_thema(store: &mut SqliteStore, id: &str, question: &str, revision: Option<u64>) {
        add_thema_with_tags(store, id, question, vec![], revision);
    }

    fn manual_source(id: &str) -> ReviewSource {
        ReviewSource {
            kind: ReviewSourceKind::Manual,
            tags: vec![],
            exclude_tags: vec![],
            topic_node_id: None,
            topic_forward_depth: None,
            topic_backward_depth: None,
            manual_ids: vec![id.into()],
        }
    }

    fn add_device(store: &mut SqliteStore, name: &str) -> String {
        let device = generate_device(name.into(), vec!["review".into()]).unwrap();
        let id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        id
    }

    fn offline_answer(
        pack: &crate::application::ReviewPackView,
        attempt_id: &str,
        sequence: u64,
    ) -> OfflineReviewOperation {
        let item = &pack.items[0];
        let response = AnswerInput::Text("answer".into());
        let detected = crate::domain::grade(
            item.matcher_version,
            &presentation_content(&item.presentation).unwrap(),
            &response.clone().into(),
        )
        .unwrap()
        .success;
        let reviewed_at_us = pack.issued_at_us + 1_000;
        let review_day =
            review_day_at(reviewed_at_us, &pack.timezone, pack.day_boundary_hour).unwrap();
        let transition = super::transition_with_sample(
            &item.review_state.clone().into(),
            &item.parameters.clone().into(),
            detected,
            review_day,
            reviewed_at_us,
            0.0,
        )
        .unwrap();
        OfflineReviewOperation {
            attempt_id: attempt_id.into(),
            pack_id: pack.pack_id.clone(),
            session_id: pack.session_id.clone(),
            lease_id: pack.lease_id.clone(),
            device_id: pack.device_id.clone(),
            device_sequence: sequence,
            session_position: item.position,
            thema_id: item.thema_id.clone(),
            thema_revision: item.thema_revision,
            base_state_revision: item.review_state.state_revision,
            content_sha256: item.content_sha256.clone(),
            presentation: item.presentation.clone(),
            parameters: item.parameters.clone(),
            matcher_version: item.matcher_version,
            response,
            detected_success: detected,
            accepted_success: detected,
            override_count: 0,
            proposed_transition: (&transition).into(),
            desired_suspended: false,
            answered_at_us: reviewed_at_us,
            fuzz_sample: 0.0,
        }
    }

    fn reconcile(
        store: &mut SqliteStore,
        operation: &OfflineReviewOperation,
        device_id: &str,
        received_at_us: u64,
    ) -> Result<crate::application::OfflineReviewResult, crate::store::StoreError> {
        let payload = crate::protocol::canonical_offline_operation(1, operation).unwrap();
        store.reconcile_offline_operation(1, operation, &payload, Some(device_id), received_at_us)
    }

    #[test]
    fn offline_pack_commits_atomically_and_replays_the_stored_result() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let device_id = add_device(&mut store, "offline");
        let pack = store
            .create_review_pack(
                1,
                &manual_source("thema"),
                Some(1),
                &device_id,
                1_752_600_000_000_000,
            )
            .unwrap();
        let operation = offline_answer(&pack, "attempt", 1);

        let result = reconcile(&mut store, &operation, &device_id, 1_752_600_000_002_000).unwrap();

        assert_eq!(result.status, ReconciliationStatus::Accepted);
        assert_eq!(result.authoritative_state.review_count, 1);
        assert_eq!(result.session.status, "completed");
        assert_eq!(result.cursor, 3);
        assert_eq!(
            store
                .replica_changes(
                    &result.cursor_server_id,
                    result.cursor,
                    &result.cursor_token,
                    10,
                )
                .unwrap()
                .0,
            vec![]
        );
        assert_eq!(
            reconcile(&mut store, &operation, &device_id, 1_752_600_000_003_000,).unwrap(),
            result
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM review_event", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT count(*) FROM review_state_revision WHERE thema_id = 'thema'",
                    [],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            2
        );
    }

    #[test]
    fn stale_pack_attempt_is_preserved_without_changing_canonical_state() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "First", None);
        let device_id = add_device(&mut store, "offline");
        let pack = store
            .create_review_pack(
                1,
                &manual_source("thema"),
                None,
                &device_id,
                1_752_600_000_000_000,
            )
            .unwrap();
        add_thema(&mut store, "thema", "Changed", Some(1));
        let operation = offline_answer(&pack, "stale-attempt", 1);

        let result = reconcile(&mut store, &operation, &device_id, 1_752_600_000_002_000).unwrap();

        assert_eq!(result.status, ReconciliationStatus::Conflict);
        assert_eq!(
            result.conflict_reason,
            Some(OfflineConflictReason::ContentChanged)
        );
        assert_eq!(result.authoritative_state.review_count, 0);
        assert_eq!(result.session.current_position, 0);
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM offline_review_operation", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM review_event", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn stale_scheduling_revision_is_preserved_without_an_event() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let device_id = add_device(&mut store, "offline");
        let pack = store
            .create_review_pack(
                1,
                &manual_source("thema"),
                Some(1),
                &device_id,
                1_752_600_000_000_000,
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE review_state SET state_revision = state_revision + 1
                 WHERE thema_id = 'thema'",
                [],
            )
            .unwrap();
        let operation = offline_answer(&pack, "stale-state", 1);

        let result = reconcile(&mut store, &operation, &device_id, 1_752_600_000_002_000).unwrap();

        assert_eq!(result.status, ReconciliationStatus::Conflict);
        assert_eq!(
            result.conflict_reason,
            Some(OfflineConflictReason::SchedulingChanged)
        );
        assert_eq!(result.authoritative_state.state_revision, 1);
        let events: i64 = store
            .connection
            .query_row("SELECT count(*) FROM review_event", [], |row| row.get(0))
            .unwrap();
        assert_eq!(events, 0);
    }

    #[test]
    fn device_sequence_gap_does_not_consume_the_missing_sequence() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let device_id = add_device(&mut store, "offline");
        let pack = store
            .create_review_pack(
                1,
                &manual_source("thema"),
                Some(1),
                &device_id,
                1_752_600_000_000_000,
            )
            .unwrap();

        assert!(matches!(
            reconcile(
                &mut store,
                &offline_answer(&pack, "gap", 2),
                &device_id,
                1_752_600_000_002_000,
            ),
            Err(crate::store::StoreError::DeviceSequence { expected: 1 })
        ));
        let accepted = reconcile(
            &mut store,
            &offline_answer(&pack, "first", 1),
            &device_id,
            1_752_600_000_003_000,
        )
        .unwrap();
        assert_eq!(accepted.status, ReconciliationStatus::Accepted);
    }

    #[test]
    fn device_sequence_continues_across_review_packs() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let device_id = add_device(&mut store, "offline");
        let first = store
            .create_review_pack(
                1,
                &manual_source("thema"),
                Some(1),
                &device_id,
                1_752_600_000_000_000,
            )
            .unwrap();
        reconcile(
            &mut store,
            &offline_answer(&first, "first-attempt", 1),
            &device_id,
            1_752_600_000_002_000,
        )
        .unwrap();
        let second = store
            .create_review_pack(
                1,
                &manual_source("thema"),
                Some(1),
                &device_id,
                1_752_600_000_003_000,
            )
            .unwrap();

        let result = reconcile(
            &mut store,
            &offline_answer(&second, "second-attempt", 2),
            &device_id,
            1_752_600_000_004_000,
        )
        .unwrap();

        assert_eq!(result.status, ReconciliationStatus::Accepted);
        assert_eq!(result.authoritative_state.review_count, 2);
    }

    #[test]
    fn pack_creation_retry_recovers_the_same_lease_after_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gnosis.sqlite");
        let (pack, device_id) = {
            let mut store = SqliteStore::open(&path).unwrap();
            add_thema(&mut store, "thema", "Question", None);
            let device_id = add_device(&mut store, "offline");
            let pack = store
                .create_review_pack(
                    1,
                    &manual_source("thema"),
                    Some(1),
                    &device_id,
                    1_752_600_000_000_000,
                )
                .unwrap();
            (pack, device_id)
        };
        let mut reopened = SqliteStore::open(&path).unwrap();

        let recovered = reopened
            .create_review_pack(
                1,
                &manual_source("thema"),
                Some(1),
                &device_id,
                1_752_600_000_001_000,
            )
            .unwrap();

        assert_eq!(recovered, pack);
        assert_eq!(
            reopened.active_review_pack(Some(&device_id)).unwrap(),
            Some(pack)
        );
    }

    #[test]
    fn accepted_operation_replays_exactly_after_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("gnosis.sqlite");
        let (operation, device_id, accepted) = {
            let mut store = SqliteStore::open(&path).unwrap();
            add_thema(&mut store, "thema", "Question", None);
            let device_id = add_device(&mut store, "offline");
            let pack = store
                .create_review_pack(
                    1,
                    &manual_source("thema"),
                    Some(1),
                    &device_id,
                    1_752_600_000_000_000,
                )
                .unwrap();
            let operation = offline_answer(&pack, "attempt", 1);
            let accepted =
                reconcile(&mut store, &operation, &device_id, 1_752_600_000_002_000).unwrap();
            (operation, device_id, accepted)
        };
        let mut reopened = SqliteStore::open(&path).unwrap();

        let replay =
            reconcile(&mut reopened, &operation, &device_id, 1_752_600_000_003_000).unwrap();

        assert_eq!(replay, accepted);
        let events: i64 = reopened
            .connection
            .query_row("SELECT count(*) FROM review_event", [], |row| row.get(0))
            .unwrap();
        assert_eq!(events, 1);
    }

    #[test]
    fn forged_review_day_is_preserved_as_a_capture_conflict() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let device_id = add_device(&mut store, "offline");
        let pack = store
            .create_review_pack(
                1,
                &manual_source("thema"),
                Some(1),
                &device_id,
                1_752_600_000_000_000,
            )
            .unwrap();
        let mut operation = offline_answer(&pack, "forged-day", 1);
        let forged_day = operation.proposed_transition.review_day + 1;
        let transition = super::transition_with_sample(
            &pack.items[0].review_state.clone().into(),
            &pack.items[0].parameters.clone().into(),
            operation.accepted_success,
            forged_day,
            operation.answered_at_us,
            operation.fuzz_sample,
        )
        .unwrap();
        operation.proposed_transition = (&transition).into();

        let result = reconcile(&mut store, &operation, &device_id, 1_752_600_000_002_000).unwrap();

        assert_eq!(result.status, ReconciliationStatus::Conflict);
        assert_eq!(
            result.conflict_reason,
            Some(OfflineConflictReason::CaptureMismatch)
        );
        assert_eq!(result.authoritative_state.review_count, 0);
    }

    #[test]
    fn active_offline_lease_blocks_a_competing_review_session() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let device_id = add_device(&mut store, "offline");
        store
            .create_review_pack(
                1,
                &manual_source("thema"),
                None,
                &device_id,
                1_752_600_000_000_000,
            )
            .unwrap();

        assert!(matches!(
            store
                .create_review_session(&manual_source("thema"), None, None, 1_752_600_000_000_001,),
            Err(crate::store::StoreError::ActiveReviewSession)
        ));
    }

    #[test]
    fn review_pack_rejects_unbounded_manual_sources() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let device_id = add_device(&mut store, "offline");
        let source = ReviewSource {
            manual_ids: (0..=MAX_REVIEW_PACK_ITEMS)
                .map(|index| format!("thema-{index}"))
                .collect(),
            ..manual_source("unused")
        };

        assert!(matches!(
            store.create_review_pack(
                1,
                &source,
                Some(MAX_REVIEW_PACK_ITEMS as u32),
                &device_id,
                1_752_600_000_000_000,
            ),
            Err(crate::store::StoreError::InvalidReviewState)
        ));
    }

    #[test]
    fn review_pack_rejects_external_assets_without_starting_a_session() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let content = thema_content(
            "basic",
            "Question".into(),
            vec![None],
            vec!["answer".into()],
            None,
            None,
            Some("https://example.test/image.png".into()),
        )
        .unwrap();
        let prepared =
            prepare_thema(Some("thema".into()), content, vec![], vec![], vec![], None).unwrap();
        store.put_thema(&prepared, 20260715, 1, None).unwrap();
        let device_id = add_device(&mut store, "offline");

        assert!(matches!(
            store.create_review_pack(
                1,
                &manual_source("thema"),
                Some(1),
                &device_id,
                1_752_600_000_000_000,
            ),
            Err(crate::store::StoreError::InvalidReviewState)
        ));
        assert_eq!(store.active_review_pack(Some(&device_id)).unwrap(), None);
        assert_eq!(store.active_review_session(Some(&device_id)).unwrap(), None);
    }

    #[test]
    fn ownerless_online_sessions_require_local_or_admin_recovery() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let device_id = add_device(&mut store, "remote");
        let session = store
            .create_review_session(
                &manual_source("thema"),
                Some(1),
                None,
                1_752_600_000_000_000,
            )
            .unwrap();

        assert_eq!(store.active_review_session(Some(&device_id)).unwrap(), None);
        assert!(matches!(
            store.begin_review_attempt(
                &session.session_id,
                Some(&device_id),
                1_752_600_000_001_000,
            ),
            Err(crate::store::StoreError::ReviewOwnership)
        ));
        assert!(
            store
                .begin_review_attempt(&session.session_id, None, 1_752_600_000_001_000)
                .is_ok()
        );
    }

    #[test]
    fn offline_and_online_paths_produce_the_same_transition() {
        let mut offline = SqliteStore::open_in_memory().unwrap();
        let mut online = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut offline, "thema", "Question", None);
        add_thema(&mut online, "thema", "Question", None);
        let device_id = add_device(&mut offline, "offline");
        let pack = offline
            .create_review_pack(
                1,
                &manual_source("thema"),
                None,
                &device_id,
                1_752_600_000_000_000,
            )
            .unwrap();
        let operation = offline_answer(&pack, "offline-attempt", 1);
        reconcile(&mut offline, &operation, &device_id, 1_752_600_000_002_000).unwrap();

        let session = online
            .create_review_session(&manual_source("thema"), None, None, 1_752_600_000_000_000)
            .unwrap();
        let attempt = online
            .begin_review_attempt(&session.session_id, None, 1_752_600_000_000_001)
            .unwrap();
        online
            .grade_review_attempt(
                &attempt.attempt_id,
                None,
                &AnswerInput::Text("answer".into()),
                1_752_600_000_001_000,
                0.0,
            )
            .unwrap();
        online
            .commit_review_attempt(&attempt.attempt_id, None, 1_752_600_000_002_000)
            .unwrap();

        assert_eq!(
            super::load_review_state(&offline.connection, "thema").unwrap(),
            super::load_review_state(&online.connection, "thema").unwrap()
        );
        let transition = |store: &SqliteStore| {
            store
                .connection
                .query_row("SELECT transition_json FROM review_event", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap()
        };
        assert_eq!(transition(&offline), transition(&online));
        assert_eq!(
            super::latest_content_cursor(&offline.connection).unwrap(),
            super::latest_content_cursor(&online.connection).unwrap()
        );
    }

    #[test]
    fn review_attempt_commits_atomically_and_replays_by_attempt_id() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let session = store
            .create_review_session(
                &manual_source("thema"),
                Some(1),
                None,
                1_752_600_000_000_000,
            )
            .unwrap();
        let attempt = store
            .begin_review_attempt(&session.session_id, None, 1_752_600_000_000_001)
            .unwrap();
        let graded = store
            .grade_review_attempt(
                &attempt.attempt_id,
                None,
                &AnswerInput::Text("ANSWER".into()),
                1_752_600_000_001_000,
                0.0,
            )
            .unwrap();
        assert_eq!(graded.detected_success, Some(true));
        let event = store
            .commit_review_attempt(&attempt.attempt_id, None, 1_752_600_000_002_000)
            .unwrap();
        assert_eq!(
            store
                .commit_review_attempt(&attempt.attempt_id, None, 1_752_600_000_003_000)
                .unwrap(),
            event
        );
        assert_eq!(
            store
                .review_session(&session.session_id)
                .unwrap()
                .unwrap()
                .status,
            "completed"
        );
        assert_eq!(
            super::load_review_state(&store.connection, "thema")
                .unwrap()
                .review_count,
            1
        );
    }

    #[test]
    fn aborted_session_rejects_an_uncommitted_attempt() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let session = store
            .create_review_session(
                &manual_source("thema"),
                Some(1),
                None,
                1_752_600_000_000_000,
            )
            .unwrap();
        let attempt = store
            .begin_review_attempt(&session.session_id, None, 1_752_600_000_000_001)
            .unwrap();
        store
            .grade_review_attempt(
                &attempt.attempt_id,
                None,
                &AnswerInput::Text("answer".into()),
                1_752_600_000_001_000,
                0.0,
            )
            .unwrap();
        store
            .stop_review_session(&session.session_id, None, true, 1_752_600_000_002_000)
            .unwrap();

        assert!(matches!(
            store.commit_review_attempt(&attempt.attempt_id, None, 1_752_600_000_003_000),
            Err(crate::store::StoreError::InvalidReviewState)
        ));
    }

    #[test]
    fn changed_content_rejects_a_pending_attempt() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "First", None);
        let session = store
            .create_review_session(&manual_source("thema"), None, None, 1_752_600_000_000_000)
            .unwrap();
        let attempt = store
            .begin_review_attempt(&session.session_id, None, 1_752_600_000_000_001)
            .unwrap();
        add_thema(&mut store, "thema", "Changed", Some(1));

        let error = store
            .grade_review_attempt(
                &attempt.attempt_id,
                None,
                &AnswerInput::Text("answer".into()),
                1_752_600_000_001_000,
                0.0,
            )
            .unwrap_err();
        assert!(matches!(error, crate::store::StoreError::StaleAttempt));
    }

    #[test]
    fn tag_rules_replace_globals_and_aggregate_like_elisp() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        for (tag, parameters) in [
            (
                "tag-a",
                r#"{"agnoia":0.4,"amnesia":0.45,"anagnosis":3,"epignosis":0.5,"lethe":2,"proto":[1,3]}"#,
            ),
            ("tag-c", r#"{"amnesia":0.45,"proto":[1,3]}"#),
        ] {
            store
                .connection
                .execute(
                    "INSERT INTO tag_rule(tag, parameters_json) VALUES (?1, ?2)",
                    params![tag, parameters],
                )
                .unwrap();
        }
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../tests/fixtures/review/parity-v1.json"
        ))
        .unwrap();
        let tags = fixture["tag_rules"]["tags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tag| tag.as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        add_thema_with_tags(&mut store, "tagged", "Question", tags.clone(), None);
        let session = store
            .create_review_session(&manual_source("tagged"), None, None, 1_752_600_000_000_000)
            .unwrap();
        let parameters =
            super::resolved_parameters(&store.connection, &session.session_id, &tags).unwrap();
        let expected = &fixture["tag_rules"];
        assert_eq!(parameters.amnesia, expected["amnesia"].as_f64().unwrap());
        assert_eq!(
            parameters.epignosis,
            expected["epignosis"].as_f64().unwrap()
        );
        assert_eq!(parameters.agnoia, expected["agnoia"].as_f64().unwrap());
        assert_eq!(
            parameters.anagnosis,
            expected["anagnosis"].as_u64().unwrap()
        );
        assert_eq!(parameters.lethe, expected["lethe"].as_u64().unwrap());
        assert_eq!(
            parameters.proto,
            expected["proto"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| u32::try_from(value.as_u64().unwrap()).unwrap())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn tag_sources_apply_include_union_and_exclusions() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema_with_tags(&mut store, "keep", "Keep", vec!["include".into()], None);
        add_thema_with_tags(
            &mut store,
            "drop",
            "Drop",
            vec!["include".into(), "exclude".into()],
            None,
        );
        add_thema_with_tags(&mut store, "other", "Other", vec!["other".into()], None);
        let source = ReviewSource {
            kind: ReviewSourceKind::Tag,
            tags: vec!["include".into()],
            exclude_tags: vec!["exclude".into()],
            topic_node_id: None,
            topic_forward_depth: None,
            topic_backward_depth: None,
            manual_ids: vec![],
        };
        assert_eq!(
            super::select_queue(&store.connection, &source, 20260715).unwrap(),
            ["keep"]
        );
    }

    #[test]
    fn matcher_dispatch_reads_the_captured_version() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let session = store
            .create_review_session(&manual_source("thema"), None, None, 1_752_600_000_000_000)
            .unwrap();
        let attempt = store
            .begin_review_attempt(&session.session_id, None, 1_752_600_000_000_001)
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE review_attempt SET matcher_version = 2 WHERE attempt_id = ?1",
                [&attempt.attempt_id],
            )
            .unwrap();
        assert!(
            store
                .grade_review_attempt(
                    &attempt.attempt_id,
                    None,
                    &AnswerInput::Text("answer".into()),
                    1_752_600_000_001_000,
                    0.0,
                )
                .is_err()
        );
    }

    #[test]
    fn suspension_is_recorded_in_the_atomic_transition() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let session = store
            .create_review_session(&manual_source("thema"), None, None, 1_752_600_000_000_000)
            .unwrap();
        let attempt = store
            .begin_review_attempt(&session.session_id, None, 1_752_600_000_000_001)
            .unwrap();
        store
            .grade_review_attempt(
                &attempt.attempt_id,
                None,
                &AnswerInput::Text("answer".into()),
                1_752_600_000_001_000,
                0.0,
            )
            .unwrap();
        store
            .toggle_attempt_suspension(&attempt.attempt_id, None)
            .unwrap();
        let event = store
            .commit_review_attempt(&attempt.attempt_id, None, 1_752_600_000_002_000)
            .unwrap();
        let transition: String = store
            .connection
            .query_row(
                "SELECT transition_json FROM review_event WHERE event_id = ?1",
                [event],
                |row| row.get(0),
            )
            .unwrap();
        let transition: crate::application::TransitionSnapshot =
            serde_json::from_str(&transition).unwrap();
        assert!(transition.after.suspended);
        assert!(
            super::load_review_state(&store.connection, "thema")
                .unwrap()
                .suspended
        );
    }
}
