use chrono::{Datelike, Days, NaiveDate};
use fsrs::{FSRS, MemoryState};
use thiserror::Error;

pub(crate) const DEFAULT_FSRS_PARAMETERS: [f32; 21] = [
    0.212, 1.2931, 2.3065, 8.2956, 6.4133, 0.8334, 3.0194, 0.001, 1.8722, 0.1666, 0.796, 1.4835,
    0.0614, 0.2629, 1.6483, 0.6014, 1.8729, 0.5425, 0.0912, 0.0658, 0.1542,
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FsrsAlgorithm {
    Fsrs,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FsrsModel {
    Six,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FsrsImplementation {
    Crate6Dot6Dot1,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FsrsConfigSnapshot {
    pub(crate) algorithm: FsrsAlgorithm,
    pub(crate) model: FsrsModel,
    pub(crate) implementation: FsrsImplementation,
    pub(crate) desired_retention: f32,
    pub(crate) parameters: [f32; 21],
}

impl Default for FsrsConfigSnapshot {
    fn default() -> Self {
        Self {
            algorithm: FsrsAlgorithm::Fsrs,
            model: FsrsModel::Six,
            implementation: FsrsImplementation::Crate6Dot6Dot1,
            desired_retention: 0.90,
            parameters: DEFAULT_FSRS_PARAMETERS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct FsrsMemoryState {
    pub(crate) stability: f32,
    pub(crate) difficulty: f32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecallOutcome {
    Failure,
    Success,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FsrsReplayMetadata {
    pub(crate) config: FsrsConfigSnapshot,
    pub(crate) prior_state: Option<FsrsMemoryState>,
    pub(crate) elapsed_days: u32,
    pub(crate) review_day: u32,
    pub(crate) reviewed_at_us: u64,
    pub(crate) outcome: RecallOutcome,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FsrsTransition {
    pub(crate) state: FsrsMemoryState,
    pub(crate) raw_interval_days: f32,
    pub(crate) calendar_interval_days: u32,
    pub(crate) due_day: u32,
    pub(crate) replay: FsrsReplayMetadata,
}

pub(crate) fn compute_fsrs_transition(
    prior_state: Option<FsrsMemoryState>,
    elapsed_days: u32,
    review_day: u32,
    reviewed_at_us: u64,
    outcome: RecallOutcome,
    config: &FsrsConfigSnapshot,
) -> Result<FsrsTransition, FsrsError> {
    validate_config(config)?;
    if prior_state.is_none() && elapsed_days != 0 {
        return Err(FsrsError::Input);
    }
    if let Some(state) = prior_state {
        validate_state(state)?;
    }
    let review_date = parse_day(review_day)?;
    let scheduler = FSRS::new(&config.parameters).map_err(|_| FsrsError::Config)?;
    let prior_memory = prior_state.map(|state| MemoryState {
        stability: state.stability,
        difficulty: state.difficulty,
    });
    let states = scheduler
        .next_states(prior_memory, config.desired_retention, elapsed_days)
        .map_err(|_| FsrsError::Computation)?;
    let selected = match outcome {
        RecallOutcome::Failure => states.again,
        RecallOutcome::Success => states.good,
    };
    let state = FsrsMemoryState {
        stability: selected.memory.stability,
        difficulty: selected.memory.difficulty,
    };
    validate_state(state).map_err(|_| FsrsError::Computation)?;
    let calendar_interval_days = calendar_interval(selected.interval)?;
    let due_date = review_date
        .checked_add_days(Days::new(u64::from(calendar_interval_days)))
        .ok_or(FsrsError::Interval)?;

    Ok(FsrsTransition {
        state,
        raw_interval_days: selected.interval,
        calendar_interval_days,
        due_day: format_day(due_date)?,
        replay: FsrsReplayMetadata {
            config: config.clone(),
            prior_state,
            elapsed_days,
            review_day,
            reviewed_at_us,
            outcome,
        },
    })
}

pub(crate) fn validate_config(config: &FsrsConfigSnapshot) -> Result<(), FsrsError> {
    if config.algorithm == FsrsAlgorithm::Fsrs
        && config.model == FsrsModel::Six
        && config.implementation == FsrsImplementation::Crate6Dot6Dot1
        && config.desired_retention.is_finite()
        && (0.0..1.0).contains(&config.desired_retention)
        && config.parameters == DEFAULT_FSRS_PARAMETERS
    {
        Ok(())
    } else {
        Err(FsrsError::Config)
    }
}

fn validate_state(state: FsrsMemoryState) -> Result<(), FsrsError> {
    if state.stability.is_finite()
        && state.stability > 0.0
        && state.difficulty.is_finite()
        && (1.0..=10.0).contains(&state.difficulty)
    {
        Ok(())
    } else {
        Err(FsrsError::State)
    }
}

fn calendar_interval(raw_interval_days: f32) -> Result<u32, FsrsError> {
    let raw_interval_days = f64::from(raw_interval_days);
    if !raw_interval_days.is_finite() || raw_interval_days < 0.0 {
        return Err(FsrsError::Interval);
    }
    // Whole calendar days use ties-to-even rounding and a one-day floor.
    let rounded = raw_interval_days.round_ties_even().max(1.0);
    if rounded > f64::from(u32::MAX) {
        return Err(FsrsError::Interval);
    }
    Ok(rounded as u32)
}

fn parse_day(day: u32) -> Result<NaiveDate, FsrsError> {
    let year = i32::try_from(day / 10_000).map_err(|_| FsrsError::Date)?;
    NaiveDate::from_ymd_opt(year, (day / 100) % 100, day % 100).ok_or(FsrsError::Date)
}

fn format_day(date: NaiveDate) -> Result<u32, FsrsError> {
    let year = u32::try_from(date.year()).map_err(|_| FsrsError::Date)?;
    year.checked_mul(10_000)
        .and_then(|day| day.checked_add(date.month() * 100))
        .and_then(|day| day.checked_add(date.day()))
        .ok_or(FsrsError::Date)
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum FsrsError {
    #[error("FSRS state is invalid")]
    State,
    #[error("FSRS configuration is invalid")]
    Config,
    #[error("FSRS transition input is invalid")]
    Input,
    #[error("review day is invalid")]
    Date,
    #[error("review interval cannot be represented")]
    Interval,
    #[error("FSRS computation failed")]
    Computation,
}

#[cfg(test)]
mod tests {
    use super::*;

    const REVIEW_DAY: u32 = 20260717;
    const REVIEWED_AT_US: u64 = 1_752_710_400_000_000;

    fn existing_state() -> FsrsMemoryState {
        FsrsMemoryState {
            stability: 10.0,
            difficulty: 5.0,
        }
    }

    fn schedule(
        state: Option<FsrsMemoryState>,
        elapsed_days: u32,
        outcome: RecallOutcome,
        config: &FsrsConfigSnapshot,
    ) -> FsrsTransition {
        compute_fsrs_transition(
            state,
            elapsed_days,
            REVIEW_DAY,
            REVIEWED_AT_US,
            outcome,
            config,
        )
        .unwrap()
    }

    #[test]
    fn first_review_maps_failure_to_again_and_success_to_good() {
        let config = FsrsConfigSnapshot::default();
        let again = schedule(None, 0, RecallOutcome::Failure, &config);
        let good = schedule(None, 0, RecallOutcome::Success, &config);

        assert_eq!(DEFAULT_FSRS_PARAMETERS, fsrs::DEFAULT_PARAMETERS);
        assert_eq!(again.state.stability, DEFAULT_FSRS_PARAMETERS[0]);
        assert_eq!(good.state.stability, DEFAULT_FSRS_PARAMETERS[2]);
        assert!(again.state.difficulty > good.state.difficulty);
        assert_eq!(again.calendar_interval_days, 1);
        assert_eq!(again.due_day, 20260718);
        assert!(good.calendar_interval_days >= again.calendar_interval_days);
    }

    #[test]
    fn existing_review_maps_failure_and_success_to_distinct_states() {
        let config = FsrsConfigSnapshot::default();
        let again = schedule(Some(existing_state()), 10, RecallOutcome::Failure, &config);
        let good = schedule(Some(existing_state()), 10, RecallOutcome::Success, &config);

        assert!(again.state.stability < good.state.stability);
        assert!(again.state.difficulty > good.state.difficulty);
        assert!(again.raw_interval_days < good.raw_interval_days);
    }

    #[test]
    fn elapsed_same_day_on_time_and_overdue_reviews_are_ordered() {
        let config = FsrsConfigSnapshot::default();
        let same_day = schedule(Some(existing_state()), 0, RecallOutcome::Success, &config);
        let on_time = schedule(Some(existing_state()), 10, RecallOutcome::Success, &config);
        let overdue = schedule(Some(existing_state()), 20, RecallOutcome::Success, &config);

        assert!(same_day.state.stability < on_time.state.stability);
        assert!(on_time.state.stability < overdue.state.stability);
        assert!(same_day.raw_interval_days < on_time.raw_interval_days);
        assert!(on_time.raw_interval_days < overdue.raw_interval_days);
    }

    #[test]
    fn higher_desired_retention_never_lengthens_interval() {
        let lower = FsrsConfigSnapshot {
            desired_retention: 0.85,
            ..FsrsConfigSnapshot::default()
        };
        let higher = FsrsConfigSnapshot {
            desired_retention: 0.95,
            ..FsrsConfigSnapshot::default()
        };

        let lower = schedule(Some(existing_state()), 10, RecallOutcome::Success, &lower);
        let higher = schedule(Some(existing_state()), 10, RecallOutcome::Success, &higher);

        assert!(higher.raw_interval_days <= lower.raw_interval_days);
        assert!(higher.calendar_interval_days <= lower.calendar_interval_days);
    }

    #[test]
    fn invalid_state_config_and_new_item_elapsed_time_are_rejected() {
        let config = FsrsConfigSnapshot::default();
        let invalid_state = FsrsMemoryState {
            stability: f32::NAN,
            difficulty: 5.0,
        };
        assert_eq!(
            compute_fsrs_transition(
                Some(invalid_state),
                1,
                REVIEW_DAY,
                REVIEWED_AT_US,
                RecallOutcome::Success,
                &config,
            ),
            Err(FsrsError::State)
        );

        let mut invalid_config = config.clone();
        invalid_config.parameters[0] += 1.0;
        assert_eq!(
            compute_fsrs_transition(
                None,
                0,
                REVIEW_DAY,
                REVIEWED_AT_US,
                RecallOutcome::Success,
                &invalid_config,
            ),
            Err(FsrsError::Config)
        );
        assert_eq!(
            compute_fsrs_transition(
                None,
                1,
                REVIEW_DAY,
                REVIEWED_AT_US,
                RecallOutcome::Success,
                &config,
            ),
            Err(FsrsError::Input)
        );
        assert_eq!(calendar_interval(f32::NAN), Err(FsrsError::Interval));
        assert_eq!(
            compute_fsrs_transition(
                None,
                0,
                20260230,
                REVIEWED_AT_US,
                RecallOutcome::Success,
                &config,
            ),
            Err(FsrsError::Date)
        );
    }

    #[test]
    fn identical_input_replays_identically_with_complete_metadata() {
        let config = FsrsConfigSnapshot::default();
        let first = schedule(Some(existing_state()), 10, RecallOutcome::Success, &config);
        let second = schedule(Some(existing_state()), 10, RecallOutcome::Success, &config);

        assert_eq!(first, second);
        assert_eq!(first.replay.config, config);
        assert_eq!(first.replay.elapsed_days, 10);
        assert_eq!(first.replay.review_day, REVIEW_DAY);
        assert_eq!(first.replay.reviewed_at_us, REVIEWED_AT_US);
        assert_eq!(first.replay.outcome, RecallOutcome::Success);
        assert_eq!(first.replay.prior_state, Some(existing_state()));
    }
}
