use std::{fmt, str::FromStr};

use thiserror::Error;

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum IdError {
    #[error("{kind} must not be empty")]
    Empty { kind: &'static str },
}

macro_rules! opaque_id {
    ($name:ident, $kind:literal) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, IdError> {
                let value = value.into();
                if value.trim().is_empty() {
                    Err(IdError::Empty { kind: $kind })
                } else {
                    Ok(Self(value))
                }
            }

            #[allow(
                dead_code,
                reason = "opaque ID families share this adapter accessor before every family has a writer"
            )]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = IdError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }

        impl TryFrom<String> for $name {
            type Error = IdError;

            fn try_from(value: String) -> Result<Self, Self::Error> {
                Self::new(value)
            }
        }
    };
}

opaque_id!(DocumentId, "document ID");
opaque_id!(NodeId, "node ID");
opaque_id!(ThemaId, "thema ID");

#[cfg(test)]
mod tests {
    use super::{DocumentId, IdError, NodeId, ThemaId};

    #[test]
    fn rejects_empty_ids() {
        for error in [
            DocumentId::new(" ").unwrap_err(),
            NodeId::new("").unwrap_err(),
            ThemaId::new("\t").unwrap_err(),
        ] {
            assert!(matches!(error, IdError::Empty { .. }));
        }
    }
}
