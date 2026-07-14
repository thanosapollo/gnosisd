use super::StoreError;

pub(super) fn string(value: &str, field: &'static str) -> Result<String, StoreError> {
    serde_json::from_str(value).map_err(|_| StoreError::CorruptData { field })
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
    if value == "nil" {
        return Ok(Vec::new());
    }
    let body = value
        .strip_prefix('(')
        .and_then(|value| value.strip_suffix(')'))
        .ok_or(StoreError::CorruptData { field })?;
    serde_json::Deserializer::from_str(body)
        .into_iter::<String>()
        .map(|value| value.map_err(|_| StoreError::CorruptData { field }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{optional_string, string, string_list};

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
    fn rejects_values_outside_the_supported_legacy_contract() {
        assert!(string("bare-symbol", "value").is_err());
        assert!(string_list(r#"("valid" bare-symbol)"#, "list").is_err());
    }
}
