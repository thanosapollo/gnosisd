use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as BASE64_URL};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use thiserror::Error;

use crate::domain::{DueThema, ThemaId};

pub const MAX_DUE_THEMATA: u16 = 100;
const MAX_DUE_CURSOR_BYTES: usize = 512;
const DUE_CURSOR_VERSION: u32 = 1;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DueCursor {
    reviewed: bool,
    next_review_day: u32,
    thema_id: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct DueCursorPayload {
    version: u32,
    day: u32,
    reviewed: bool,
    next_review_day: u32,
    thema_id: String,
}

impl DueCursor {
    fn decode(value: &str, day: u32, key: &str) -> Result<Self, LegacyQueryError> {
        if value.is_empty() || value.len() > MAX_DUE_CURSOR_BYTES {
            return Err(LegacyQueryError::Cursor);
        }
        let token = BASE64_URL
            .decode(value)
            .map_err(|_| LegacyQueryError::Cursor)?;
        let payload_len = token
            .len()
            .checked_sub(32)
            .ok_or(LegacyQueryError::Cursor)?;
        let (payload, digest) = token.split_at(payload_len);
        if cursor_digest(key, payload).as_slice() != digest {
            return Err(LegacyQueryError::Cursor);
        }
        let decoded: DueCursorPayload =
            serde_json::from_slice(payload).map_err(|_| LegacyQueryError::Cursor)?;
        if decoded.version != DUE_CURSOR_VERSION || decoded.day != day {
            return Err(LegacyQueryError::Cursor);
        }
        let thema_id = ThemaId::new(decoded.thema_id)
            .map_err(|_| LegacyQueryError::Cursor)?
            .to_string();
        Ok(Self {
            reviewed: decoded.reviewed,
            next_review_day: decoded.next_review_day,
            thema_id,
        })
    }

    pub(crate) const fn reviewed(&self) -> bool {
        self.reviewed
    }

    pub(crate) const fn next_review_day(&self) -> u32 {
        self.next_review_day
    }

    pub(crate) fn thema_id(&self) -> &str {
        &self.thema_id
    }

    pub(crate) fn encode(self, day: u32, key: &str) -> String {
        let payload = serde_json::to_vec(&DueCursorPayload {
            version: DUE_CURSOR_VERSION,
            day,
            reviewed: self.reviewed,
            next_review_day: self.next_review_day,
            thema_id: self.thema_id,
        })
        .expect("due cursor payload is serializable");
        let digest = cursor_digest(key, &payload);
        let mut token = Vec::with_capacity(payload.len() + digest.len());
        token.extend_from_slice(&payload);
        token.extend_from_slice(&digest);
        BASE64_URL.encode(token)
    }

    pub(crate) fn after(thema: &DueThema) -> Self {
        Self {
            reviewed: thema.review_count > 0,
            next_review_day: thema.next_review_day,
            thema_id: thema.id.to_string(),
        }
    }
}

fn cursor_digest(key: &str, payload: &[u8]) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"gnosis-due-cursor-v1\0");
    digest.update(key.as_bytes());
    digest.update(payload);
    digest.finalize().into()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct QueryDueThemataPage {
    day: u32,
    limit: u16,
    after: Option<DueCursor>,
}

impl QueryDueThemataPage {
    pub(crate) fn new(
        day: u32,
        limit: u16,
        after: Option<String>,
        cursor_key: &str,
    ) -> Result<Self, LegacyQueryError> {
        if !valid_day(day) {
            return Err(LegacyQueryError::Day);
        }
        if limit == 0 || limit > MAX_DUE_THEMATA {
            return Err(LegacyQueryError::Limit);
        }
        Ok(Self {
            day,
            limit,
            after: after
                .as_deref()
                .map(|value| DueCursor::decode(value, day, cursor_key))
                .transpose()?,
        })
    }

    pub(crate) const fn day(&self) -> u32 {
        self.day
    }

    pub(crate) const fn limit(&self) -> u16 {
        self.limit
    }

    pub(crate) fn after(&self) -> Option<&DueCursor> {
        self.after.as_ref()
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum LegacyQueryError {
    #[error("day must be a valid YYYYMMDD date")]
    Day,
    #[error("due limit must be between 1 and {MAX_DUE_THEMATA}")]
    Limit,
    #[error("invalid due queue cursor")]
    Cursor,
}

fn valid_day(day: u32) -> bool {
    let year = day / 10_000;
    let month = (day / 100) % 100;
    let day_of_month = day % 100;
    if year == 0 || !(1..=12).contains(&month) {
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
    use crate::domain::{DueThema, ThemaId};

    use super::{DueCursor, LegacyQueryError, QueryDueThemataPage};

    const KEY: &str = "server-identity";

    #[test]
    fn validates_legacy_query_boundaries() {
        assert!(QueryDueThemataPage::new(101, 1, None, KEY).is_err());
        assert!(QueryDueThemataPage::new(10101, 1, None, KEY).is_ok());
        assert!(QueryDueThemataPage::new(20260229, 1, None, KEY).is_err());
        assert!(QueryDueThemataPage::new(20240229, 100, None, KEY).is_ok());
        assert_eq!(
            QueryDueThemataPage::new(20260714, 101, None, KEY).unwrap_err(),
            LegacyQueryError::Limit
        );
    }

    #[test]
    fn validates_due_page_cursors() {
        let first = QueryDueThemataPage::new(20260714, 100, None, KEY).unwrap();
        assert_eq!(first.after(), None);

        let source = DueThema {
            id: ThemaId::new("thema-b").unwrap(),
            kind: "basic".into(),
            keimenon: "Question".into(),
            hypothesis: Vec::new(),
            answer: vec!["Answer".into()],
            next_review_day: 20260713,
            review_count: 0,
        };
        let token = DueCursor::after(&source).encode(20260714, KEY);
        assert!(!token.contains("thema-b"));
        assert!(!token.contains("20260713"));
        let next = QueryDueThemataPage::new(20260714, 25, Some(token.clone()), KEY).unwrap();
        let cursor = next.after().unwrap();
        assert!(!cursor.reviewed());
        assert_eq!(cursor.next_review_day(), 20260713);
        assert_eq!(cursor.thema_id(), "thema-b");

        let longest_id = "t".repeat(128);
        let longest = DueThema {
            id: ThemaId::new(longest_id.clone()).unwrap(),
            kind: "basic".into(),
            keimenon: "Question".into(),
            hypothesis: Vec::new(),
            answer: vec!["Answer".into()],
            next_review_day: 20260713,
            review_count: 1,
        };
        let token = DueCursor::after(&longest).encode(20260714, KEY);
        let decoded = QueryDueThemataPage::new(20260714, 25, Some(token.clone()), KEY).unwrap();
        assert_eq!(decoded.after().unwrap().thema_id(), longest_id);

        assert!(QueryDueThemataPage::new(20260715, 25, Some(token.clone()), KEY).is_err());
        assert!(QueryDueThemataPage::new(20260714, 25, Some(token), "another-server").is_err());

        let mut tampered = DueCursor::after(&source).encode(20260714, KEY).into_bytes();
        let last = tampered.last_mut().unwrap();
        *last = if *last == b'A' { b'B' } else { b'A' };
        assert!(
            QueryDueThemataPage::new(
                20260714,
                25,
                Some(String::from_utf8(tampered).unwrap()),
                KEY,
            )
            .is_err()
        );
    }
}
