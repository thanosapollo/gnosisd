use chrono::{Datelike, Days, NaiveDate};
use thiserror::Error;

pub(crate) const SCHEDULER_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ReviewState {
    pub(crate) state_revision: u64,
    pub(crate) review_count: u64,
    pub(crate) legacy_review_count: u64,
    pub(crate) legacy_unclassified_count: u64,
    pub(crate) last_reviewed_at_us: Option<u64>,
    pub(crate) last_review_day: Option<u32>,
    pub(crate) next_review_day: u32,
    pub(crate) interval_days: Option<u32>,
    pub(crate) consecutive_successes: u64,
    pub(crate) total_successes: u64,
    pub(crate) consecutive_failures: u64,
    pub(crate) total_failures: u64,
    pub(crate) suspended: bool,
    pub(crate) gnosis_plus: f64,
    pub(crate) gnosis_minus: f64,
    pub(crate) gnosis_synolon: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SchedulerParameters {
    pub(crate) scheduler_version: u32,
    pub(crate) amnesia: f64,
    pub(crate) epignosis: f64,
    pub(crate) agnoia: f64,
    pub(crate) anagnosis: u64,
    pub(crate) lethe: u64,
    pub(crate) synolon_max: f64,
    pub(crate) interval_fuzz: f64,
    pub(crate) proto: Vec<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Transition {
    pub(crate) before: ReviewState,
    pub(crate) after: ReviewState,
    pub(crate) parameters: SchedulerParameters,
    pub(crate) reviewed_at_us: u64,
    pub(crate) review_day: u32,
    pub(crate) success: bool,
    pub(crate) first_review: bool,
    pub(crate) elapsed_days: u32,
    pub(crate) algorithm_interval_days: f64,
    pub(crate) computed_interval_days: u32,
    pub(crate) fuzz_delta: f64,
}

pub(crate) fn compute_transition(
    state: &ReviewState,
    parameters: &SchedulerParameters,
    success: bool,
    review_day: u32,
    reviewed_at_us: u64,
    fuzz_delta: f64,
) -> Result<Transition, SchedulerError> {
    validate_state(state)?;
    validate_parameters(parameters)?;
    if reviewed_at_us == 0 {
        return Err(SchedulerError::Input);
    }
    let review_date = parse_day(review_day)?;
    let counters = transition_counters(state, success)?;
    let score = transition_score(state, parameters, success, counters.0, counters.2);
    let intervals = transition_interval(state, parameters, success, review_date, fuzz_delta)?;
    let after = transition_state(
        state,
        reviewed_at_us,
        review_day,
        &counters,
        score,
        &intervals,
    )?;
    Ok(Transition {
        before: state.clone(),
        after,
        parameters: parameters.clone(),
        reviewed_at_us,
        review_day,
        success,
        first_review: state.review_count == 0,
        elapsed_days: intervals.elapsed,
        algorithm_interval_days: intervals.algorithm,
        computed_interval_days: intervals.computed,
        fuzz_delta: intervals.delta,
    })
}

pub(crate) fn fuzz_delta(
    interval: f64,
    fuzz_factor: f64,
    sample: f64,
) -> Result<f64, SchedulerError> {
    if !interval.is_finite()
        || interval < 0.0
        || !fuzz_factor.is_finite()
        || !(0.0..1.0).contains(&fuzz_factor)
        || !sample.is_finite()
        || !(-1.0..=1.0).contains(&sample)
    {
        return Err(SchedulerError::Input);
    }
    Ok(if interval <= 2.0 || fuzz_factor == 0.0 {
        0.0
    } else {
        fuzz_factor * sample
    })
}

fn validate_state(state: &ReviewState) -> Result<(), SchedulerError> {
    let counts_match = state.review_count
        == state.total_successes + state.total_failures + state.legacy_unclassified_count;
    let legacy_match = state.legacy_unclassified_count <= state.legacy_review_count
        && state.legacy_review_count <= state.review_count;
    let streaks_match = state.consecutive_successes <= state.total_successes
        && state.consecutive_failures <= state.total_failures
        && (state.consecutive_successes == 0 || state.consecutive_failures == 0);
    let dates_match = parse_day(state.next_review_day).is_ok()
        && if state.review_count == 0 {
            state.last_reviewed_at_us.is_none()
                && state.last_review_day.is_none()
                && state.interval_days.is_none()
        } else {
            state.last_reviewed_at_us.is_some_and(|value| value > 0)
                && state
                    .last_review_day
                    .is_some_and(|day| parse_day(day).is_ok())
        };
    let scores_match = [state.gnosis_plus, state.gnosis_minus, state.gnosis_synolon]
        .into_iter()
        .all(f64::is_finite)
        && state.gnosis_plus >= 0.0
        && state.gnosis_minus >= 0.0
        && state.gnosis_synolon >= 1.3;
    if counts_match && legacy_match && streaks_match && dates_match && scores_match {
        Ok(())
    } else {
        Err(SchedulerError::State)
    }
}

pub(crate) fn validate_parameters(parameters: &SchedulerParameters) -> Result<(), SchedulerError> {
    let fractional = [
        parameters.epignosis,
        parameters.agnoia,
        parameters.interval_fuzz,
    ]
    .into_iter()
    .all(|value| value.is_finite() && (0.0..1.0).contains(&value));
    if parameters.scheduler_version == SCHEDULER_VERSION
        && parameters.amnesia.is_finite()
        && parameters.amnesia > 0.0
        && parameters.amnesia <= 1.0
        && fractional
        && parameters.anagnosis > 0
        && parameters.lethe > 0
        && parameters.synolon_max.is_finite()
        && parameters.synolon_max >= 1.3
    {
        Ok(())
    } else {
        Err(SchedulerError::Parameters)
    }
}

fn transition_counters(
    state: &ReviewState,
    success: bool,
) -> Result<(u64, u64, u64, u64), SchedulerError> {
    if success {
        Ok((
            checked_next(state.consecutive_successes)?,
            checked_next(state.total_successes)?,
            0,
            state.total_failures,
        ))
    } else {
        Ok((
            0,
            state.total_successes,
            checked_next(state.consecutive_failures)?,
            checked_next(state.total_failures)?,
        ))
    }
}

fn transition_score(
    state: &ReviewState,
    parameters: &SchedulerParameters,
    success: bool,
    consecutive_successes: u64,
    consecutive_failures: u64,
) -> (f64, f64, f64) {
    let streak = if success {
        consecutive_successes
    } else {
        consecutive_failures
    };
    let event = streak.max(1) % parameters.anagnosis == 0;
    let synolon = if success {
        (state.gnosis_synolon + state.gnosis_plus).min(parameters.synolon_max)
    } else {
        (state.gnosis_synolon - state.gnosis_minus).max(1.3)
    };
    let mut plus = state.gnosis_plus
        + if success && event {
            parameters.epignosis
        } else {
            0.0
        };
    let minus = state.gnosis_minus
        + if !success && event {
            parameters.agnoia
        } else {
            0.0
        };
    if !success && consecutive_failures >= parameters.lethe {
        plus = (plus - parameters.epignosis).max(0.1);
    }
    (round_two(plus), round_two(minus), round_two(synolon))
}

struct IntervalFields {
    elapsed: u32,
    algorithm: f64,
    computed: u32,
    next_day: u32,
    delta: f64,
}

fn transition_interval(
    state: &ReviewState,
    parameters: &SchedulerParameters,
    success: bool,
    review_date: NaiveDate,
    delta: f64,
) -> Result<IntervalFields, SchedulerError> {
    let elapsed = state
        .last_review_day
        .map(parse_day)
        .transpose()?
        .map_or(Ok(0), |last| date_difference(last, review_date))?;
    let algorithm = next_interval(state, parameters, success, elapsed);
    validate_delta(algorithm, parameters.interval_fuzz, delta)?;
    let candidate = (algorithm * (1.0 + delta)).round_ties_even();
    let candidate = u32::try_from(candidate as i64).map_err(|_| SchedulerError::Interval)?;
    let candidate_date = review_date
        .checked_add_days(Days::new(u64::from(candidate)))
        .ok_or(SchedulerError::Interval)?;
    let current_next = parse_day(state.next_review_day)?;
    let next = if success && current_next > candidate_date {
        current_next
    } else {
        candidate_date
    };
    Ok(IntervalFields {
        elapsed,
        algorithm,
        computed: date_difference(review_date, next)?,
        next_day: format_day(next),
        delta,
    })
}

fn next_interval(
    state: &ReviewState,
    parameters: &SchedulerParameters,
    success: bool,
    elapsed: u32,
) -> f64 {
    let elapsed = if elapsed == 0 && success {
        1.0
    } else {
        f64::from(elapsed)
    };
    let successes = usize::try_from(state.total_successes).unwrap_or(usize::MAX);
    if successes < parameters.proto.len() {
        return if success {
            f64::from(parameters.proto[successes])
        } else {
            0.0
        };
    }
    if !success && state.consecutive_failures >= parameters.lethe {
        return 0.0;
    }
    if success {
        state.gnosis_synolon * elapsed
    } else {
        (state.gnosis_synolon * elapsed)
            .min((1.0 - parameters.amnesia) * elapsed)
            .clamp(0.0, 7.0)
    }
}

fn transition_state(
    state: &ReviewState,
    reviewed_at_us: u64,
    review_day: u32,
    counters: &(u64, u64, u64, u64),
    score: (f64, f64, f64),
    interval: &IntervalFields,
) -> Result<ReviewState, SchedulerError> {
    Ok(ReviewState {
        state_revision: checked_next(state.state_revision)?,
        review_count: checked_next(state.review_count)?,
        legacy_review_count: state.legacy_review_count,
        legacy_unclassified_count: state.legacy_unclassified_count,
        last_reviewed_at_us: Some(reviewed_at_us),
        last_review_day: Some(review_day),
        next_review_day: interval.next_day,
        interval_days: Some(interval.computed),
        consecutive_successes: counters.0,
        total_successes: counters.1,
        consecutive_failures: counters.2,
        total_failures: counters.3,
        suspended: state.suspended,
        gnosis_plus: score.0,
        gnosis_minus: score.1,
        gnosis_synolon: score.2,
    })
}

fn validate_delta(interval: f64, factor: f64, delta: f64) -> Result<(), SchedulerError> {
    if delta.is_finite()
        && (-factor..=factor).contains(&delta)
        && ((interval > 2.0 && factor > 0.0) || delta == 0.0)
    {
        Ok(())
    } else {
        Err(SchedulerError::Input)
    }
}

fn parse_day(day: u32) -> Result<NaiveDate, SchedulerError> {
    let year = i32::try_from(day / 10_000).map_err(|_| SchedulerError::Date)?;
    NaiveDate::from_ymd_opt(year, (day / 100) % 100, day % 100).ok_or(SchedulerError::Date)
}

fn format_day(date: NaiveDate) -> u32 {
    u32::try_from(date.year()).unwrap_or_default() * 10_000 + date.month() * 100 + date.day()
}

fn date_difference(from: NaiveDate, to: NaiveDate) -> Result<u32, SchedulerError> {
    u32::try_from((to - from).num_days()).map_err(|_| SchedulerError::Date)
}

fn checked_next(value: u64) -> Result<u64, SchedulerError> {
    value.checked_add(1).ok_or(SchedulerError::Count)
}

fn round_two(value: f64) -> f64 {
    (value * 100.0).round_ties_even() / 100.0
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum SchedulerError {
    #[error("scheduling state is invalid")]
    State,
    #[error("scheduler parameters are invalid")]
    Parameters,
    #[error("scheduling input is invalid")]
    Input,
    #[error("review day is invalid")]
    Date,
    #[error("review counter overflowed")]
    Count,
    #[error("review interval cannot be represented")]
    Interval,
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::{ReviewState, SchedulerParameters, compute_transition, fuzz_delta};

    fn parameters() -> SchedulerParameters {
        SchedulerParameters {
            scheduler_version: 1,
            amnesia: 0.5,
            epignosis: 0.1,
            agnoia: 0.2,
            anagnosis: 3,
            lethe: 2,
            synolon_max: 3.0,
            interval_fuzz: 0.1,
            proto: vec![0, 1, 2],
        }
    }

    fn new_state() -> ReviewState {
        ReviewState {
            state_revision: 0,
            review_count: 0,
            legacy_review_count: 0,
            legacy_unclassified_count: 0,
            last_reviewed_at_us: None,
            last_review_day: None,
            next_review_day: 20260715,
            interval_days: None,
            consecutive_successes: 0,
            total_successes: 0,
            consecutive_failures: 0,
            total_failures: 0,
            suspended: false,
            gnosis_plus: 0.35,
            gnosis_minus: 0.30,
            gnosis_synolon: 1.3,
        }
    }

    #[test]
    fn first_success_uses_proto_and_preserves_due_floor() {
        let transition = compute_transition(
            &new_state(),
            &parameters(),
            true,
            20260715,
            1_752_600_000_000_000,
            0.0,
        )
        .unwrap();
        assert_eq!(transition.algorithm_interval_days, 0.0);
        assert_eq!(transition.after.next_review_day, 20260715);
        assert_eq!(transition.after.gnosis_synolon, 1.65);
        assert_eq!(transition.after.total_successes, 1);
    }

    #[test]
    fn mature_failure_caps_and_replays_fuzz() {
        let mut state = new_state();
        state.state_revision = 8;
        state.review_count = 8;
        state.last_reviewed_at_us = Some(1);
        state.last_review_day = Some(20260705);
        state.interval_days = Some(10);
        state.total_successes = 8;
        state.consecutive_successes = 8;
        state.gnosis_synolon = 2.3;
        let delta = fuzz_delta(5.0, 0.1, -0.5).unwrap();
        let transition = compute_transition(
            &state,
            &parameters(),
            false,
            20260715,
            1_752_600_000_000_000,
            delta,
        )
        .unwrap();
        assert_eq!(transition.algorithm_interval_days, 5.0);
        assert_eq!(transition.computed_interval_days, 5);
        assert_eq!(transition.fuzz_delta, -0.05);
        assert_eq!(transition.after.gnosis_synolon, 2.0);
    }

    #[test]
    fn invalid_future_matcher_inputs_are_rejected() {
        assert!(fuzz_delta(3.0, 0.1, 1.1).is_err());
        let mut parameters = parameters();
        parameters.scheduler_version = 2;
        assert!(compute_transition(&new_state(), &parameters, true, 20260715, 1, 0.0,).is_err());
    }

    #[derive(Deserialize)]
    struct ParityFixture {
        scheduler: Vec<ExpectedTransition>,
    }

    #[derive(Deserialize)]
    struct ExpectedTransition {
        name: String,
        algorithm_interval_days: f64,
        computed_interval_days: u32,
        fuzz_delta: f64,
        next_review_day: u32,
        gnosis_plus: f64,
        gnosis_minus: f64,
        gnosis_synolon: f64,
        total_successes: u64,
        total_failures: u64,
    }

    #[test]
    fn scheduler_matches_live_elisp_parity_vectors() {
        let fixture: ParityFixture =
            serde_json::from_str(include_str!("../../tests/fixtures/review/parity-v1.json"))
                .unwrap();
        for expected in fixture.scheduler {
            let (state, success) = parity_case(&expected.name);
            let transition = compute_transition(
                &state,
                &parameters(),
                success,
                20260715,
                1_752_600_000_000_000,
                expected.fuzz_delta,
            )
            .unwrap();
            assert_eq!(
                transition.algorithm_interval_days, expected.algorithm_interval_days,
                "{}",
                expected.name
            );
            assert_eq!(
                transition.computed_interval_days, expected.computed_interval_days,
                "{}",
                expected.name
            );
            assert_eq!(transition.after.next_review_day, expected.next_review_day);
            assert_eq!(transition.after.gnosis_plus, expected.gnosis_plus);
            assert_eq!(transition.after.gnosis_minus, expected.gnosis_minus);
            assert_eq!(transition.after.gnosis_synolon, expected.gnosis_synolon);
            assert_eq!(transition.after.total_successes, expected.total_successes);
            assert_eq!(transition.after.total_failures, expected.total_failures);
        }
    }

    fn parity_case(name: &str) -> (ReviewState, bool) {
        match name {
            "first_success" => (new_state(), true),
            "first_failure" => (new_state(), false),
            "proto_success" => (reviewed_state(), true),
            "proto_failure" => (reviewed_state(), false),
            "mature_success_fuzz_max" => (mature_state(), true),
            "mature_failure" => (mature_state(), false),
            "lethe_failure" => (lethe_state(), false),
            "due_floor" => {
                let mut state = mature_state();
                state.next_review_day = 20260815;
                (state, true)
            }
            _ => panic!("unknown parity case {name}"),
        }
    }

    fn reviewed_state() -> ReviewState {
        ReviewState {
            state_revision: 1,
            review_count: 1,
            last_reviewed_at_us: Some(1),
            last_review_day: Some(20260714),
            interval_days: Some(0),
            consecutive_successes: 1,
            total_successes: 1,
            ..new_state()
        }
    }

    fn mature_state() -> ReviewState {
        ReviewState {
            state_revision: 8,
            review_count: 8,
            last_reviewed_at_us: Some(1),
            last_review_day: Some(20260705),
            interval_days: Some(10),
            consecutive_successes: 8,
            total_successes: 8,
            gnosis_synolon: 2.3,
            ..new_state()
        }
    }

    fn lethe_state() -> ReviewState {
        ReviewState {
            total_successes: 6,
            consecutive_successes: 0,
            total_failures: 2,
            consecutive_failures: 2,
            ..mature_state()
        }
    }
}
