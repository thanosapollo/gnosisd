use chrono::{Datelike, Duration, NaiveDate};
use rusqlite::{OptionalExtension, params};

use super::{SqliteStore, StoreError};
use crate::application::{
    ContentCounts, DashboardSummary, ReviewHistoryEvent, ReviewHistoryQuery, ReviewQueueCounts,
    ReviewQueueSummary, ReviewSessionMode, ReviewSessionQuery, ReviewSessionSummary,
    ReviewSessionView, ReviewSource, ReviewStateSnapshot, ReviewTotals, ReviewWindow,
    ThemaReviewState, TransitionSnapshot, review_day_at,
};

impl SqliteStore {
    pub(crate) fn dashboard(&self, days: u16, now_us: u64) -> Result<DashboardSummary, StoreError> {
        if !(1..=365).contains(&days) {
            return Err(StoreError::InvalidProjection {
                reason: "dashboard window",
            });
        }
        let settings = self.settings()?;
        let review_day = review_day_at(now_us, &settings.timezone, settings.day_boundary_hour)
            .map_err(StoreError::storage)?;
        let from_day = review_window_start(review_day, days)?;
        Ok(DashboardSummary {
            generated_at_us: now_us,
            review_day,
            today: review_totals(&self.connection, review_day, review_day)?.1,
            window: review_window(&self.connection, from_day, review_day, days)?,
            queue: queue_counts(&self.connection, review_day)?,
            content: content_counts(&self.connection)?,
            active_session: self.active_review_session(None)?,
        })
    }

    pub(crate) fn review_sessions(
        &self,
        query: &ReviewSessionQuery,
    ) -> Result<Vec<ReviewSessionSummary>, StoreError> {
        let mut statement = self
            .connection
            .prepare_cached(SESSION_PAGE_SQL)
            .map_err(StoreError::storage)?;
        let rows = statement
            .query_map(
                params![
                    query.after.as_deref(),
                    query.status.as_deref(),
                    query.mode.as_deref(),
                    i64::from(query.limit),
                ],
                raw_session_summary,
            )
            .map_err(StoreError::storage)?;
        rows.map(|row| {
            row.map_err(StoreError::storage)
                .and_then(materialize_session_summary)
        })
        .collect()
    }

    pub(crate) fn review_session_summary(
        &self,
        session_id: &str,
    ) -> Result<Option<ReviewSessionSummary>, StoreError> {
        self.connection
            .query_row(SESSION_DETAIL_SQL, [session_id], raw_session_summary)
            .optional()
            .map_err(StoreError::storage)?
            .map(materialize_session_summary)
            .transpose()
    }

    pub(crate) fn review_history(
        &self,
        query: &ReviewHistoryQuery,
    ) -> Result<Vec<ReviewHistoryEvent>, StoreError> {
        let accepted = query.accepted_success.map(i64::from);
        let mut statement = self
            .connection
            .prepare_cached(REVIEW_HISTORY_SQL)
            .map_err(StoreError::storage)?;
        let rows = statement
            .query_map(
                params![
                    query.after.as_deref(),
                    query.thema_id.as_deref(),
                    query.session_id.as_deref(),
                    query.from_day,
                    query.through_day,
                    accepted,
                    i64::from(query.limit),
                ],
                raw_review_event,
            )
            .map_err(StoreError::storage)?;
        rows.map(|row| {
            row.map_err(StoreError::storage)
                .and_then(materialize_review_event)
        })
        .collect()
    }

    pub(crate) fn review_queue_summary(
        &self,
        source: &ReviewSource,
        now_us: u64,
    ) -> Result<ReviewQueueSummary, StoreError> {
        let settings = self.settings()?;
        let review_day = review_day_at(now_us, &settings.timezone, settings.day_boundary_hour)
            .map_err(StoreError::storage)?;
        let matched_count =
            super::review::select_queue(&self.connection, source, review_day)?.len();
        Ok(ReviewQueueSummary {
            source: source.clone(),
            matched_count: u64::try_from(matched_count).map_err(StoreError::storage)?,
        })
    }

    pub(crate) fn thema_review_state(
        &self,
        thema_id: &str,
    ) -> Result<ThemaReviewState, StoreError> {
        let state = super::review::load_review_state(&self.connection, thema_id)?;
        Ok(ThemaReviewState {
            thema_id: thema_id.to_owned(),
            state: ReviewStateSnapshot::from(&state),
        })
    }
}

fn review_window_start(review_day: u32, days: u16) -> Result<u32, StoreError> {
    let year = i32::try_from(review_day / 10_000).map_err(StoreError::storage)?;
    let month = review_day / 100 % 100;
    let day = review_day % 100;
    let date = NaiveDate::from_ymd_opt(year, month, day).ok_or(StoreError::CorruptData {
        field: "review day",
    })?;
    let start = date
        .checked_sub_signed(Duration::days(i64::from(days.saturating_sub(1))))
        .ok_or(StoreError::CorruptData {
            field: "review day window",
        })?;
    let year = u32::try_from(start.year()).map_err(StoreError::storage)?;
    year.checked_mul(10_000)
        .and_then(|value| value.checked_add(start.month() * 100))
        .and_then(|value| value.checked_add(start.day()))
        .ok_or(StoreError::CorruptData {
            field: "review day window",
        })
}

fn review_window(
    connection: &rusqlite::Connection,
    from_day: u32,
    through_day: u32,
    days: u16,
) -> Result<ReviewWindow, StoreError> {
    let (active_days, totals) = review_totals(connection, from_day, through_day)?;
    Ok(ReviewWindow {
        from_day,
        through_day,
        days,
        active_days,
        totals,
    })
}

fn review_totals(
    connection: &rusqlite::Connection,
    from_day: u32,
    through_day: u32,
) -> Result<(u64, ReviewTotals), StoreError> {
    connection
        .query_row(
            "SELECT count(*), coalesce(sum(review_count), 0), coalesce(sum(new_count), 0),
                    coalesce(sum(success_count), 0), coalesce(sum(failure_count), 0),
                    coalesce(sum(override_count), 0)
             FROM review_day WHERE review_day BETWEEN ?1 AND ?2",
            params![from_day, through_day],
            |row| {
                Ok((
                    super::row_u64(row, 0)?,
                    ReviewTotals {
                        review_count: super::row_u64(row, 1)?,
                        new_count: super::row_u64(row, 2)?,
                        success_count: super::row_u64(row, 3)?,
                        failure_count: super::row_u64(row, 4)?,
                        override_count: super::row_u64(row, 5)?,
                    },
                ))
            },
        )
        .map_err(StoreError::storage)
}

fn queue_counts(
    connection: &rusqlite::Connection,
    review_day: u32,
) -> Result<ReviewQueueCounts, StoreError> {
    connection
        .query_row(
            "SELECT
               coalesce(sum(state.review_count = 0 AND NOT state.suspended), 0),
               coalesce(sum(state.review_count > 0 AND state.next_review_day = ?1
                            AND NOT state.suspended), 0),
               coalesce(sum(state.review_count > 0 AND state.next_review_day < ?1
                            AND NOT state.suspended), 0),
               coalesce(sum(state.suspended), 0)
             FROM thema JOIN review_state AS state USING(thema_id)
             WHERE thema.archived_at_us IS NULL",
            [review_day],
            |row| {
                Ok(ReviewQueueCounts {
                    new_count: super::row_u64(row, 0)?,
                    due_count: super::row_u64(row, 1)?,
                    overdue_count: super::row_u64(row, 2)?,
                    suspended_count: super::row_u64(row, 3)?,
                })
            },
        )
        .map_err(StoreError::storage)
}

fn content_counts(connection: &rusqlite::Connection) -> Result<ContentCounts, StoreError> {
    connection
        .query_row(
            "SELECT
               (SELECT count(*) FROM thema WHERE archived_at_us IS NULL),
               (SELECT count(*) FROM thema WHERE archived_at_us IS NOT NULL),
               (SELECT count(*) FROM document WHERE active),
               (SELECT count(*) FROM node WHERE active)",
            [],
            |row| {
                Ok(ContentCounts {
                    active_themata: super::row_u64(row, 0)?,
                    archived_themata: super::row_u64(row, 1)?,
                    active_documents: super::row_u64(row, 2)?,
                    active_nodes: super::row_u64(row, 3)?,
                })
            },
        )
        .map_err(StoreError::storage)
}

struct RawSessionSummary {
    session_id: String,
    owner_device_id: Option<String>,
    mode: String,
    source_json: String,
    target_count: Option<i64>,
    status: String,
    current_position: i64,
    queue_length: i64,
    started_at_us: i64,
    ended_at_us: Option<i64>,
    review_count: i64,
    new_count: i64,
    success_count: i64,
    failure_count: i64,
    override_count: i64,
}

fn raw_session_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSessionSummary> {
    Ok(RawSessionSummary {
        session_id: row.get(0)?,
        owner_device_id: row.get(1)?,
        mode: row.get(2)?,
        source_json: row.get(3)?,
        target_count: row.get(4)?,
        status: row.get(5)?,
        current_position: row.get(6)?,
        queue_length: row.get(7)?,
        started_at_us: row.get(8)?,
        ended_at_us: row.get(9)?,
        review_count: row.get(10)?,
        new_count: row.get(11)?,
        success_count: row.get(12)?,
        failure_count: row.get(13)?,
        override_count: row.get(14)?,
    })
}

fn materialize_session_summary(row: RawSessionSummary) -> Result<ReviewSessionSummary, StoreError> {
    Ok(ReviewSessionSummary {
        session: ReviewSessionView {
            session_id: row.session_id,
            owner_device_id: row.owner_device_id,
            mode: parse_session_mode(&row.mode)?,
            source: serde_json::from_str(&row.source_json).map_err(StoreError::storage)?,
            target_count: optional_u32(row.target_count, "target count")?,
            status: row.status,
            current_position: to_u32(row.current_position, "session position")?,
            queue_length: to_u32(row.queue_length, "queue length")?,
            started_at_us: to_u64(row.started_at_us, "session start")?,
        },
        ended_at_us: row
            .ended_at_us
            .map(|value| to_u64(value, "session end"))
            .transpose()?,
        review_count: to_u64(row.review_count, "session review count")?,
        new_count: to_u64(row.new_count, "session new count")?,
        success_count: to_u64(row.success_count, "session success count")?,
        failure_count: to_u64(row.failure_count, "session failure count")?,
        override_count: to_u64(row.override_count, "session override count")?,
    })
}

struct RawReviewEvent {
    event_id: String,
    thema_id: String,
    session_id: String,
    reviewed_at_us: i64,
    review_day: i64,
    detected_success: bool,
    accepted_success: bool,
    override_count: i64,
    transition_json: Option<String>,
}

fn raw_review_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawReviewEvent> {
    Ok(RawReviewEvent {
        event_id: row.get(0)?,
        thema_id: row.get(1)?,
        session_id: row.get(2)?,
        reviewed_at_us: row.get(3)?,
        review_day: row.get(4)?,
        detected_success: row.get(5)?,
        accepted_success: row.get(6)?,
        override_count: row.get(7)?,
        transition_json: row.get(8)?,
    })
}

fn materialize_review_event(row: RawReviewEvent) -> Result<ReviewHistoryEvent, StoreError> {
    Ok(ReviewHistoryEvent {
        event_id: row.event_id,
        thema_id: row.thema_id,
        session_id: row.session_id,
        reviewed_at_us: to_u64(row.reviewed_at_us, "review timestamp")?,
        review_day: to_u32(row.review_day, "review day")?,
        detected_success: row.detected_success,
        accepted_success: row.accepted_success,
        override_count: to_u64(row.override_count, "override count")?,
        transition: row
            .transition_json
            .map(|value| serde_json::from_str::<TransitionSnapshot>(&value))
            .transpose()
            .map_err(StoreError::storage)?,
    })
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

fn optional_u32(value: Option<i64>, field: &'static str) -> Result<Option<u32>, StoreError> {
    value.map(|value| to_u32(value, field)).transpose()
}

fn to_u32(value: i64, field: &'static str) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::CorruptData { field })
}

fn to_u64(value: i64, field: &'static str) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::CorruptData { field })
}

const SESSION_PAGE_SQL: &str = "
    SELECT session.session_id, session.owner_device_id, session.review_mode,
           session.source_json, session.target_count, session.status,
           session.current_position,
           (SELECT count(*) FROM review_session_item AS item
            WHERE item.session_id = session.session_id),
           session.started_at_us, session.ended_at_us,
           count(event.event_id),
           coalesce(sum(json_extract(event.transition_json, '$.first_review')), 0),
           coalesce(sum(event.accepted_success), 0),
           coalesce(sum(NOT event.accepted_success), 0),
           coalesce(sum(event.override_count), 0)
    FROM review_session AS session
    LEFT JOIN review_event AS event USING(session_id)
    WHERE (
      ?1 IS NULL OR session.started_at_us < (
        SELECT cursor.started_at_us FROM review_session AS cursor
        WHERE cursor.session_id = ?1
      ) OR (
        session.started_at_us = (
          SELECT cursor.started_at_us FROM review_session AS cursor
          WHERE cursor.session_id = ?1
        ) AND session.session_id < ?1
      )
    )
    AND (?2 IS NULL OR session.status = ?2)
    AND (?3 IS NULL OR session.review_mode = ?3)
    GROUP BY session.session_id
    ORDER BY session.started_at_us DESC, session.session_id DESC
    LIMIT ?4";

const SESSION_DETAIL_SQL: &str = "
    SELECT session.session_id, session.owner_device_id, session.review_mode,
           session.source_json, session.target_count, session.status,
           session.current_position,
           (SELECT count(*) FROM review_session_item AS item
            WHERE item.session_id = session.session_id),
           session.started_at_us, session.ended_at_us,
           count(event.event_id),
           coalesce(sum(json_extract(event.transition_json, '$.first_review')), 0),
           coalesce(sum(event.accepted_success), 0),
           coalesce(sum(NOT event.accepted_success), 0),
           coalesce(sum(event.override_count), 0)
    FROM review_session AS session
    LEFT JOIN review_event AS event USING(session_id)
    WHERE session.session_id = ?1
    GROUP BY session.session_id";

const REVIEW_HISTORY_SQL: &str = "
    SELECT event_id, thema_id, session_id, reviewed_at_us, review_day,
           detected_success, accepted_success, override_count,
           CASE WHEN imported_payload_json IS NULL THEN transition_json END
    FROM review_event AS event
    WHERE (
      ?1 IS NULL OR event.reviewed_at_us < (
        SELECT cursor.reviewed_at_us FROM review_event AS cursor WHERE cursor.event_id = ?1
      ) OR (
        event.reviewed_at_us = (
          SELECT cursor.reviewed_at_us FROM review_event AS cursor WHERE cursor.event_id = ?1
        ) AND event.event_id < ?1
      )
    )
    AND (?2 IS NULL OR event.thema_id = ?2)
    AND (?3 IS NULL OR event.session_id = ?3)
    AND (?4 IS NULL OR event.review_day >= ?4)
    AND (?5 IS NULL OR event.review_day <= ?5)
    AND (?6 IS NULL OR event.accepted_success = ?6)
    ORDER BY event.reviewed_at_us DESC, event.event_id DESC
    LIMIT ?7";

#[cfg(test)]
mod tests {
    use super::SqliteStore;
    use crate::application::{
        AnswerInput, ReviewHistoryQuery, ReviewSessionQuery, ReviewSource, ReviewSourceKind,
        prepare_thema, thema_content,
    };

    const REVIEWED_AT_US: u64 = 1_784_116_800_000_000;

    #[test]
    fn dashboard_queue_and_thema_state_are_server_projections() {
        let store = store_with_thema();

        let dashboard = store.dashboard(30, REVIEWED_AT_US).unwrap();
        let queue = store
            .review_queue_summary(&manual_source(), REVIEWED_AT_US)
            .unwrap();
        let state = store.thema_review_state("thema").unwrap();

        assert_eq!(dashboard.review_day, 20260715);
        assert_eq!(dashboard.queue.new_count, 1);
        assert_eq!(dashboard.content.active_themata, 1);
        assert_eq!(dashboard.today.review_count, 0);
        assert_eq!(queue.matched_count, 1);
        assert_eq!(state.state.review_count, 0);
    }

    #[test]
    fn session_and_review_history_are_summarized_and_filtered() {
        let mut store = store_with_thema();
        let session = store
            .create_review_session(&manual_source(), Some(1), None, REVIEWED_AT_US)
            .unwrap();
        let attempt = store
            .begin_http_review_attempt(&session.session_id, None, REVIEWED_AT_US + 1)
            .unwrap();
        store
            .grade_review_attempt(
                &attempt.attempt_id,
                None,
                &AnswerInput::Text("answer".into()),
                REVIEWED_AT_US + 2,
                0.0,
            )
            .unwrap();
        store
            .commit_review_attempt(&attempt.attempt_id, None, REVIEWED_AT_US + 3)
            .unwrap();

        let sessions = store.review_sessions(&session_query()).unwrap();
        let history = store.review_history(&history_query()).unwrap();
        let dashboard = store.dashboard(30, REVIEWED_AT_US + 4).unwrap();

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session.status, "completed");
        assert_eq!(sessions[0].review_count, 1);
        assert_eq!(sessions[0].new_count, 1);
        assert_eq!(sessions[0].success_count, 1);
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].session_id, session.session_id);
        assert!(history[0].accepted_success);
        assert!(history[0].transition.is_some());
        assert_eq!(dashboard.today.review_count, 1);
        assert_eq!(dashboard.today.success_count, 1);

        store
            .connection
            .execute(
                "UPDATE review_event SET transition_json = '{}', imported_payload_json = '{}'",
                [],
            )
            .unwrap();
        assert!(
            store.review_history(&history_query()).unwrap()[0]
                .transition
                .is_none()
        );
    }

    #[test]
    fn online_sessions_resume_without_device_ownership_transfer() {
        let mut store = store_with_thema();
        let device =
            crate::application::generate_device("device-a".into(), vec!["review".into()]).unwrap();
        let device_id = device.device_id.clone();
        store.create_device(&device, 1).unwrap();
        let session = store
            .create_review_session(&manual_source(), Some(1), Some(&device_id), REVIEWED_AT_US)
            .unwrap();
        store
            .stop_review_session(&session.session_id, None, false, REVIEWED_AT_US + 1)
            .unwrap();

        let resumed = store
            .resume_review_session(&session.session_id, None)
            .unwrap();

        assert_eq!(resumed.status, "active");
        assert_eq!(resumed.owner_device_id.as_deref(), Some(device_id.as_str()));
        assert_eq!(
            store
                .active_review_session(None)
                .unwrap()
                .unwrap()
                .session_id,
            session.session_id
        );
    }

    fn store_with_thema() -> SqliteStore {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let content = thema_content(
            "basic",
            "Question".into(),
            vec![None],
            vec!["answer".into()],
            None,
            None,
            None,
        )
        .unwrap();
        let thema =
            prepare_thema(Some("thema".into()), content, vec![], vec![], vec![], None).unwrap();
        store.put_thema(&thema, 20260715, 1, None).unwrap();
        store
    }

    fn manual_source() -> ReviewSource {
        ReviewSource {
            kind: ReviewSourceKind::Manual,
            tags: vec![],
            exclude_tags: vec![],
            topic_node_id: None,
            topic_forward_depth: None,
            topic_backward_depth: None,
            manual_ids: vec!["thema".into()],
        }
    }

    fn session_query() -> ReviewSessionQuery {
        ReviewSessionQuery {
            after: None,
            limit: 10,
            status: Some("completed".into()),
            mode: Some("online".into()),
        }
    }

    fn history_query() -> ReviewHistoryQuery {
        ReviewHistoryQuery {
            after: None,
            limit: 10,
            thema_id: Some("thema".into()),
            session_id: None,
            from_day: Some(20260715),
            through_day: Some(20260715),
            accepted_success: Some(true),
        }
    }
}
