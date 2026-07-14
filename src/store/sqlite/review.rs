use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{SqliteStore, StoreError, load_thema, to_u32, to_u64, u64_to_i64};
use crate::{
    application::{
        AnswerInput, ParametersSnapshot, PresentationSnapshot, ReviewAttemptView,
        ReviewSessionView, ReviewSource, ReviewSourceKind, ReviewStateSnapshot, TransitionSnapshot,
        presentation_content, review_day_at,
    },
    domain::{compute_transition, fuzz_delta, grade},
};

impl SqliteStore {
    pub(crate) fn create_review_session(
        &mut self,
        source: &ReviewSource,
        target_count: Option<u32>,
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

    pub(crate) fn active_review_session(&self) -> Result<Option<ReviewSessionView>, StoreError> {
        let id = self
            .connection
            .query_row(
                "SELECT session_id FROM review_session WHERE status = 'active'",
                [],
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
    ) -> Result<ReviewSessionView, StoreError> {
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
        abort: bool,
        ended_at_us: u64,
    ) -> Result<ReviewSessionView, StoreError> {
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
        started_at_us: u64,
    ) -> Result<ReviewAttemptView, StoreError> {
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
        response: &AnswerInput,
        answered_at_us: u64,
        fuzz_sample: f64,
    ) -> Result<ReviewAttemptView, StoreError> {
        let row = load_attempt_row(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)?;
        if row.status == "graded" || row.status == "committed" {
            return load_attempt(&self.connection, attempt_id)?.ok_or(StoreError::NotFound);
        }
        verify_live_attempt(&self.connection, &row)?;
        let content = presentation_content(&row.presentation).map_err(StoreError::storage)?;
        let result = grade(row.matcher_version, &content, &response.clone().into())
            .map_err(StoreError::storage)?;
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
    ) -> Result<ReviewAttemptView, StoreError> {
        let row = load_attempt_row(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)?;
        if row.status != "graded" {
            return Err(StoreError::InvalidReviewState);
        }
        verify_live_attempt(&self.connection, &row)?;
        let accepted = !row.accepted_success.ok_or(StoreError::InvalidReviewState)?;
        let previous = row
            .transition
            .as_ref()
            .ok_or(StoreError::InvalidReviewState)?;
        let state = load_review_state(&self.connection, &row.thema_id)?;
        let parameters = row.parameters.clone().into();
        let transition = transition_with_sample(
            &state,
            &parameters,
            accepted,
            previous.review_day,
            previous.reviewed_at_us,
            row.fuzz_sample.ok_or(StoreError::InvalidReviewState)?,
        )?;
        self.connection
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
        load_attempt(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)
    }

    pub(crate) fn toggle_attempt_suspension(
        &mut self,
        attempt_id: &str,
    ) -> Result<ReviewAttemptView, StoreError> {
        let changed = self
            .connection
            .execute(
                "UPDATE review_attempt SET desired_suspended = 1 - desired_suspended
             WHERE attempt_id = ?1 AND status IN ('pending', 'graded')",
                [attempt_id],
            )
            .map_err(StoreError::storage)?;
        if changed == 0 {
            return Err(StoreError::InvalidReviewState);
        }
        load_attempt(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)
    }

    pub(crate) fn commit_review_attempt(
        &mut self,
        attempt_id: &str,
        committed_at_us: u64,
    ) -> Result<String, StoreError> {
        if let Some(event_id) = committed_event(&self.connection, attempt_id)? {
            return Ok(event_id);
        }
        let row = load_attempt_row(&self.connection, attempt_id)?.ok_or(StoreError::NotFound)?;
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
        finish_attempt(&transaction, &row, &event_id, committed_at_us)?;
        update_review_day(&transaction, &transition, row.override_count)?;
        transaction.commit().map_err(StoreError::storage)?;
        Ok(event_id)
    }
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

fn insert_session(
    transaction: &Transaction<'_>,
    session_id: &str,
    source: &ReviewSource,
    target_count: Option<u32>,
    captured: &CapturedSettings,
    started_at_us: u64,
) -> Result<(), StoreError> {
    transaction
        .execute(
            "INSERT INTO review_session(
             session_id, source_kind, source_json, target_count, timezone,
             day_boundary_hour, scheduler_json, status, started_at_us
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'active', ?8)",
            params![
                session_id,
                source.kind.as_str(),
                serde_json::to_string(source).map_err(StoreError::storage)?,
                target_count.map(i64::from),
                captured.timezone,
                i64::from(captured.day_boundary_hour),
                serde_json::to_string(captured).map_err(StoreError::storage)?,
                u64_to_i64(started_at_us, "session start")?,
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
                started_at_us, (SELECT count(*) FROM review_session_item WHERE session_id = ?1)
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
                ))
            },
        )
        .optional()
        .map_err(StoreError::storage)?
        .map(|row| {
            Ok(ReviewSessionView {
                session_id: row.0,
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

fn load_review_state(
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
    use super::SqliteStore;
    use crate::application::{
        AnswerInput, ReviewSource, ReviewSourceKind, prepare_thema, thema_content,
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
        store.put_thema(&prepared, 20260715, 1).unwrap();
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

    #[test]
    fn review_attempt_commits_atomically_and_replays_by_attempt_id() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "Question", None);
        let session = store
            .create_review_session(&manual_source("thema"), Some(1), 1_752_600_000_000_000)
            .unwrap();
        let attempt = store
            .begin_review_attempt(&session.session_id, 1_752_600_000_000_001)
            .unwrap();
        let graded = store
            .grade_review_attempt(
                &attempt.attempt_id,
                &AnswerInput::Text("ANSWER".into()),
                1_752_600_000_001_000,
                0.0,
            )
            .unwrap();
        assert_eq!(graded.detected_success, Some(true));
        let event = store
            .commit_review_attempt(&attempt.attempt_id, 1_752_600_000_002_000)
            .unwrap();
        assert_eq!(
            store
                .commit_review_attempt(&attempt.attempt_id, 1_752_600_000_003_000)
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
            .create_review_session(&manual_source("thema"), Some(1), 1_752_600_000_000_000)
            .unwrap();
        let attempt = store
            .begin_review_attempt(&session.session_id, 1_752_600_000_000_001)
            .unwrap();
        store
            .grade_review_attempt(
                &attempt.attempt_id,
                &AnswerInput::Text("answer".into()),
                1_752_600_000_001_000,
                0.0,
            )
            .unwrap();
        store
            .stop_review_session(&session.session_id, true, 1_752_600_000_002_000)
            .unwrap();

        assert!(matches!(
            store.commit_review_attempt(&attempt.attempt_id, 1_752_600_000_003_000),
            Err(crate::store::StoreError::InvalidReviewState)
        ));
    }

    #[test]
    fn changed_content_rejects_a_pending_attempt() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        add_thema(&mut store, "thema", "First", None);
        let session = store
            .create_review_session(&manual_source("thema"), None, 1_752_600_000_000_000)
            .unwrap();
        let attempt = store
            .begin_review_attempt(&session.session_id, 1_752_600_000_000_001)
            .unwrap();
        add_thema(&mut store, "thema", "Changed", Some(1));

        let error = store
            .grade_review_attempt(
                &attempt.attempt_id,
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
            .create_review_session(&manual_source("tagged"), None, 1_752_600_000_000_000)
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
            .create_review_session(&manual_source("thema"), None, 1_752_600_000_000_000)
            .unwrap();
        let attempt = store
            .begin_review_attempt(&session.session_id, 1_752_600_000_000_001)
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
            .create_review_session(&manual_source("thema"), None, 1_752_600_000_000_000)
            .unwrap();
        let attempt = store
            .begin_review_attempt(&session.session_id, 1_752_600_000_000_001)
            .unwrap();
        store
            .grade_review_attempt(
                &attempt.attempt_id,
                &AnswerInput::Text("answer".into()),
                1_752_600_000_001_000,
                0.0,
            )
            .unwrap();
        store
            .toggle_attempt_suspension(&attempt.attempt_id)
            .unwrap();
        let event = store
            .commit_review_attempt(&attempt.attempt_id, 1_752_600_000_002_000)
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
