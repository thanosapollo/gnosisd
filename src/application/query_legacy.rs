use thiserror::Error;

use crate::domain::{DueThema, ThemaId};

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DueCursor {
    reviewed: bool,
    next_review_day: u32,
    thema_id: i64,
}

impl DueCursor {
    fn parse(value: &str) -> Result<Self, LegacyQueryError> {
        let mut parts = value.split(':');
        let reviewed = match parts.next() {
            Some("0") => false,
            Some("1") => true,
            _ => return Err(LegacyQueryError::Cursor),
        };
        let next_review_day = parse_cursor_part(parts.next())?;
        let thema_id = parse_cursor_part(parts.next())?;
        if parts.next().is_some() || thema_id <= 0 {
            return Err(LegacyQueryError::Cursor);
        }
        Ok(Self {
            reviewed,
            next_review_day,
            thema_id,
        })
    }

    pub(crate) const fn reviewed(self) -> bool {
        self.reviewed
    }

    pub(crate) const fn next_review_day(self) -> u32 {
        self.next_review_day
    }

    pub(crate) const fn thema_id(self) -> i64 {
        self.thema_id
    }

    pub(crate) fn encode(self) -> String {
        format!(
            "{}:{}:{}",
            u8::from(self.reviewed),
            self.next_review_day,
            self.thema_id
        )
    }

    pub(crate) fn after(thema: &DueThema) -> Result<Self, LegacyQueryError> {
        Ok(Self {
            reviewed: thema.review_count > 0,
            next_review_day: thema.next_review_day,
            thema_id: thema
                .id
                .as_str()
                .parse()
                .map_err(|_| LegacyQueryError::Cursor)?,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueryDueThemataPage {
    day: u32,
    limit: u16,
    after: Option<DueCursor>,
}

impl QueryDueThemataPage {
    pub fn new(day: u32, limit: u16, after: Option<String>) -> Result<Self, LegacyQueryError> {
        QueryDueThemata::new(day, limit)?;
        Ok(Self {
            day,
            limit,
            after: after.as_deref().map(DueCursor::parse).transpose()?,
        })
    }

    pub const fn day(&self) -> u32 {
        self.day
    }

    pub const fn limit(&self) -> u16 {
        self.limit
    }

    pub(crate) const fn after(&self) -> Option<DueCursor> {
        self.after
    }
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
    #[error("invalid due queue cursor")]
    Cursor,
}

fn parse_cursor_part<T: std::str::FromStr>(value: Option<&str>) -> Result<T, LegacyQueryError> {
    value
        .ok_or(LegacyQueryError::Cursor)?
        .parse()
        .map_err(|_| LegacyQueryError::Cursor)
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
    use super::{LegacyQueryError, QueryDueThemata, QueryDueThemataPage, QueryLegacyThema};

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

    #[test]
    fn validates_due_page_cursors() {
        let first = QueryDueThemataPage::new(20260714, 100, None).unwrap();
        assert_eq!(first.after(), None);

        let next = QueryDueThemataPage::new(20260714, 25, Some("0:20260713:73000000000000".into()))
            .unwrap();
        let cursor = next.after().unwrap();
        assert!(!cursor.reviewed());
        assert_eq!(cursor.next_review_day(), 20260713);
        assert_eq!(cursor.thema_id(), 73_000_000_000_000);

        let legacy = QueryDueThemataPage::new(20260714, 25, Some("0:0:1".into())).unwrap();
        assert_eq!(legacy.after().unwrap().next_review_day(), 0);

        assert!(QueryDueThemataPage::new(20260714, 25, Some("bad".into())).is_err());
    }
}
