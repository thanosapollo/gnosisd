use std::borrow::Cow;

use super::StoreError;

pub(super) fn string(value: &str, field: &'static str) -> Result<String, StoreError> {
    serde_json::from_str(json_compatible(value).as_ref())
        .map_err(|_| StoreError::CorruptData { field })
}

pub(super) fn optional_string(
    value: Option<&str>,
    field: &'static str,
) -> Result<Option<String>, StoreError> {
    match value {
        None | Some("nil") => Ok(None),
        Some(value) => string(value, field).map(Some),
    }
}

pub(super) fn string_list(value: &str, field: &'static str) -> Result<Vec<String>, StoreError> {
    optional_string_list(value, field)?
        .into_iter()
        .map(|value| value.ok_or(StoreError::CorruptData { field }))
        .collect()
}

pub(super) fn optional_string_list(
    value: &str,
    field: &'static str,
) -> Result<Vec<Option<String>>, StoreError> {
    if value == "nil" {
        return Ok(Vec::new());
    }
    let body = value
        .strip_prefix('(')
        .and_then(|value| value.strip_suffix(')'))
        .ok_or(StoreError::CorruptData { field })?;
    parse_optional_string_list(body, field)
}

fn parse_optional_string_list(
    mut value: &str,
    field: &'static str,
) -> Result<Vec<Option<String>>, StoreError> {
    let mut values = Vec::new();
    loop {
        value = value.trim_start();
        if value.is_empty() {
            return Ok(values);
        }
        if let Some(rest) = value.strip_prefix("nil").filter(|rest| token_ends(rest)) {
            values.push(None);
            value = rest;
            continue;
        }
        let end = quoted_string_end(value).ok_or(StoreError::CorruptData { field })?;
        values.push(Some(string(&value[..end], field)?));
        value = &value[end..];
    }
}

fn token_ends(value: &str) -> bool {
    value.chars().next().is_none_or(char::is_whitespace)
}

fn quoted_string_end(value: &str) -> Option<usize> {
    if !value.starts_with('"') {
        return None;
    }
    let mut escaped = false;
    for (index, character) in value.char_indices().skip(1) {
        match (escaped, character) {
            (true, _) => escaped = false,
            (false, '\\') => escaped = true,
            (false, '"') => return Some(index + 1),
            _ => {}
        }
    }
    None
}

fn json_compatible(value: &str) -> Cow<'_, str> {
    if !value.chars().any(|character| character <= '\u{1f}') {
        return Cow::Borrowed(value);
    }
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            character if character <= '\u{1f}' => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                escaped.push_str("\\u00");
                escaped.push(char::from(HEX[usize::from(character as u8 >> 4)]));
                escaped.push(char::from(HEX[usize::from(character as u8 & 0x0f)]));
            }
            character => escaped.push(character),
        }
    }
    Cow::Owned(escaped)
}

#[cfg(test)]
mod tests {
    use super::{optional_string, optional_string_list, string, string_list};

    #[test]
    fn decodes_emacsql_string_values() {
        assert_eq!(
            string(r#""line\n\"quoted\"""#, "value").unwrap(),
            "line\n\"quoted\""
        );
        assert_eq!(
            string_list(r#"("first" "second value")"#, "list").unwrap(),
            ["first", "second value"]
        );
        assert_eq!(optional_string(Some("nil"), "optional").unwrap(), None);
    }

    #[test]
    fn decodes_literal_newlines_in_emacsql_strings() {
        assert_eq!(
            string("\"first line\nsecond line\"", "value").unwrap(),
            "first line\nsecond line"
        );
        assert_eq!(
            string_list("(\"first line\nsecond line\")", "list").unwrap(),
            ["first line\nsecond line"]
        );
    }

    #[test]
    fn decodes_absent_values_in_emacsql_string_lists() {
        assert_eq!(
            optional_string_list("(nil \"hint\" nil)", "list").unwrap(),
            [None, Some("hint".into()), None]
        );
    }

    #[test]
    fn rejects_values_outside_the_supported_legacy_contract() {
        assert!(string("bare-symbol", "value").is_err());
        assert!(string_list(r#"("valid" bare-symbol)"#, "list").is_err());
    }
}
