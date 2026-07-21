use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

use super::{
    DeviceOperationInsert, SqliteStore, StoreError, advance_device_sequence,
    insert_device_operation, load_thema, replayed_device_operation, require_device_sequence,
    scoped_operation_id, to_u32, to_u64, u64_to_i64,
};
use crate::{
    application::{
        AnswerInput, DEFAULT_ONLINE_REVIEW_TARGET_COUNT, FsrsConfigSnapshot,
        MAX_OFFLINE_ASSET_BYTES, MAX_ONLINE_REVIEW_TARGET_COUNT, MAX_REVIEW_PACK_BYTES,
        MAX_REVIEW_PACK_ITEMS, OfflineConflictReason, OfflineReviewOperation, OfflineReviewResult,
        PresentationSnapshot, RecallOutcomeSnapshot, ReconciliationStatus, ReviewAttemptMutation,
        ReviewAttemptOperation, ReviewAttemptOperationResult, ReviewAttemptOperationStatus,
        ReviewAttemptState, ReviewAttemptView, ReviewOperationAuthority, ReviewPackAsset,
        ReviewPackItem, ReviewPackView, ReviewSessionMode, ReviewSessionView, ReviewSource,
        ReviewSourceKind, ReviewStateSnapshot, TransitionSnapshot, presentation_content,
        review_day_at, review_question, validate_offline_thema_assets,
        validate_review_pack_projection,
    },
    domain::{
        ClozeCheck, GradeError, MATCHER_VERSION, ThemaKind, check_clozes, compute_transition, grade,
    },
};

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct StoredAttemptOperationPayload<'a> {
    format_version: u32,
    operation_id: &'a str,
    attempt_id: &'a str,
    expected_attempt_revision: u64,
    mutation: ReviewAttemptMutation,
}

const ATTEMPT_OPERATION_REPLAY_SQL: &str =
    "SELECT payload_sha256, result_json FROM review_attempt_operation
     WHERE operation_id = ?1 AND actor_device_id = ?2";
const MAX_TOPIC_TRAVERSAL_ROWS: i64 = 1_024;

impl SqliteStore {
    pub(crate) fn create_review_session(
        &mut self,
        source: &ReviewSource,
        target_count: Option<u32>,
        owner_device_id: Option<&str>,
        started_at_us: u64,
    ) -> Result<ReviewSessionView, StoreError> {
        source
            .clone()
            .validate()
            .map_err(|_| StoreError::InvalidReviewState)?;
        let target_count = Some(target_count.unwrap_or(DEFAULT_ONLINE_REVIEW_TARGET_COUNT));
        if !target_count.is_some_and(|count| (1..=MAX_ONLINE_REVIEW_TARGET_COUNT).contains(&count))
        {
            return Err(StoreError::InvalidReviewState);
        }
        require_no_active_session(&self.connection)?;
        let captured = captured_settings(&self.connection)?;
        let review_day = review_day_at(
            started_at_us,
            &captured.timezone,
            captured.day_boundary_hour,
        )
        .map_err(StoreError::storage)?;
        let queue = select_queue(&self.connection, source, review_day, target_count)?;
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
        source
            .clone()
            .validate()
            .map_err(|_| StoreError::InvalidReviewState)?;
        if format_version != 1 {
            return Err(StoreError::InvalidReviewState);
        }
        let target_count = Some(target_count.unwrap_or(100));
        if !target_count.is_some_and(|count| (1..=MAX_REVIEW_PACK_ITEMS as u32).contains(&count)) {
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
        validate_review_pack_projection(&pack).map_err(|_| StoreError::InvalidReviewState)?;
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
                   AND review_mode = 'online'
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

    #[cfg(test)]
    pub(crate) fn begin_review_attempt(
        &mut self,
        session_id: &str,
        actor_device_id: Option<&str>,
        started_at_us: u64,
    ) -> Result<ReviewAttemptView, StoreError> {
        self.begin_review_attempt_with_contract(session_id, actor_device_id, started_at_us, false)
    }

    pub(crate) fn begin_http_review_attempt(
        &mut self,
        session_id: &str,
        actor_device_id: Option<&str>,
        started_at_us: u64,
    ) -> Result<ReviewAttemptView, StoreError> {
        self.begin_review_attempt_with_contract(session_id, actor_device_id, started_at_us, true)
    }

    fn begin_review_attempt_with_contract(
        &mut self,
        session_id: &str,
        actor_device_id: Option<&str>,
        started_at_us: u64,
        require_safe_question: bool,
    ) -> Result<ReviewAttemptView, StoreError> {
        require_session_actor(
            &self.connection,
            session_id,
            actor_device_id,
            ReviewSessionMode::Online,
        )?;
        if let Some(attempt) = current_attempt(&self.connection, session_id)? {
            require_safe_review_question(&attempt.presentation, require_safe_question)?;
            return Ok(attempt);
        }
        let (position, thema_id) = current_session_item(&self.connection, session_id)?;
        let thema = load_thema(&self.connection, &thema_id)?.ok_or(StoreError::NotFound)?;
        if thema.archived {
            return Err(StoreError::InvalidReviewState);
        }
        let state = load_review_state(&self.connection, &thema_id)?;
        let presentation = presentation(&thema);
        require_safe_review_question(&presentation, require_safe_question)?;
        let config = resolved_config(&self.connection, session_id)?;
        let attempt_id = Uuid::now_v7().to_string();
        self.connection
            .execute(
                "INSERT INTO review_attempt(
                 attempt_id, session_id, session_position, thema_id, thema_revision, state_revision,
                 content_sha256, presentation_json, config_json, matcher_version,
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
                    serde_json::to_string(&FsrsConfigSnapshot::from(&config))
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
        let config = row.config.clone().into();
        let (timezone, boundary) = session_clock(&self.connection, &row.session_id)?;
        let review_day =
            review_day_at(answered_at_us, &timezone, boundary).map_err(StoreError::storage)?;
        let transition =
            compute_review_transition(&state, &config, result.success, review_day, answered_at_us)?;
        self.connection
            .execute(
                "UPDATE review_attempt SET response_json = ?2, detected_success = ?3,
                 accepted_success = ?3, proposed_transition_json = ?4,
                 status = 'graded', answered_at_us = ?5,
                 mutation_revision = mutation_revision + 1 WHERE attempt_id = ?1",
                params![
                    attempt_id,
                    serde_json::to_string(response).map_err(StoreError::storage)?,
                    i64::from(result.success),
                    serde_json::to_string(&TransitionSnapshot::from(&transition))
                        .map_err(StoreError::storage)?,
                    u64_to_i64(answered_at_us, "answer timestamp")?,
                ],
            )
            .map_err(StoreError::storage)?;
        load_attempt(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)
    }

    pub(crate) fn check_cloze_attempt(
        &self,
        attempt_id: &str,
        actor_device_id: Option<&str>,
        inputs: &[String],
    ) -> Result<ClozeCheck, StoreError> {
        let row = load_attempt_row(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)?;
        require_session_actor(
            &self.connection,
            &row.session_id,
            actor_device_id,
            ReviewSessionMode::Online,
        )?;
        if row.status != "pending" {
            return Err(StoreError::InvalidReviewState);
        }
        verify_live_attempt(&self.connection, &row)?;
        let content = presentation_content(&row.presentation).map_err(StoreError::storage)?;
        if content.kind != ThemaKind::Cloze
            || inputs.is_empty()
            || inputs.len() > 500
            || inputs.len() > content.answer.len()
        {
            return Err(StoreError::InvalidReviewResponse);
        }
        Ok(check_clozes(&content.answer, inputs))
    }

    pub(crate) fn override_review_attempt(
        &mut self,
        attempt_id: &str,
        actor_device_id: Option<&str>,
    ) -> Result<ReviewAttemptView, StoreError> {
        override_review_attempt_in(&self.connection, attempt_id, actor_device_id)
    }

    pub(crate) fn toggle_attempt_suspension(
        &mut self,
        attempt_id: &str,
        actor_device_id: Option<&str>,
    ) -> Result<ReviewAttemptView, StoreError> {
        toggle_attempt_suspension_in(&self.connection, attempt_id, actor_device_id)
    }

    pub(crate) fn apply_review_attempt_operation(
        &mut self,
        operation: &ReviewAttemptOperation,
        authority: &ReviewOperationAuthority,
        operated_at_us: u64,
    ) -> Result<ReviewAttemptOperationResult, StoreError> {
        let payload = canonical_attempt_operation(operation)?;
        let digest = format!("{:x}", Sha256::digest(payload.as_bytes()));
        let transaction = self.write_transaction()?;
        let result = reconcile_attempt_operation(
            &transaction,
            operation,
            authority,
            &payload,
            &digest,
            operated_at_us,
        )?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(result)
    }

    pub(crate) fn review_attempt_state(
        &self,
        attempt_id: &str,
        actor_device_id: Option<&str>,
    ) -> Result<ReviewAttemptState, StoreError> {
        let row = load_attempt_row(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)?;
        require_session_actor(
            &self.connection,
            &row.session_id,
            actor_device_id,
            ReviewSessionMode::Online,
        )?;
        Ok(ReviewAttemptState {
            attempt_revision: row.mutation_revision,
            attempt: load_attempt(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)?,
        })
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
        if row.accepted_success == Some(false) && !row.desired_suspended {
            append_relearning_item(&transaction, &row.session_id, &row.thema_id)?;
        }
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
            &operation.device_id,
        )? {
            let result =
                load_offline_result(&transaction, &operation.device_id, &operation.attempt_id)?
                    .ok_or(StoreError::CorruptData {
                        field: "offline review result",
                    })?;
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
                scoped_operation_id(&operation.device_id, &operation.attempt_id),
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
    insert_offline_result(transaction, &operation.device_id, &result, reconciled_at_us)?;
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
        && operation.config == item.config
        && operation.matcher_version == item.matcher_version
        && operation.proposed_transition.before == item.review_state
        && operation.proposed_transition.config == item.config
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
    let parameters = operation.config.clone().into();
    let Ok(expected) = compute_review_transition(
        &before,
        &parameters,
        operation.accepted_success,
        operation.proposed_transition.review_day,
        operation.answered_at_us,
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
    let parameters = operation.config.clone().into();
    let transition = compute_review_transition(
        state,
        &parameters,
        operation.accepted_success,
        operation.proposed_transition.review_day,
        operation.answered_at_us,
    )?;
    Ok((&transition).into())
}

fn offline_attempt_row(
    operation: &OfflineReviewOperation,
    transition: TransitionSnapshot,
) -> AttemptRow {
    AttemptRow {
        attempt_id: scoped_operation_id(&operation.device_id, &operation.attempt_id),
        session_id: operation.session_id.clone(),
        thema_id: operation.thema_id.clone(),
        thema_revision: operation.thema_revision,
        state_revision: operation.base_state_revision,
        content_sha256: operation.content_sha256.clone(),
        presentation: operation.presentation.clone(),
        config: operation.config.clone(),
        matcher_version: operation.matcher_version,
        response: Some(operation.response.clone()),
        detected_success: Some(operation.detected_success),
        accepted_success: Some(operation.accepted_success),
        override_count: operation.override_count,
        transition: Some(transition),
        desired_suspended: operation.desired_suspended,
        status: "graded".into(),
        mutation_revision: 0,
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
                 state_revision, content_sha256, presentation_json, config_json,
                 matcher_version, response_json, detected_success, accepted_success,
                 override_count, proposed_transition_json, desired_suspended,
                 status, started_at_us, answered_at_us
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, 'graded', ?17, ?17
             )",
            params![
                scoped_operation_id(&operation.device_id, &operation.attempt_id),
                operation.session_id,
                i64::from(operation.session_position),
                operation.thema_id,
                u64_to_i64(operation.thema_revision, "thema revision")?,
                u64_to_i64(operation.base_state_revision, "state revision")?,
                operation.content_sha256,
                serde_json::to_string(&operation.presentation).map_err(StoreError::storage)?,
                serde_json::to_string(&operation.config).map_err(StoreError::storage)?,
                i64::from(operation.matcher_version),
                serde_json::to_string(&operation.response).map_err(StoreError::storage)?,
                i64::from(operation.detected_success),
                i64::from(operation.accepted_success),
                i64::from(operation.override_count),
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
    device_id: &str,
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
                scoped_operation_id(device_id, &result.attempt_id),
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
    device_id: &str,
    attempt_id: &str,
) -> Result<Option<OfflineReviewResult>, StoreError> {
    let storage_attempt_id = scoped_operation_id(device_id, attempt_id);
    let row = connection
        .query_row(
            "SELECT format_version, status, conflict_reason, event_id,
                    authoritative_state_json, session_json, cursor,
                    cursor_server_id, cursor_token
             FROM offline_review_result WHERE attempt_id = ?1",
            [&storage_attempt_id],
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
    scheduler: FsrsConfigSnapshot,
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

fn require_safe_review_question(
    presentation: &PresentationSnapshot,
    required: bool,
) -> Result<(), StoreError> {
    if required {
        review_question(presentation).map_err(|_| StoreError::InvalidReviewState)?;
    }
    Ok(())
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
    let queue = select_queue(connection, source, review_day, target_count)?;
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
    let config = resolved_config(connection, session_id)?;
    validate_offline_thema_assets(&thema.content).map_err(|_| StoreError::InvalidReviewState)?;
    let presentation = presentation(&thema);
    Ok(ReviewPackItem {
        position: u32::try_from(position).map_err(StoreError::storage)?,
        thema_id: thema.thema_id,
        thema_revision: thema.revision,
        content_sha256: thema.content_sha256,
        presentation,
        review_state: (&state).into(),
        config: (&config).into(),
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
                 WHERE asset.asset_path = ?2 AND asset.active = 1
                   AND blob.size_bytes <= ?3",
                params![pack_id, path, MAX_OFFLINE_ASSET_BYTES],
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
    let (timezone, boundary, scheduler): (String, i64, String) = connection
        .query_row(
            "SELECT timezone, day_boundary_hour, scheduler_json FROM settings WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(StoreError::storage)?;
    let scheduler: FsrsConfigSnapshot =
        serde_json::from_str(&scheduler).map_err(StoreError::storage)?;
    if !scheduler.valid() {
        return Err(StoreError::CorruptData {
            field: "FSRS configuration",
        });
    }
    Ok(CapturedSettings {
        timezone,
        day_boundary_hour: u8::try_from(boundary).map_err(|_| StoreError::CorruptData {
            field: "day boundary",
        })?,
        scheduler,
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

pub(super) fn select_queue(
    connection: &Connection,
    source: &ReviewSource,
    review_day: u32,
    target_count: Option<u32>,
) -> Result<Vec<String>, StoreError> {
    let limit = target_count.map_or(-1, i64::from);
    if source.kind == ReviewSourceKind::Manual {
        return select_manual(connection, &source.manual_ids, target_count);
    }
    if source.kind == ReviewSourceKind::Topic {
        return select_topic(
            connection,
            source.topic_node_id.as_deref().unwrap_or_default(),
            source.topic_forward_depth.unwrap_or_default(),
            source.topic_backward_depth.unwrap_or_default(),
            limit,
        );
    }
    select_filtered(connection, source, review_day, limit)
}

fn select_manual(
    connection: &Connection,
    ids: &[String],
    target_count: Option<u32>,
) -> Result<Vec<String>, StoreError> {
    let mut selected = Vec::new();
    for id in ids.iter().take(MAX_REVIEW_PACK_ITEMS) {
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
            if target_count.is_some_and(|count| selected.len() == count as usize) {
                break;
            }
        }
    }
    Ok(selected)
}

fn select_topic(
    connection: &Connection,
    node_id: &str,
    forward_depth: u32,
    backward_depth: u32,
    limit: i64,
) -> Result<Vec<String>, StoreError> {
    let mut statement = connection
        .prepare(
            "WITH RECURSIVE
             walk(node_id, depth, direction) AS (
                 VALUES (?1, 0, 0), (?1, 0, 1)
                 UNION
                 SELECT edge.target_id, walk.depth + 1, walk.direction
                 FROM node_edge AS edge
                 JOIN walk ON walk.direction = 0 AND edge.source_id = walk.node_id
                 WHERE walk.depth < ?2
                 UNION
                 SELECT edge.source_id, walk.depth + 1, walk.direction
                 FROM node_edge AS edge
                 JOIN walk ON walk.direction = 1 AND edge.target_id = walk.node_id
                 WHERE walk.depth < ?3
                 LIMIT ?4
             ),
             traversal(row_count) AS (
                 SELECT COUNT(*) FROM walk
             ),
             graph(node_id) AS (
                 SELECT DISTINCT node_id FROM walk
             ),
             candidate(thema_id, reviewed, next_review_day, traversal_exceeded) AS (
                 SELECT DISTINCT thema.thema_id, (state.review_count > 0),
                                 state.next_review_day, 0
                 FROM graph
                 JOIN thema_concept AS link ON link.concept_id = graph.node_id
                 JOIN thema ON thema.thema_id = link.thema_id
                 JOIN review_state AS state ON state.thema_id = thema.thema_id
                 CROSS JOIN traversal
                 WHERE thema.archived_at_us IS NULL AND state.suspended = 0
                   AND traversal.row_count <= ?5
                 UNION ALL
                 SELECT NULL, 0, 0, 1 FROM traversal WHERE row_count > ?5
             )
         SELECT thema_id, traversal_exceeded FROM candidate
         ORDER BY traversal_exceeded DESC, reviewed, next_review_day, thema_id
         LIMIT ?6",
        )
        .map_err(StoreError::storage)?;
    let rows = statement
        .query_map(
            params![
                node_id,
                i64::from(forward_depth),
                i64::from(backward_depth),
                MAX_TOPIC_TRAVERSAL_ROWS + 1,
                MAX_TOPIC_TRAVERSAL_ROWS,
                limit
            ],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, bool>(1)?)),
        )
        .map_err(StoreError::storage)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::storage)?;
    if rows
        .first()
        .is_some_and(|(_, traversal_exceeded)| *traversal_exceeded)
    {
        return Err(StoreError::TopicTraversalLimitExceeded);
    }
    Ok(rows
        .into_iter()
        .filter_map(|(thema_id, _)| thema_id)
        .collect())
}

fn select_filtered(
    connection: &Connection,
    source: &ReviewSource,
    review_day: u32,
    limit: i64,
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
         ORDER BY (state.review_count > 0), state.next_review_day, thema.thema_id
         LIMIT ?4"
    );
    let mut statement = connection.prepare(&query).map_err(StoreError::storage)?;
    statement
        .query_map(
            params![i64::from(review_day), include, exclude, limit],
            |row| row.get(0),
        )
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
    type RawState = (
        i64,
        i64,
        Option<i64>,
        Option<i64>,
        i64,
        Option<i64>,
        i64,
        i64,
        bool,
        Option<f32>,
        Option<f32>,
    );
    let row: RawState = connection
        .query_row(
            "SELECT state_revision, review_count, last_reviewed_at_us, last_review_day,
                next_review_day, interval_days, total_successes, total_failures,
                suspended, stability, difficulty
             FROM review_state WHERE thema_id = ?1",
            [thema_id],
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
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?
        .ok_or(StoreError::NotFound)?;
    let optional_u64 =
        |value: Option<i64>, field| value.map(|value| to_u64(value, field)).transpose();
    let optional_u32 =
        |value: Option<i64>, field| value.map(|value| to_u32(value, field)).transpose();
    let snapshot = ReviewStateSnapshot {
        state_revision: to_u64(row.0, "state revision")?,
        review_count: to_u64(row.1, "review count")?,
        last_reviewed_at_us: optional_u64(row.2, "last review timestamp")?,
        last_review_day: optional_u32(row.3, "last review day")?,
        next_review_day: to_u32(row.4, "next review day")?,
        interval_days: optional_u32(row.5, "review interval")?,
        total_successes: to_u64(row.6, "total successes")?,
        total_failures: to_u64(row.7, "total failures")?,
        suspended: row.8,
        stability: row.9,
        difficulty: row.10,
    };
    crate::domain::validate_state(&snapshot.clone().into()).map_err(StoreError::storage)?;
    Ok(snapshot.into())
}

fn resolved_config(
    connection: &Connection,
    session_id: &str,
) -> Result<crate::domain::fsrs::FsrsConfigSnapshot, StoreError> {
    let captured: String = connection
        .query_row(
            "SELECT scheduler_json FROM review_session WHERE session_id = ?1",
            [session_id],
            |row| row.get(0),
        )
        .map_err(StoreError::storage)?;
    let captured: CapturedSettings =
        serde_json::from_str(&captured).map_err(StoreError::storage)?;
    if !captured.scheduler.valid() {
        return Err(StoreError::CorruptData {
            field: "FSRS configuration",
        });
    }
    Ok(captured.scheduler.into())
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
    config: FsrsConfigSnapshot,
    matcher_version: u32,
    response: Option<AnswerInput>,
    detected_success: Option<bool>,
    accepted_success: Option<bool>,
    override_count: u32,

    transition: Option<TransitionSnapshot>,
    desired_suspended: bool,
    status: String,
    mutation_revision: u64,
}

fn load_attempt_row(
    connection: &Connection,
    attempt_id: &str,
) -> Result<Option<AttemptRow>, StoreError> {
    let row = connection
        .query_row(
            "SELECT attempt_id, session_id, thema_id, thema_revision, state_revision, content_sha256,
                presentation_json, config_json, matcher_version, response_json, detected_success,
                accepted_success, override_count, proposed_transition_json,
                desired_suspended, status, mutation_revision
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
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, bool>(14)?,
                    row.get::<_, String>(15)?,
                    row.get::<_, i64>(16)?,
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
    Option<String>,
    bool,
    String,
    i64,
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
        config: serde_json::from_str(&row.7).map_err(StoreError::storage)?,
        matcher_version: to_u32(row.8, "attempt matcher version")?,
        response: row
            .9
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(StoreError::storage)?,
        detected_success: row.10,
        accepted_success: row.11,
        override_count: to_u32(row.12, "override count")?,
        transition: row
            .13
            .map(|value| serde_json::from_str(&value))
            .transpose()
            .map_err(StoreError::storage)?,
        desired_suspended: row.14,
        status: row.15,
        mutation_revision: to_u64(row.16, "attempt mutation revision")?,
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
    let config = row.config.clone().into();
    let transition = compute_review_transition(
        &state,
        &config,
        accepted,
        previous.review_day,
        previous.reviewed_at_us,
    )?;
    connection
        .execute(
            "UPDATE review_attempt SET accepted_success = ?2,
             override_count = override_count + 1, proposed_transition_json = ?3,
             mutation_revision = mutation_revision + 1
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
            "UPDATE review_attempt SET desired_suspended = 1 - desired_suspended,
             mutation_revision = mutation_revision + 1
             WHERE attempt_id = ?1 AND status IN ('pending', 'graded')",
            [attempt_id],
        )
        .map_err(StoreError::storage)?;
    if changed == 0 {
        return Err(StoreError::InvalidReviewState);
    }
    load_attempt(connection, attempt_id)?.ok_or(StoreError::NotFound)
}

fn canonical_attempt_operation(operation: &ReviewAttemptOperation) -> Result<String, StoreError> {
    serde_json::to_string(&StoredAttemptOperationPayload {
        format_version: operation.format_version,
        operation_id: &operation.operation_id,
        attempt_id: &operation.attempt_id,
        expected_attempt_revision: operation.expected_attempt_revision,
        mutation: operation.mutation,
    })
    .map_err(StoreError::storage)
}

fn reconcile_attempt_operation(
    connection: &Connection,
    operation: &ReviewAttemptOperation,
    authority: &ReviewOperationAuthority,
    payload: &str,
    digest: &str,
    operated_at_us: u64,
) -> Result<ReviewAttemptOperationResult, StoreError> {
    if let Some(result) = replayed_attempt_operation(connection, operation, authority, digest)? {
        return Ok(result);
    }
    let result = apply_new_attempt_operation(connection, operation, None)?;
    insert_attempt_operation(
        connection,
        operation,
        authority,
        payload,
        digest,
        &result,
        operated_at_us,
    )?;
    Ok(result)
}

fn replayed_attempt_operation(
    connection: &Connection,
    operation: &ReviewAttemptOperation,
    actor: &ReviewOperationAuthority,
    payload_sha256: &str,
) -> Result<Option<ReviewAttemptOperationResult>, StoreError> {
    let stored = connection
        .query_row(
            ATTEMPT_OPERATION_REPLAY_SQL,
            params![operation.operation_id, actor.device_id()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(StoreError::storage)?;
    let Some((digest, result_json)) = stored else {
        return Ok(None);
    };
    if digest != payload_sha256 {
        return Err(StoreError::IdempotencyConflict);
    }
    serde_json::from_str(&result_json)
        .map(Some)
        .map_err(StoreError::storage)
}

fn apply_new_attempt_operation(
    connection: &Connection,
    operation: &ReviewAttemptOperation,
    actor_device_id: Option<&str>,
) -> Result<ReviewAttemptOperationResult, StoreError> {
    let row = load_attempt_row(connection, &operation.attempt_id)?.ok_or(StoreError::NotFound)?;
    require_session_actor(
        connection,
        &row.session_id,
        actor_device_id,
        ReviewSessionMode::Online,
    )?;
    if let Some(status) = attempt_operation_conflict(connection, operation, &row)? {
        return attempt_operation_result(
            connection,
            operation,
            status,
            row.mutation_revision,
            false,
        );
    }
    let changed = apply_attempt_mutation(connection, operation, &row)?;
    let revision = row
        .mutation_revision
        .checked_add(1)
        .ok_or(StoreError::CorruptData {
            field: "attempt mutation revision",
        })?;
    attempt_operation_result(
        connection,
        operation,
        ReviewAttemptOperationStatus::Accepted,
        revision,
        changed,
    )
}

fn attempt_operation_conflict(
    connection: &Connection,
    operation: &ReviewAttemptOperation,
    row: &AttemptRow,
) -> Result<Option<ReviewAttemptOperationStatus>, StoreError> {
    if row.mutation_revision != operation.expected_attempt_revision {
        return Ok(Some(ReviewAttemptOperationStatus::RevisionConflict));
    }
    if !attempt_mutation_allowed(row, operation.mutation) {
        return Ok(Some(ReviewAttemptOperationStatus::StateConflict));
    }
    match verify_live_attempt(connection, row) {
        Ok(()) => Ok(None),
        Err(StoreError::StaleAttempt) => Ok(Some(ReviewAttemptOperationStatus::StaleAttempt)),
        Err(StoreError::InvalidReviewState) => {
            Ok(Some(ReviewAttemptOperationStatus::StateConflict))
        }
        Err(error) => Err(error),
    }
}

fn attempt_mutation_allowed(row: &AttemptRow, mutation: ReviewAttemptMutation) -> bool {
    match mutation {
        ReviewAttemptMutation::AcceptedSuccess(_) => row.status == "graded",
        ReviewAttemptMutation::Suspension(_) => matches!(row.status.as_str(), "pending" | "graded"),
    }
}

fn apply_attempt_mutation(
    connection: &Connection,
    operation: &ReviewAttemptOperation,
    row: &AttemptRow,
) -> Result<bool, StoreError> {
    match operation.mutation {
        ReviewAttemptMutation::AcceptedSuccess(value) => {
            set_attempt_accepted_success(connection, row, value)
        }
        ReviewAttemptMutation::Suspension(value) => set_attempt_suspension(connection, row, value),
    }
}

fn set_attempt_accepted_success(
    connection: &Connection,
    row: &AttemptRow,
    accepted: bool,
) -> Result<bool, StoreError> {
    let current = row.accepted_success.ok_or(StoreError::InvalidReviewState)?;
    if current == accepted {
        advance_attempt_revision(connection, row)?;
        return Ok(false);
    }
    let previous = row
        .transition
        .as_ref()
        .ok_or(StoreError::InvalidReviewState)?;
    let state = load_review_state(connection, &row.thema_id)?;
    let transition = compute_review_transition(
        &state,
        &row.config.clone().into(),
        accepted,
        previous.review_day,
        previous.reviewed_at_us,
    )?;
    update_attempt_acceptance(connection, row, accepted, &transition)?;
    Ok(true)
}

fn update_attempt_acceptance(
    connection: &Connection,
    row: &AttemptRow,
    accepted: bool,
    transition: &crate::domain::Transition,
) -> Result<(), StoreError> {
    let changed = connection
        .execute(
            "UPDATE review_attempt SET accepted_success = ?2,
             override_count = override_count + 1, proposed_transition_json = ?3,
             mutation_revision = mutation_revision + 1
             WHERE attempt_id = ?1 AND mutation_revision = ?4",
            params![
                row.attempt_id,
                i64::from(accepted),
                serde_json::to_string(&TransitionSnapshot::from(transition))
                    .map_err(StoreError::storage)?,
                u64_to_i64(row.mutation_revision, "attempt mutation revision")?,
            ],
        )
        .map_err(StoreError::storage)?;
    require_attempt_updated(connection, row, changed)
}

fn set_attempt_suspension(
    connection: &Connection,
    row: &AttemptRow,
    suspended: bool,
) -> Result<bool, StoreError> {
    let changed_value = row.desired_suspended != suspended;
    let changed = connection
        .execute(
            "UPDATE review_attempt SET desired_suspended = ?2,
             mutation_revision = mutation_revision + 1
             WHERE attempt_id = ?1 AND mutation_revision = ?3",
            params![
                row.attempt_id,
                i64::from(suspended),
                u64_to_i64(row.mutation_revision, "attempt mutation revision")?,
            ],
        )
        .map_err(StoreError::storage)?;
    require_attempt_updated(connection, row, changed)?;
    Ok(changed_value)
}

fn advance_attempt_revision(connection: &Connection, row: &AttemptRow) -> Result<(), StoreError> {
    let changed = connection
        .execute(
            "UPDATE review_attempt SET mutation_revision = mutation_revision + 1
             WHERE attempt_id = ?1 AND mutation_revision = ?2",
            params![
                row.attempt_id,
                u64_to_i64(row.mutation_revision, "attempt mutation revision")?,
            ],
        )
        .map_err(StoreError::storage)?;
    require_attempt_updated(connection, row, changed)
}

fn require_attempt_updated(
    connection: &Connection,
    row: &AttemptRow,
    changed: usize,
) -> Result<(), StoreError> {
    if changed == 1 {
        return Ok(());
    }
    Err(StoreError::Conflict {
        expected: row.mutation_revision,
        actual: load_attempt_row(connection, &row.attempt_id)?
            .map(|attempt| attempt.mutation_revision),
    })
}

fn attempt_operation_result(
    connection: &Connection,
    operation: &ReviewAttemptOperation,
    status: ReviewAttemptOperationStatus,
    attempt_revision: u64,
    changed: bool,
) -> Result<ReviewAttemptOperationResult, StoreError> {
    Ok(ReviewAttemptOperationResult {
        format_version: 1,
        operation_id: operation.operation_id.clone(),
        status,
        expected_attempt_revision: operation.expected_attempt_revision,
        attempt_revision,
        changed,
        attempt: load_attempt(connection, &operation.attempt_id)?.ok_or(StoreError::NotFound)?,
    })
}

fn insert_attempt_operation(
    connection: &Connection,
    operation: &ReviewAttemptOperation,
    actor: &ReviewOperationAuthority,
    payload_json: &str,
    payload_sha256: &str,
    result: &ReviewAttemptOperationResult,
    created_at_us: u64,
) -> Result<(), StoreError> {
    connection
        .execute(
            "INSERT INTO review_attempt_operation(
             operation_id, format_version, actor_device_id, attempt_id,
             operation_kind, expected_revision, desired_value, payload_sha256,
             payload_json, result_kind, result_json, created_at_us
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                operation.operation_id,
                operation.format_version,
                actor.device_id(),
                operation.attempt_id,
                operation.mutation.kind(),
                u64_to_i64(
                    operation.expected_attempt_revision,
                    "expected attempt revision"
                )?,
                i64::from(operation.mutation.desired_value()),
                payload_sha256,
                payload_json,
                result.status.as_str(),
                serde_json::to_string(result).map_err(StoreError::storage)?,
                u64_to_i64(created_at_us, "review operation timestamp")?,
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

fn compute_review_transition(
    state: &crate::domain::ReviewState,
    config: &crate::domain::fsrs::FsrsConfigSnapshot,
    success: bool,
    review_day: u32,
    reviewed_at_us: u64,
) -> Result<crate::domain::Transition, StoreError> {
    compute_transition(state, config, success, review_day, reviewed_at_us)
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
             interval_days = ?7, total_successes = ?8, total_failures = ?9,
             suspended = ?10, stability = ?11, difficulty = ?12
             WHERE thema_id = ?1 AND state_revision = ?13",
            params![
                row.thema_id,
                after.state_revision,
                after.review_count,
                after.last_reviewed_at_us,
                after.last_review_day,
                after.next_review_day,
                after.interval_days,
                after.total_successes,
                after.total_failures,
                i64::from(after.suspended),
                after.stability,
                after.difficulty,
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
             override_count, config_json, transition_json
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
                serde_json::to_string(&transition.config).map_err(StoreError::storage)?,
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
    let changed = transaction
        .execute(
            "UPDATE review_attempt SET status = 'committed', committed_at_us = ?2,
             mutation_revision = mutation_revision + 1
             WHERE attempt_id = ?1 AND mutation_revision = ?3",
            params![
                row.attempt_id,
                u64_to_i64(committed_at_us, "commit timestamp")?,
                u64_to_i64(row.mutation_revision, "attempt mutation revision")?,
            ],
        )
        .map_err(StoreError::storage)?;
    require_attempt_updated(transaction, row, changed)?;
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

fn append_relearning_item(
    transaction: &Transaction<'_>,
    session_id: &str,
    thema_id: &str,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO review_session_item(session_id, position, thema_id)
             SELECT ?1, count(*), ?2
             FROM review_session_item
             WHERE session_id = ?1",
            params![session_id, thema_id],
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
                i64::from(transition.outcome == RecallOutcomeSnapshot::Success),
                i64::from(transition.outcome == RecallOutcomeSnapshot::Failure),
                override_count,
            ],
        )
        .map_err(StoreError::storage)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rusqlite::params;
    use tempfile::tempdir;

    use super::{ATTEMPT_OPERATION_REPLAY_SQL, MAX_REVIEW_PACK_ITEMS, SqliteStore};
    use crate::application::{
        AnswerInput, MAX_OFFLINE_ASSET_BYTES, OfflineConflictReason, OfflineReviewOperation,
        ReconciliationStatus, ReviewAttemptMutation, ReviewAttemptOperation,
        ReviewAttemptOperationStatus, ReviewOperationAuthority, ReviewSource, ReviewSourceKind,
        canonical_asset_path, generate_device, prepare_asset, prepare_thema, presentation_content,
        review_day_at, thema_content,
    };
    use crate::blob::BlobMeta;
    use crate::store::StoreError;

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

    fn add_thema(store: &mut SqliteStore, id: &str, question: &str, revision: Option<u64>) {
        add_thema_with_tags(store, id, question, vec![], revision);
    }

    fn manual_source(id: &str) -> ReviewSource {
        manual_source_many(&[id])
    }

    fn manual_source_many(ids: &[&str]) -> ReviewSource {
        ReviewSource {
            kind: ReviewSourceKind::Manual,
            tags: vec![],
            exclude_tags: vec![],
            topic_node_id: None,
            topic_forward_depth: None,
            topic_backward_depth: None,
            manual_ids: ids.iter().map(|id| (*id).into()).collect(),
        }
    }

    fn topic_source(node_id: &str, forward_depth: u32, backward_depth: u32) -> ReviewSource {
        ReviewSource {
            kind: ReviewSourceKind::Topic,
            tags: vec![],
            exclude_tags: vec![],
            topic_node_id: Some(node_id.into()),
            topic_forward_depth: Some(forward_depth),
            topic_backward_depth: Some(backward_depth),
            manual_ids: vec![],
        }
    }

    fn add_topic_nodes(store: &mut SqliteStore, node_ids: &[String]) {
        let transaction = store.connection.transaction().unwrap();
        transaction
            .execute(
                "INSERT INTO document(
                     document_id, revision, title, file_tags_json, content_sha256
                 ) VALUES ('topic-document', 1, 'Topics', '[]', 'digest')",
                [],
            )
            .unwrap();
        for (ordinal, node_id) in node_ids.iter().enumerate() {
            transaction
                .execute("INSERT INTO node_identity(node_id) VALUES (?1)", [node_id])
                .unwrap();
            transaction
                .execute(
                    "INSERT INTO node(
                         document_id, ordinal, node_id, title, title_path_json, level
                     ) VALUES ('topic-document', ?1, ?2, ?2, '[]', 1)",
                    params![ordinal as i64, node_id],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
    }

    #[test]
    fn topic_queue_preserves_ordering_and_result_limit_below_traversal_budget() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_topic_nodes(
            &mut store,
            &["backward".into(), "root".into(), "forward".into()],
        );
        store
            .connection
            .execute(
                "INSERT INTO node_edge(document_id, ordinal, source_id, target_id)
                 VALUES ('topic-document', 0, 'backward', 'root'),
                        ('topic-document', 1, 'root', 'forward')",
                [],
            )
            .unwrap();
        for (thema_id, concept_id) in [
            ("thema-c", "root"),
            ("thema-b", "forward"),
            ("thema-a", "backward"),
        ] {
            add_thema(&mut store, thema_id, "Question", None);
            store
                .connection
                .execute(
                    "INSERT INTO thema_concept(thema_id, concept_id, relation)
                     VALUES (?1, ?2, 'primary')",
                    params![thema_id, concept_id],
                )
                .unwrap();
        }

        assert_eq!(
            super::select_queue(
                &store.connection,
                &topic_source("root", 1, 1),
                20260715,
                Some(2),
            )
            .unwrap(),
            ["thema-a", "thema-b"]
        );
    }

    #[test]
    fn topic_queue_rejects_wide_traversal_and_remains_usable() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_topic_nodes(&mut store, &["root".into()]);
        let transaction = store.connection.transaction().unwrap();
        for ordinal in 0..1_200 {
            transaction
                .execute(
                    "INSERT INTO node_edge(document_id, ordinal, source_id, target_id)
                     VALUES ('topic-document', ?1, 'root', ?2)",
                    params![ordinal, format!("target-{ordinal}")],
                )
                .unwrap();
        }
        transaction.commit().unwrap();

        assert!(matches!(
            super::select_queue(
                &store.connection,
                &topic_source("root", 1, 0),
                20260715,
                Some(1),
            ),
            Err(StoreError::TopicTraversalLimitExceeded)
        ));
        store.settings().unwrap();
    }

    #[test]
    fn topic_queue_rejects_dense_cyclic_traversal() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let node_ids = (0..40)
            .map(|index| format!("node-{index}"))
            .collect::<Vec<_>>();
        add_topic_nodes(&mut store, &node_ids);
        let transaction = store.connection.transaction().unwrap();
        let mut ordinal = 0;
        for source_id in &node_ids {
            for target_id in &node_ids {
                transaction
                    .execute(
                        "INSERT INTO node_edge(document_id, ordinal, source_id, target_id)
                         VALUES ('topic-document', ?1, ?2, ?3)",
                        params![ordinal, source_id, target_id],
                    )
                    .unwrap();
                ordinal += 1;
            }
        }
        transaction.commit().unwrap();

        assert!(matches!(
            super::select_queue(
                &store.connection,
                &topic_source("node-0", 32, 32),
                20260715,
                Some(1),
            ),
            Err(StoreError::TopicTraversalLimitExceeded)
        ));
    }

    fn grade_and_commit(
        store: &mut SqliteStore,
        session_id: &str,
        answer: &str,
        started_at_us: u64,
    ) -> (crate::application::ReviewAttemptView, String) {
        let attempt = store
            .begin_review_attempt(session_id, None, started_at_us)
            .unwrap();
        let graded = store
            .grade_review_attempt(
                &attempt.attempt_id,
                None,
                &AnswerInput::Text(answer.into()),
                started_at_us + 1,
            )
            .unwrap();
        let event_id = store
            .commit_review_attempt(&attempt.attempt_id, None, started_at_us + 2)
            .unwrap();
        (graded, event_id)
    }

    fn add_device(store: &mut SqliteStore, name: &str) -> String {
        let device = generate_device(name.into(), vec!["review".into()]).unwrap();
        let id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        id
    }

    #[test]
    fn malformed_stored_presentations_do_not_create_attempts() {
        for (kind, question, hypotheses) in [
            ("basic", "", r#"[null]"#),
            ("mcq", "Question", r#"["only"]"#),
        ] {
            let mut store = SqliteStore::open_in_memory().unwrap();
            add_thema(&mut store, "thema", "Question", None);
            store
                .connection
                .execute(
                    "UPDATE thema SET kind = ?1, keimenon = ?2,
                     hypothesis_json = ?3, answer_json = '[\"only\"]'
                     WHERE thema_id = 'thema'",
                    params![kind, question, hypotheses],
                )
                .unwrap();
            let session = store
                .create_review_session(&manual_source("thema"), Some(1), None, 2)
                .unwrap();

            assert!(matches!(
                store.begin_http_review_attempt(&session.session_id, None, 3),
                Err(crate::store::StoreError::InvalidReviewState)
            ));
            assert_eq!(
                store
                    .connection
                    .query_row("SELECT count(*) FROM review_attempt", [], |row| row
                        .get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
    }

    #[test]
    fn local_review_begin_keeps_raw_compatibility_presentation() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "The answer is answer", None);
        let device_id = add_device(&mut store, "reviewer");
        let session = store
            .create_review_session(&manual_source("thema"), Some(1), Some(&device_id), 2)
            .unwrap();

        let attempt = store
            .begin_review_attempt(&session.session_id, None, 3)
            .unwrap();

        assert_eq!(attempt.presentation.keimenon, "The answer is answer");
        assert_eq!(attempt.presentation.answer, vec!["answer"]);
        assert!(matches!(
            store.begin_http_review_attempt(&session.session_id, Some(&device_id), 4),
            Err(crate::store::StoreError::InvalidReviewState)
        ));
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM review_attempt", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
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
        let transition = super::compute_review_transition(
            &item.review_state.clone().into(),
            &item.config.clone().into(),
            detected,
            review_day,
            reviewed_at_us,
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
            config: item.config.clone(),
            matcher_version: item.matcher_version,
            response,
            detected_success: detected,
            accepted_success: detected,
            override_count: 0,
            proposed_transition: (&transition).into(),
            desired_suspended: false,
            answered_at_us: reviewed_at_us,
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
    fn offline_attempt_ids_are_scoped_to_device() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let devices = [
            add_device(&mut store, "first-offline"),
            add_device(&mut store, "second-offline"),
        ];

        for (index, device_id) in devices.iter().enumerate() {
            let pack = store
                .create_review_pack(
                    1,
                    &manual_source("thema"),
                    Some(1),
                    device_id,
                    1_752_600_000_000_000 + u64::try_from(index).unwrap() * 10_000,
                )
                .unwrap();
            let operation = offline_answer(&pack, "shared-attempt", 1);
            reconcile(
                &mut store,
                &operation,
                device_id,
                1_752_600_000_002_000 + u64::try_from(index).unwrap() * 10_000,
            )
            .unwrap();
        }

        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM offline_review_operation", [], |row| {
                    row.get::<_, u64>(0)
                })
                .unwrap(),
            2
        );
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
        let transition = super::compute_review_transition(
            &pack.items[0].review_state.clone().into(),
            &pack.items[0].config.clone().into(),
            operation.accepted_success,
            forged_day,
            operation.answered_at_us,
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
    fn review_pack_rejects_oversized_assets_without_starting_a_session() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let digest = "a".repeat(64);
        let path = canonical_asset_path(&digest, "image.png").unwrap();
        let asset = prepare_asset(path.clone(), "image.png".into(), None).unwrap();
        store
            .put_asset(
                &asset,
                &BlobMeta {
                    sha256: digest,
                    size_bytes: MAX_OFFLINE_ASSET_BYTES + 1,
                    media_type: "image/png".into(),
                },
                1,
                None,
            )
            .unwrap();
        let content = thema_content(
            "basic",
            "Question".into(),
            vec![None],
            vec!["answer".into()],
            None,
            None,
            Some(path.clone()),
        )
        .unwrap();
        let thema = prepare_thema(
            Some("thema".into()),
            content,
            vec![],
            vec![],
            vec![path],
            None,
        )
        .unwrap();
        store.put_thema(&thema, 20260715, 2, None).unwrap();
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
        let totals = store
            .connection
            .query_row(
                "SELECT review_count, new_count, success_count, failure_count, override_count
                 FROM review_day",
                [],
                |row| {
                    Ok((
                        row.get::<_, u32>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, u32>(2)?,
                        row.get::<_, u32>(3)?,
                        row.get::<_, u32>(4)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(totals, (1, 1, 1, 0, 0));
    }

    #[test]
    fn failed_online_reviews_return_at_session_tail_until_passed() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "a", "A", None);
        add_thema(&mut store, "b", "B", None);
        let session = store
            .create_review_session(
                &manual_source_many(&["a", "b"]),
                Some(2),
                None,
                1_752_600_000_000_000,
            )
            .unwrap();

        let (first_failure, first_event) = grade_and_commit(
            &mut store,
            &session.session_id,
            "wrong",
            1_752_600_000_001_000,
        );
        assert_eq!(first_failure.thema_id, "a");
        let after_failure = store.review_session(&session.session_id).unwrap().unwrap();
        assert_eq!(
            (after_failure.current_position, after_failure.queue_length),
            (1, 3)
        );
        assert_eq!(after_failure.status, "active");
        assert_eq!(
            store
                .commit_review_attempt(&first_failure.attempt_id, None, 1_752_600_000_002_000,)
                .unwrap(),
            first_event
        );
        assert_eq!(
            store
                .review_session(&session.session_id)
                .unwrap()
                .unwrap()
                .queue_length,
            3
        );
        let failed_state = super::load_review_state(&store.connection, "a").unwrap();
        assert!(failed_state.next_review_day > failed_state.last_review_day.unwrap());

        let (second, _) = grade_and_commit(
            &mut store,
            &session.session_id,
            "answer",
            1_752_600_000_003_000,
        );
        assert_eq!(second.thema_id, "b");
        let repeated = store
            .begin_review_attempt(&session.session_id, None, 1_752_600_000_004_000)
            .unwrap();
        assert_eq!(repeated.thema_id, "a");
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT state_revision FROM review_attempt WHERE attempt_id = ?1",
                    [&repeated.attempt_id],
                    |row| row.get::<_, u64>(0),
                )
                .unwrap(),
            1
        );
        let repeated_failure = store
            .grade_review_attempt(
                &repeated.attempt_id,
                None,
                &AnswerInput::Text("wrong".into()),
                1_752_600_000_004_001,
            )
            .unwrap();
        assert_eq!(
            repeated_failure
                .proposed_transition
                .as_ref()
                .unwrap()
                .elapsed_days,
            0
        );
        store
            .commit_review_attempt(&repeated.attempt_id, None, 1_752_600_000_004_002)
            .unwrap();
        let repeated_session = store.review_session(&session.session_id).unwrap().unwrap();
        assert_eq!(
            (
                repeated_session.current_position,
                repeated_session.queue_length
            ),
            (3, 4)
        );
        assert_eq!(repeated_session.status, "active");

        let (final_attempt, _) = grade_and_commit(
            &mut store,
            &session.session_id,
            "answer",
            1_752_600_000_005_000,
        );
        assert_eq!(final_attempt.thema_id, "a");
        let completed = store.review_session(&session.session_id).unwrap().unwrap();
        assert_eq!((completed.current_position, completed.queue_length), (4, 4));
        assert_eq!(completed.status, "completed");

        let totals = store
            .connection
            .query_row(
                "SELECT review_count, new_count, success_count, failure_count FROM review_day",
                [],
                |row| {
                    Ok((
                        row.get::<_, u32>(0)?,
                        row.get::<_, u32>(1)?,
                        row.get::<_, u32>(2)?,
                        row.get::<_, u32>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(totals, (4, 2, 2, 2));
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM review_event", [], |row| {
                    row.get::<_, u32>(0)
                })
                .unwrap(),
            4
        );
    }

    #[test]
    fn suspended_failure_does_not_append_a_relearning_item() {
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
            .begin_review_attempt(&session.session_id, None, 1_752_600_000_001_000)
            .unwrap();
        store
            .grade_review_attempt(
                &attempt.attempt_id,
                None,
                &AnswerInput::Text("wrong".into()),
                1_752_600_000_001_001,
            )
            .unwrap();
        store
            .toggle_attempt_suspension(&attempt.attempt_id, None)
            .unwrap();
        store
            .commit_review_attempt(&attempt.attempt_id, None, 1_752_600_000_001_002)
            .unwrap();

        let completed = store.review_session(&session.session_id).unwrap().unwrap();
        assert_eq!((completed.current_position, completed.queue_length), (1, 1));
        assert_eq!(completed.status, "completed");
    }

    #[test]
    fn failed_attempt_and_relearning_append_roll_back_together() {
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
            .begin_review_attempt(&session.session_id, None, 1_752_600_000_001_000)
            .unwrap();
        store
            .grade_review_attempt(
                &attempt.attempt_id,
                None,
                &AnswerInput::Text("wrong".into()),
                1_752_600_000_001_001,
            )
            .unwrap();
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_online_commit
                 BEFORE UPDATE OF status ON review_attempt
                 WHEN NEW.status = 'committed'
                 BEGIN SELECT RAISE(ABORT, 'fail'); END;",
            )
            .unwrap();

        assert!(
            store
                .commit_review_attempt(&attempt.attempt_id, None, 1_752_600_000_001_002)
                .is_err()
        );
        let unchanged = store.review_session(&session.session_id).unwrap().unwrap();
        assert_eq!((unchanged.current_position, unchanged.queue_length), (0, 1));
        assert_eq!(unchanged.status, "active");
        assert_eq!(
            super::load_review_state(&store.connection, "thema")
                .unwrap()
                .review_count,
            0
        );
        assert_eq!(
            store
                .connection
                .query_row("SELECT count(*) FROM review_event", [], |row| {
                    row.get::<_, u32>(0)
                })
                .unwrap(),
            0
        );

        store
            .connection
            .execute_batch("DROP TRIGGER fail_online_commit")
            .unwrap();
        store
            .commit_review_attempt(&attempt.attempt_id, None, 1_752_600_000_001_003)
            .unwrap();
        let committed = store.review_session(&session.session_id).unwrap().unwrap();
        assert_eq!((committed.current_position, committed.queue_length), (1, 2));
    }

    #[test]
    fn quit_resume_preserves_relearning_tail_and_abort_stops_it() {
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
        grade_and_commit(
            &mut store,
            &session.session_id,
            "wrong",
            1_752_600_000_001_000,
        );

        let quit = store
            .stop_review_session(&session.session_id, None, false, 1_752_600_000_002_000)
            .unwrap();
        assert_eq!((quit.current_position, quit.queue_length), (1, 2));
        assert_eq!(quit.status, "quit");
        let resumed = store
            .resume_review_session(&session.session_id, None)
            .unwrap();
        assert_eq!((resumed.current_position, resumed.queue_length), (1, 2));
        assert_eq!(resumed.status, "active");
        let resumed_attempt = store
            .begin_review_attempt(&session.session_id, None, 1_752_600_000_003_000)
            .unwrap();
        assert_eq!(resumed_attempt.thema_id, "thema");
        let aborted = store
            .stop_review_session(&session.session_id, None, true, 1_752_600_000_004_000)
            .unwrap();
        assert_eq!((aborted.current_position, aborted.queue_length), (1, 2));
        assert_eq!(aborted.status, "aborted");
        assert_eq!(store.active_review_session(None).unwrap(), None);
        assert!(matches!(
            store.grade_review_attempt(
                &resumed_attempt.attempt_id,
                None,
                &AnswerInput::Text("answer".into()),
                1_752_600_000_005_000,
            ),
            Err(crate::store::StoreError::InvalidReviewState)
        ));
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
            )
            .unwrap_err();
        assert!(matches!(error, crate::store::StoreError::StaleAttempt));
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
            super::select_queue(&store.connection, &source, 20260715, None).unwrap(),
            ["keep"]
        );
    }

    #[test]
    fn filtered_review_selection_applies_target_in_sql() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        for id in ["a", "b", "c"] {
            add_thema(&mut store, id, "Question", None);
        }
        let source = ReviewSource {
            kind: ReviewSourceKind::Tag,
            tags: vec![],
            exclude_tags: vec![],
            topic_node_id: None,
            topic_forward_depth: None,
            topic_backward_depth: None,
            manual_ids: vec![],
        };

        assert_eq!(
            super::select_queue(&store.connection, &source, 20260715, Some(2)).unwrap(),
            ["a", "b"]
        );
    }

    #[test]
    fn session_store_normalizes_and_rejects_targets_before_state_access() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "bounded", "Question", None);
        let source = manual_source("bounded");

        for target_count in [Some(0), Some(501)] {
            assert!(matches!(
                store.create_review_session(&source, target_count, None, 1),
                Err(StoreError::InvalidReviewState)
            ));
            assert_eq!(store.active_review_session(None).unwrap(), None);
        }

        let session = store.create_review_session(&source, None, None, 1).unwrap();
        assert_eq!(session.target_count, Some(100));

        for target_count in [Some(0), Some(501)] {
            assert!(matches!(
                store.create_review_session(&source, target_count, None, 1),
                Err(StoreError::InvalidReviewState)
            ));
        }
        let oversized_source = ReviewSource {
            manual_ids: (0..=MAX_REVIEW_PACK_ITEMS)
                .map(|index| format!("thema-{index}"))
                .collect(),
            ..source.clone()
        };
        assert!(matches!(
            store.create_review_session(&oversized_source, Some(1), None, 1),
            Err(StoreError::InvalidReviewState)
        ));
        assert_eq!(
            store
                .active_review_session(None)
                .unwrap()
                .unwrap()
                .session_id,
            session.session_id
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

    #[test]
    fn online_attempt_mutations_advance_one_revision() {
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
            )
            .unwrap();
        assert_eq!(attempt_revision(&store, &attempt.attempt_id), 1);

        store
            .override_review_attempt(&attempt.attempt_id, None)
            .unwrap();
        assert_eq!(attempt_revision(&store, &attempt.attempt_id), 2);

        store
            .toggle_attempt_suspension(&attempt.attempt_id, None)
            .unwrap();
        assert_eq!(attempt_revision(&store, &attempt.attempt_id), 3);

        store
            .commit_review_attempt(&attempt.attempt_id, None, 1_752_600_000_002_000)
            .unwrap();
        assert_eq!(attempt_revision(&store, &attempt.attempt_id), 4);
    }

    #[test]
    fn desired_acceptance_operation_replays_without_mutating_twice() {
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
            )
            .unwrap();
        let operation = ReviewAttemptOperation::new(
            1,
            "operation-1".into(),
            attempt.attempt_id.clone(),
            1,
            ReviewAttemptMutation::AcceptedSuccess(false),
        )
        .unwrap();
        let device_id = add_device(&mut store, "device");

        let first = store
            .apply_review_attempt_operation(
                &operation,
                &ReviewOperationAuthority::DeviceAdministrator(device_id.clone()),
                1_752_600_000_002_000,
            )
            .unwrap();
        let replay = store
            .apply_review_attempt_operation(
                &operation,
                &ReviewOperationAuthority::DeviceAdministrator(device_id),
                1_752_600_000_003_000,
            )
            .unwrap();

        assert_eq!(first, replay);
        assert_eq!(first.status, ReviewAttemptOperationStatus::Accepted);
        assert!(first.changed);
        assert_eq!(first.attempt_revision, 2);
        assert_eq!(first.attempt.accepted_success, Some(false));
        assert_eq!(first.attempt.override_count, 1);
        assert_eq!(attempt_revision(&store, &attempt.attempt_id), 2);
    }

    #[test]
    fn no_op_consumes_revision_and_competing_operation_is_retained() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let attempt_id = graded_online_attempt(&mut store);
        let device_id = add_device(&mut store, "device");
        let no_op = attempt_operation(
            "no-op",
            &attempt_id,
            1,
            ReviewAttemptMutation::Suspension(false),
        );
        let no_op_result = apply_device_operation(&mut store, &device_id, &no_op, 2);
        assert!(!no_op_result.changed);
        assert_eq!(no_op_result.attempt_revision, 2);

        let competing = attempt_operation(
            "competing",
            &attempt_id,
            1,
            ReviewAttemptMutation::Suspension(true),
        );
        let conflict = apply_device_operation(&mut store, &device_id, &competing, 3);
        assert_eq!(
            conflict.status,
            ReviewAttemptOperationStatus::RevisionConflict
        );
        assert_eq!(conflict.attempt_revision, 2);
        assert!(!conflict.attempt.desired_suspended);

        let later = attempt_operation(
            "later",
            &attempt_id,
            2,
            ReviewAttemptMutation::Suspension(true),
        );
        assert!(apply_device_operation(&mut store, &device_id, &later, 4).changed);
        assert_eq!(
            apply_device_operation(&mut store, &device_id, &competing, 5),
            conflict
        );
        assert_eq!(attempt_revision(&store, &attempt_id), 3);
    }

    #[test]
    fn operation_identity_rejects_changed_payload() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let attempt_id = graded_online_attempt(&mut store);
        let device_id = add_device(&mut store, "device");
        let original = attempt_operation(
            "operation",
            &attempt_id,
            1,
            ReviewAttemptMutation::AcceptedSuccess(false),
        );
        apply_device_operation(&mut store, &device_id, &original, 2);
        let changed = attempt_operation(
            "operation",
            &attempt_id,
            2,
            ReviewAttemptMutation::AcceptedSuccess(true),
        );

        assert!(matches!(
            store.apply_review_attempt_operation(
                &changed,
                &ReviewOperationAuthority::DeviceAdministrator(device_id),
                3
            ),
            Err(crate::store::StoreError::IdempotencyConflict)
        ));
        assert_eq!(attempt_revision(&store, &attempt_id), 2);
    }

    #[test]
    fn operation_identity_is_scoped_to_the_authenticated_device() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let attempt_id = graded_online_attempt(&mut store);
        let first_device_id = add_device(&mut store, "first-device");
        let second_device_id = add_device(&mut store, "second-device");
        let first = attempt_operation(
            "shared-operation",
            &attempt_id,
            1,
            ReviewAttemptMutation::Suspension(true),
        );
        apply_device_operation(&mut store, &first_device_id, &first, 2);
        let second = attempt_operation(
            "shared-operation",
            &attempt_id,
            2,
            ReviewAttemptMutation::Suspension(false),
        );

        let result = store
            .apply_review_attempt_operation(
                &second,
                &ReviewOperationAuthority::DeviceAdministrator(second_device_id),
                3,
            )
            .unwrap();

        assert_eq!(result.status, ReviewAttemptOperationStatus::Accepted);
        assert_eq!(result.attempt_revision, 3);
        assert!(!result.attempt.desired_suspended);
    }

    #[test]
    fn operation_replay_lookup_uses_device_identity_index() {
        let store = SqliteStore::open_in_memory().unwrap();
        let plan = operation_replay_plan(&store, "device");
        assert!(
            plan.iter()
                .any(|line| line.contains("sqlite_autoindex_review_attempt_operation_1")),
            "{plan:?}"
        );
    }

    #[test]
    fn review_operation_receipts_reject_local_identity() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let attempt_id = graded_online_attempt(&mut store);
        let payload_sha256 = "a".repeat(64);

        assert!(
            store
                .connection
                .execute(
                    "INSERT INTO review_attempt_operation(
                     operation_id, format_version, actor_device_id, attempt_id,
                     operation_kind, expected_revision, desired_value, payload_sha256,
                     payload_json, result_kind, result_json, created_at_us
                     ) VALUES ('missing-device', 1, NULL, ?1,
                     'accepted_success_set', 1, 0, ?2, '{}', 'accepted', '{}', 1)",
                    params![attempt_id, payload_sha256],
                )
                .is_err()
        );

        assert!(
            store
                .connection
                .execute(
                    "INSERT INTO review_attempt_operation(
                     operation_id, format_version, actor_kind, actor_device_id, attempt_id,
                     operation_kind, expected_revision, desired_value, payload_sha256,
                     payload_json, result_kind, result_json, created_at_us
                     ) VALUES ('local-operation', 1, 'local', NULL, ?1,
                     'accepted_success_set', 1, 0, ?2, '{}', 'accepted', '{}', 1)",
                    params![attempt_id, payload_sha256],
                )
                .is_err()
        );
    }

    fn operation_replay_plan(store: &SqliteStore, actor_device_id: &str) -> Vec<String> {
        let sql = format!("EXPLAIN QUERY PLAN {ATTEMPT_OPERATION_REPLAY_SQL}");
        let mut statement = store.connection.prepare(&sql).unwrap();
        statement
            .query_map(rusqlite::params!["operation", actor_device_id], |row| {
                row.get(3)
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    }

    #[test]
    fn stale_content_operation_is_replayable_and_changes_nothing() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let attempt_id = graded_online_attempt(&mut store);
        let device_id = add_device(&mut store, "device");
        add_thema(&mut store, "thema", "Changed", Some(1));
        let operation = attempt_operation(
            "stale",
            &attempt_id,
            1,
            ReviewAttemptMutation::AcceptedSuccess(false),
        );

        let conflict = apply_device_operation(&mut store, &device_id, &operation, 2);
        assert_eq!(conflict.status, ReviewAttemptOperationStatus::StaleAttempt);
        assert_eq!(conflict.attempt_revision, 1);
        assert_eq!(conflict.attempt.accepted_success, Some(true));
        assert_eq!(
            apply_device_operation(&mut store, &device_id, &operation, 3),
            conflict
        );
        assert_eq!(attempt_revision(&store, &attempt_id), 1);
    }

    #[test]
    fn operation_replay_survives_store_restart() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("review-operation.sqlite");
        let (operation, device_id, first) = {
            let mut store = SqliteStore::open(&path).unwrap();
            let attempt_id = graded_online_attempt(&mut store);
            let device_id = add_device(&mut store, "device");
            let operation = attempt_operation(
                "restart",
                &attempt_id,
                1,
                ReviewAttemptMutation::Suspension(true),
            );
            let result = apply_device_operation(&mut store, &device_id, &operation, 2);
            (operation, device_id, result)
        };
        let mut reopened = SqliteStore::open(&path).unwrap();

        assert_eq!(
            apply_device_operation(&mut reopened, &device_id, &operation, 3),
            first
        );
    }

    #[test]
    fn failed_receipt_insert_rolls_back_attempt_mutation() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let attempt_id = graded_online_attempt(&mut store);
        let device_id = add_device(&mut store, "device");
        let operation = attempt_operation(
            "rollback",
            &attempt_id,
            1,
            ReviewAttemptMutation::AcceptedSuccess(false),
        );

        assert!(matches!(
            store.apply_review_attempt_operation(
                &operation,
                &ReviewOperationAuthority::DeviceAdministrator("missing".into()),
                2
            ),
            Err(crate::store::StoreError::Storage(_))
        ));
        assert_eq!(attempt_revision(&store, &attempt_id), 1);
        let applied = apply_device_operation(&mut store, &device_id, &operation, 3);
        assert_eq!(applied.status, ReviewAttemptOperationStatus::Accepted);
        assert_eq!(applied.attempt.accepted_success, Some(false));
    }

    fn graded_online_attempt(store: &mut SqliteStore) -> String {
        add_thema(store, "thema", "Question", None);
        let session = store
            .create_review_session(&manual_source("thema"), None, None, 1)
            .unwrap();
        let attempt = store
            .begin_review_attempt(&session.session_id, None, 2)
            .unwrap();
        store
            .grade_review_attempt(
                &attempt.attempt_id,
                None,
                &AnswerInput::Text("answer".into()),
                3,
            )
            .unwrap();
        attempt.attempt_id
    }

    fn attempt_operation(
        operation_id: &str,
        attempt_id: &str,
        expected_revision: u64,
        mutation: ReviewAttemptMutation,
    ) -> ReviewAttemptOperation {
        ReviewAttemptOperation::new(
            1,
            operation_id.into(),
            attempt_id.into(),
            expected_revision,
            mutation,
        )
        .unwrap()
    }

    fn apply_device_operation(
        store: &mut SqliteStore,
        device_id: &str,
        operation: &ReviewAttemptOperation,
        operated_at_us: u64,
    ) -> crate::application::ReviewAttemptOperationResult {
        store
            .apply_review_attempt_operation(
                operation,
                &ReviewOperationAuthority::DeviceAdministrator(device_id.into()),
                operated_at_us,
            )
            .unwrap()
    }

    fn attempt_revision(store: &SqliteStore, attempt_id: &str) -> u64 {
        store
            .connection
            .query_row(
                "SELECT mutation_revision FROM review_attempt WHERE attempt_id = ?1",
                [attempt_id],
                |row| row.get(0),
            )
            .unwrap()
    }
}
