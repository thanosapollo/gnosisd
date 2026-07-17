use std::collections::BTreeMap;

use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::ParametersSnapshot;
use crate::domain::{SchedulerParameters, validate_parameters};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct SettingsRecord {
    pub(crate) revision: u64,
    pub(crate) timezone: String,
    pub(crate) day_boundary_hour: u8,
    pub(crate) scheduler: ParametersSnapshot,
    pub(crate) tag_rules: BTreeMap<String, TagRule>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SettingsInput {
    pub(crate) timezone: String,
    pub(crate) day_boundary_hour: u8,
    pub(crate) scheduler: ParametersSnapshot,
    #[serde(default)]
    pub(crate) tag_rules: BTreeMap<String, TagRule>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TagRule {
    pub(crate) amnesia: Option<f64>,
    pub(crate) epignosis: Option<f64>,
    pub(crate) agnoia: Option<f64>,
    pub(crate) anagnosis: Option<u64>,
    pub(crate) lethe: Option<u64>,
    pub(crate) proto: Option<Vec<u32>>,
}

impl SettingsInput {
    pub(crate) fn validate(self) -> Result<Self, SettingsError> {
        self.timezone
            .parse::<Tz>()
            .map_err(|_| SettingsError::Timezone)?;
        if self.day_boundary_hour > 23 {
            return Err(SettingsError::Boundary);
        }
        validate_parameters(&SchedulerParameters::from(self.scheduler.clone()))
            .map_err(|_| SettingsError::Scheduler)?;
        for (tag, rule) in &self.tag_rules {
            if tag.trim().is_empty() || tag.len() > 128 {
                return Err(SettingsError::Tag);
            }
            validate_parameters(&rule.apply(&self.scheduler)).map_err(|_| SettingsError::Rule)?;
        }
        Ok(self)
    }
}

impl TagRule {
    pub(crate) fn apply(&self, global: &ParametersSnapshot) -> SchedulerParameters {
        SchedulerParameters {
            scheduler_version: global.scheduler_version,
            amnesia: self.amnesia.unwrap_or(global.amnesia),
            epignosis: self.epignosis.unwrap_or(global.epignosis),
            agnoia: self.agnoia.unwrap_or(global.agnoia),
            anagnosis: self.anagnosis.unwrap_or(global.anagnosis),
            lethe: self.lethe.unwrap_or(global.lethe),
            synolon_max: global.synolon_max,
            interval_fuzz: global.interval_fuzz,
            proto: self.proto.clone().unwrap_or_else(|| global.proto.clone()),
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub(crate) enum SettingsError {
    #[error("timezone is invalid")]
    Timezone,
    #[error("scheduler parameters are invalid")]
    Scheduler,
    #[error("day boundary is invalid")]
    Boundary,
    #[error("tag rule name is invalid")]
    Tag,
    #[error("tag rule parameters are invalid")]
    Rule,
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::{SettingsError, SettingsInput, TagRule};
    use crate::application::ParametersSnapshot;

    fn settings() -> SettingsInput {
        SettingsInput {
            timezone: "Europe/Athens".into(),
            day_boundary_hour: 0,
            scheduler: ParametersSnapshot {
                scheduler_version: 1,
                amnesia: 0.5,
                epignosis: 0.1,
                agnoia: 0.2,
                anagnosis: 3,
                lethe: 2,
                synolon_max: 3.0,
                interval_fuzz: 0.1,
                proto: vec![0, 1, 2],
            },
            tag_rules: BTreeMap::from([(
                "vocabulary".into(),
                TagRule {
                    amnesia: Some(0.65),
                    ..TagRule::default()
                },
            )]),
        }
    }

    #[test]
    fn validates_timezone_and_merged_tag_rules() {
        assert!(settings().validate().is_ok());
        let mut invalid = settings();
        invalid.timezone = "Mars/Olympus".into();
        assert_eq!(invalid.validate().unwrap_err(), SettingsError::Timezone);
    }

    #[test]
    fn rejects_invalid_rule_after_global_merge() {
        let mut invalid = settings();
        invalid.tag_rules.get_mut("vocabulary").unwrap().amnesia = Some(1.5);
        assert_eq!(invalid.validate().unwrap_err(), SettingsError::Rule);
    }
}
