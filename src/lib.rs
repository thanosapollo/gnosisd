#![forbid(unsafe_code)]

mod application;
mod blob;
pub mod client_http;
pub mod client_replica;
mod domain;
mod http;
pub mod migration;
pub(crate) mod parser;
pub mod protocol;
mod store;

pub use domain::{ThemaConcept, ThemaConceptRelation};

pub(crate) fn is_canonical_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(test)]
mod digest_tests {
    use super::is_canonical_sha256;

    #[test]
    fn canonical_sha256_is_exact_lowercase_hex() {
        assert!(is_canonical_sha256(&"0".repeat(64)));
        assert!(is_canonical_sha256(&"af".repeat(32)));
        for value in [
            "a".repeat(63),
            "a".repeat(65),
            "A".repeat(64),
            "g".repeat(64),
        ] {
            assert!(!is_canonical_sha256(&value));
        }
    }
}
