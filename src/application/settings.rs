use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::FsrsConfigSnapshot;
use crate::domain::fsrs::FsrsConfigSnapshot as DomainFsrsConfigSnapshot;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SettingsRecord {
    pub(crate) revision: u64,
    pub(crate) timezone: String,
    pub(crate) day_boundary_hour: u8,
    pub(crate) scheduler: FsrsConfigSnapshot,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsInput {
    pub(crate) timezone: String,
    pub(crate) day_boundary_hour: u8,
    pub(crate) desired_retention: f32,
}

impl SettingsInput {
    pub(crate) fn validate(self) -> Result<Self, SettingsError> {
        self.timezone
            .parse::<Tz>()
            .map_err(|_| SettingsError::Timezone)?;
        if self.day_boundary_hour > 23 {
            return Err(SettingsError::Boundary);
        }
        let config = DomainFsrsConfigSnapshot {
            desired_retention: self.desired_retention,
            ..DomainFsrsConfigSnapshot::default()
        };
        crate::domain::fsrs::compute_fsrs_transition(
            None,
            0,
            20260101,
            1,
            crate::domain::fsrs::RecallOutcome::Success,
            &config,
        )
        .map_err(|_| SettingsError::Scheduler)?;
        Ok(self)
    }

    pub(crate) fn scheduler(&self) -> FsrsConfigSnapshot {
        let config = DomainFsrsConfigSnapshot {
            desired_retention: self.desired_retention,
            ..DomainFsrsConfigSnapshot::default()
        };
        (&config).into()
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum SettingsError {
    #[error("timezone is invalid")]
    Timezone,
    #[error("desired retention is invalid")]
    Scheduler,
    #[error("day boundary is invalid")]
    Boundary,
}

#[cfg(test)]
mod tests {
    use super::{SettingsError, SettingsInput};

    fn settings() -> SettingsInput {
        SettingsInput {
            timezone: "Europe/Athens".into(),
            day_boundary_hour: 0,
            desired_retention: 0.9,
        }
    }

    #[test]
    fn validates_timezone_and_desired_retention() {
        assert!(settings().validate().is_ok());
        let mut invalid = settings();
        invalid.timezone = "Mars/Olympus".into();
        assert_eq!(invalid.validate().unwrap_err(), SettingsError::Timezone);

        let mut invalid = settings();
        invalid.desired_retention = 1.0;
        assert_eq!(invalid.validate().unwrap_err(), SettingsError::Scheduler);
    }
}
