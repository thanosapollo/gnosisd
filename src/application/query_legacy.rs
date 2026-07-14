use thiserror::Error;

use crate::domain::ThemaId;

pub const MAX_DUE_THEMATA: u16 = 100;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryLegacyThema {
    thema_id: ThemaId,
}

impl QueryLegacyThema {
    pub fn new(thema_id: impl Into<String>) -> Result<Self, LegacyQueryError> {
        Ok(Self {
            thema_id: ThemaId::new(thema_id).map_err(|_| LegacyQueryError::ThemaId)?,
        })
    }

    pub(crate) fn thema_id(&self) -> &ThemaId {
        &self.thema_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueryDueThemata {
    day: u32,
    limit: u16,
}

impl QueryDueThemata {
    pub fn new(day: u32, limit: u16) -> Result<Self, LegacyQueryError> {
        if !valid_day(day) {
            return Err(LegacyQueryError::Day);
        }
        if limit == 0 || limit > MAX_DUE_THEMATA {
            return Err(LegacyQueryError::Limit);
        }
        Ok(Self { day, limit })
    }

    pub const fn day(self) -> u32 {
        self.day
    }

    pub const fn limit(self) -> u16 {
        self.limit
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub enum LegacyQueryError {
    #[error("invalid thema ID")]
    ThemaId,
    #[error("day must be a valid YYYYMMDD date")]
    Day,
    #[error("due limit must be between 1 and {MAX_DUE_THEMATA}")]
    Limit,
}

fn valid_day(day: u32) -> bool {
    let year = day / 10_000;
    let month = (day / 100) % 100;
    let day_of_month = day % 100;
    if !(1..=12).contains(&month) {
        return false;
    }
    (1..=days_in_month(year, month)).contains(&day_of_month)
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        2 if leap_year(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

const fn leap_year(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

#[cfg(test)]
mod tests {
    use super::{LegacyQueryError, QueryDueThemata, QueryLegacyThema};

    #[test]
    fn validates_legacy_query_boundaries() {
        assert!(QueryLegacyThema::new("7").is_ok());
        assert_eq!(
            QueryLegacyThema::new(" ").unwrap_err(),
            LegacyQueryError::ThemaId
        );
        assert!(QueryDueThemata::new(20260229, 1).is_err());
        assert!(QueryDueThemata::new(20240229, 100).is_ok());
        assert_eq!(
            QueryDueThemata::new(20260714, 101).unwrap_err(),
            LegacyQueryError::Limit
        );
    }
}
