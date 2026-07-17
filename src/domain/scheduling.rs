use chrono::NaiveDate;
use thiserror::Error;

use super::fsrs::{FsrsConfigSnapshot, FsrsMemoryState, RecallOutcome, compute_fsrs_transition};

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReviewState {
    pub(crate) state_revision: u64,
    pub(crate) review_count: u64,
    pub(crate) last_reviewed_at_us: Option<u64>,
    pub(crate) last_review_day: Option<u32>,
    pub(crate) next_review_day: u32,
    pub(crate) interval_days: Option<u32>,
    pub(crate) total_successes: u64,
    pub(crate) total_failures: u64,
    pub(crate) suspended: bool,
    pub(crate) stability: Option<f32>,
    pub(crate) difficulty: Option<f32>,
}

impl Default for ReviewState {
    fn default() -> Self {
        Self {
            state_revision: 0,
            review_count: 0,
            last_reviewed_at_us: None,
            last_review_day: None,
            next_review_day: 19700101,
            interval_days: None,
            total_successes: 0,
            total_failures: 0,
            suspended: false,
            stability: None,
            difficulty: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Transition {
    pub(crate) before: ReviewState,
    pub(crate) after: ReviewState,
    pub(crate) config: FsrsConfigSnapshot,
    pub(crate) reviewed_at_us: u64,
    pub(crate) review_day: u32,
    pub(crate) outcome: RecallOutcome,
    pub(crate) first_review: bool,
    pub(crate) elapsed_days: u32,
    pub(crate) raw_interval_days: f32,
    pub(crate) interval_days: u32,
    pub(crate) due_day: u32,
}

pub(crate) fn compute_transition(
    state: &ReviewState,
    config: &FsrsConfigSnapshot,
    success: bool,
    review_day: u32,
    reviewed_at_us: u64,
) -> Result<Transition, SchedulerError> {
    validate_state(state)?;
    let elapsed_days = match state.last_review_day {
        Some(last_day) => date_difference(parse_day(last_day)?, parse_day(review_day)?)?,
        None => 0,
    };
    let prior_state = match (state.stability, state.difficulty) {
        (Some(stability), Some(difficulty)) => Some(FsrsMemoryState {
            stability,
            difficulty,
        }),
        (None, None) => None,
        _ => return Err(SchedulerError::State),
    };
    let outcome = if success {
        RecallOutcome::Success
    } else {
        RecallOutcome::Failure
    };
    let scheduled = compute_fsrs_transition(
        prior_state,
        elapsed_days,
        review_day,
        reviewed_at_us,
        outcome,
        config,
    )
    .map_err(|_| SchedulerError::Schedule)?;
    let after = ReviewState {
        state_revision: checked_next(state.state_revision)?,
        review_count: checked_next(state.review_count)?,
        last_reviewed_at_us: Some(reviewed_at_us),
        last_review_day: Some(review_day),
        next_review_day: scheduled.due_day,
        interval_days: Some(scheduled.calendar_interval_days),
        total_successes: state.total_successes + u64::from(success),
        total_failures: state.total_failures + u64::from(!success),
        suspended: state.suspended,
        stability: Some(scheduled.state.stability),
        difficulty: Some(scheduled.state.difficulty),
    };
    validate_state(&after)?;
    Ok(Transition {
        before: state.clone(),
        after,
        config: config.clone(),
        reviewed_at_us,
        review_day,
        outcome,
        first_review: state.review_count == 0,
        elapsed_days,
        raw_interval_days: scheduled.raw_interval_days,
        interval_days: scheduled.calendar_interval_days,
        due_day: scheduled.due_day,
    })
}

pub(crate) fn validate_state(state: &ReviewState) -> Result<(), SchedulerError> {
    let counts_match = state.review_count == state.total_successes + state.total_failures;
    let dates_match = parse_day(state.next_review_day).is_ok()
        && if state.review_count == 0 {
            state.last_reviewed_at_us.is_none()
                && state.last_review_day.is_none()
                && state.interval_days.is_none()
                && state.stability.is_none()
                && state.difficulty.is_none()
        } else {
            state.last_reviewed_at_us.is_some_and(|value| value > 0)
                && state
                    .last_review_day
                    .is_some_and(|day| parse_day(day).is_ok())
                && state.interval_days.is_some_and(|days| days >= 1)
                && state
                    .stability
                    .is_some_and(|value| value.is_finite() && value > 0.0)
                && state
                    .difficulty
                    .is_some_and(|value| value.is_finite() && (1.0..=10.0).contains(&value))
        };
    if counts_match && dates_match {
        Ok(())
    } else {
        Err(SchedulerError::State)
    }
}

fn parse_day(day: u32) -> Result<NaiveDate, SchedulerError> {
    let year = i32::try_from(day / 10_000).map_err(|_| SchedulerError::Date)?;
    NaiveDate::from_ymd_opt(year, (day / 100) % 100, day % 100).ok_or(SchedulerError::Date)
}

fn date_difference(from: NaiveDate, to: NaiveDate) -> Result<u32, SchedulerError> {
    u32::try_from((to - from).num_days()).map_err(|_| SchedulerError::Date)
}

fn checked_next(value: u64) -> Result<u64, SchedulerError> {
    value.checked_add(1).ok_or(SchedulerError::Count)
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum SchedulerError {
    #[error("scheduling state is invalid")]
    State,
    #[error("FSRS scheduling failed")]
    Schedule,
    #[error("review day is invalid")]
    Date,
    #[error("review counter overflowed")]
    Count,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_state() -> ReviewState {
        ReviewState {
            state_revision: 0,
            review_count: 0,
            last_reviewed_at_us: None,
            last_review_day: None,
            next_review_day: 20260717,
            interval_days: None,
            total_successes: 0,
            total_failures: 0,
            suspended: false,
            stability: None,
            difficulty: None,
        }
    }

    #[test]
    fn event_sequence_replays_to_identical_projection() {
        let config = FsrsConfigSnapshot::default();
        let events = [
            (true, 20260717, 1),
            (false, 20260720, 2),
            (true, 20260720, 3),
        ];
        let replay = || {
            events
                .into_iter()
                .fold(new_state(), |state, (success, day, at)| {
                    compute_transition(&state, &config, success, day, at)
                        .unwrap()
                        .after
                })
        };
        assert_eq!(replay(), replay());
        let state = replay();
        assert_eq!(state.review_count, 3);
        assert_eq!(state.total_successes, 2);
        assert_eq!(state.total_failures, 1);
        assert_eq!(state.last_review_day, Some(20260720));
    }
}
