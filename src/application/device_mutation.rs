use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use super::{PreparedNote, valid_operation_id};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DeviceMutationKind {
    DocumentCreate,
    DocumentReplace,
}

impl DeviceMutationKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::DocumentCreate => "document_create",
            Self::DocumentReplace => "document_replace",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeviceMutationIdentity {
    pub(crate) format_version: u32,
    pub(crate) operation_id: String,
    pub(crate) device_id: String,
    pub(crate) mutation_kind: DeviceMutationKind,
    pub(crate) target_id: Option<String>,
    pub(crate) request_sha256: String,
    pub(crate) request_json: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreparedDeviceDocumentMutation {
    pub(crate) identity: DeviceMutationIdentity,
    pub(crate) prepared: PreparedNote,
}

impl PreparedDeviceDocumentMutation {
    pub(crate) fn validate(&self) -> Result<(), DeviceMutationError> {
        validate_identity(
            self.identity.format_version,
            &self.identity.operation_id,
            &self.identity.device_id,
            &self.prepared,
        )?;
        let kind = mutation_kind(&self.prepared);
        let target = self
            .prepared
            .expected_revision
            .map(|_| self.prepared.document_id.to_string());
        let request = document_request_json(self.identity.format_version, kind, &self.prepared)?;
        let digest = format!("{:x}", Sha256::digest(request.as_bytes()));
        if self.identity.mutation_kind == kind
            && self.identity.target_id == target
            && self.identity.request_json == request
            && self.identity.request_sha256 == digest
        {
            Ok(())
        } else {
            Err(DeviceMutationError::Identity)
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeviceMutationCursor {
    pub(crate) server_id: String,
    pub(crate) position: u64,
    pub(crate) token: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub(crate) enum DeviceMutationProjection {
    Document(super::DocumentHead),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub(crate) enum DeviceMutationRejection {
    RevisionConflict {
        expected: Option<u64>,
        actual: Option<u64>,
    },
    NotFound,
    InvalidContent,
    RenameRequired,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum DeviceMutationResult {
    Applied {
        format_version: u32,
        operation_id: String,
        changed: bool,
        authoritative: DeviceMutationProjection,
        cursor: DeviceMutationCursor,
    },
    Rejected {
        format_version: u32,
        operation_id: String,
        rejection: DeviceMutationRejection,
        cursor: DeviceMutationCursor,
    },
}

impl DeviceMutationResult {
    pub(crate) fn applied(
        identity: &DeviceMutationIdentity,
        changed: bool,
        authoritative: DeviceMutationProjection,
        cursor: DeviceMutationCursor,
    ) -> Self {
        Self::Applied {
            format_version: identity.format_version,
            operation_id: identity.operation_id.clone(),
            changed,
            authoritative,
            cursor,
        }
    }

    pub(crate) fn rejected(
        identity: &DeviceMutationIdentity,
        rejection: DeviceMutationRejection,
        cursor: DeviceMutationCursor,
    ) -> Self {
        Self::Rejected {
            format_version: identity.format_version,
            operation_id: identity.operation_id.clone(),
            rejection,
            cursor,
        }
    }

    pub(crate) fn validate_for(
        &self,
        identity: &DeviceMutationIdentity,
    ) -> Result<(), DeviceMutationError> {
        let (format_version, operation_id, cursor) = match self {
            Self::Applied {
                format_version,
                operation_id,
                cursor,
                ..
            }
            | Self::Rejected {
                format_version,
                operation_id,
                cursor,
                ..
            } => (*format_version, operation_id, cursor),
        };
        if format_version == identity.format_version
            && operation_id == &identity.operation_id
            && !cursor.server_id.is_empty()
            && !cursor.token.is_empty()
            && self.projection_matches(identity)
        {
            Ok(())
        } else {
            Err(DeviceMutationError::Result)
        }
    }

    pub(crate) const fn result_kind(&self) -> &'static str {
        match self {
            Self::Applied { .. } => "applied",
            Self::Rejected { .. } => "rejected",
        }
    }

    pub(crate) const fn changed(&self) -> bool {
        matches!(self, Self::Applied { changed: true, .. })
    }

    fn projection_matches(&self, identity: &DeviceMutationIdentity) -> bool {
        match self {
            Self::Applied {
                authoritative: DeviceMutationProjection::Document(document),
                ..
            } => identity
                .target_id
                .as_deref()
                .is_none_or(|target| target == document.document_id),
            Self::Rejected { .. } => true,
        }
    }
}

#[derive(Serialize)]
struct DocumentMutationPayload<'a> {
    format_version: u32,
    mutation_kind: DeviceMutationKind,
    document_id: Option<&'a str>,
    logical_path: &'a str,
    org: &'a str,
    blob_sha256: &'a str,
    expected_revision: Option<u64>,
}

pub(crate) fn prepare_device_document_mutation(
    format_version: u32,
    operation_id: String,
    device_id: String,
    prepared: PreparedNote,
) -> Result<PreparedDeviceDocumentMutation, DeviceMutationError> {
    validate_identity(format_version, &operation_id, &device_id, &prepared)?;
    let mutation_kind = mutation_kind(&prepared);
    let request_json = document_request_json(format_version, mutation_kind, &prepared)?;
    let request_sha256 = format!("{:x}", Sha256::digest(request_json.as_bytes()));
    Ok(PreparedDeviceDocumentMutation {
        identity: DeviceMutationIdentity {
            format_version,
            operation_id,
            device_id,
            mutation_kind,
            target_id: prepared
                .expected_revision
                .map(|_| prepared.document_id.to_string()),
            request_sha256,
            request_json,
        },
        prepared,
    })
}

fn validate_identity(
    format_version: u32,
    operation_id: &str,
    device_id: &str,
    prepared: &PreparedNote,
) -> Result<(), DeviceMutationError> {
    let revision_fits = prepared
        .expected_revision
        .is_none_or(|revision| i64::try_from(revision).is_ok());
    if format_version == 1
        && valid_operation_id(operation_id)
        && !device_id.is_empty()
        && device_id.len() <= 128
        && !device_id.contains('\0')
        && revision_fits
    {
        Ok(())
    } else {
        Err(DeviceMutationError::Identity)
    }
}

fn mutation_kind(prepared: &PreparedNote) -> DeviceMutationKind {
    if prepared.expected_revision.is_some() {
        DeviceMutationKind::DocumentReplace
    } else {
        DeviceMutationKind::DocumentCreate
    }
}

fn document_request_json(
    format_version: u32,
    mutation_kind: DeviceMutationKind,
    prepared: &PreparedNote,
) -> Result<String, DeviceMutationError> {
    serde_json::to_string(&DocumentMutationPayload {
        format_version,
        mutation_kind,
        document_id: prepared
            .expected_revision
            .map(|_| prepared.document_id.as_str()),
        logical_path: &prepared.logical_path,
        org: &prepared.org,
        blob_sha256: &prepared.blob_sha256,
        expected_revision: prepared.expected_revision,
    })
    .map_err(|_| DeviceMutationError::Encoding)
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
pub(crate) enum DeviceMutationError {
    #[error("device mutation identity is invalid")]
    Identity,
    #[error("device mutation could not be encoded")]
    Encoding,
    #[error("stored device mutation result is invalid")]
    Result,
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::{
        DeviceMutationCursor, DeviceMutationError, DeviceMutationKind, DeviceMutationProjection,
        DeviceMutationRejection, DeviceMutationResult, prepare_device_document_mutation,
    };
    use crate::application::{DocumentHead, prepare_note};

    #[test]
    fn document_create_fingerprint_is_stable_and_excludes_generated_identity() {
        let org = "#+title: Stable\n";
        let first = prepare_device_document_mutation(
            1,
            "operation-a".into(),
            "device-a".into(),
            note("first", org, None),
        )
        .unwrap();
        let second = prepare_device_document_mutation(
            1,
            "operation-b".into(),
            "device-b".into(),
            note("second", org, None),
        )
        .unwrap();
        let digest = format!("{:x}", Sha256::digest(org.as_bytes()));
        let expected = format!(
            "{{\"format_version\":1,\"mutation_kind\":\"document_create\",\
             \"document_id\":null,\"logical_path\":\"note.org\",\
             \"org\":\"#+title: Stable\\n\",\"blob_sha256\":\"{digest}\",\
             \"expected_revision\":null}}"
        );

        assert_eq!(
            first.identity.mutation_kind,
            DeviceMutationKind::DocumentCreate
        );
        assert_eq!(first.identity.request_json, expected);
        assert_eq!(first.identity.request_json, second.identity.request_json);
        assert_eq!(
            first.identity.request_sha256,
            second.identity.request_sha256
        );
    }

    #[test]
    fn document_replace_fingerprint_captures_target_and_revision() {
        let operation = prepare_device_document_mutation(
            1,
            "operation".into(),
            "device".into(),
            note("document", "#+title: Changed\n", Some(7)),
        )
        .unwrap();

        assert_eq!(
            operation.identity.mutation_kind,
            DeviceMutationKind::DocumentReplace
        );
        assert!(
            operation
                .identity
                .request_json
                .contains("\"document_id\":\"document\"")
        );
        assert!(
            operation
                .identity
                .request_json
                .contains("\"expected_revision\":7")
        );
    }

    #[test]
    fn device_mutations_reject_invalid_envelope_identity() {
        for (version, operation_id, device_id) in [
            (2, "operation", "device"),
            (1, "-operation", "device"),
            (1, "operation", ""),
        ] {
            assert_eq!(
                prepare_device_document_mutation(
                    version,
                    operation_id.into(),
                    device_id.into(),
                    note("document", "#+title: Note\n", None),
                ),
                Err(DeviceMutationError::Identity)
            );
        }
    }

    #[test]
    fn mutation_results_round_trip_with_exclusive_typed_shapes() {
        let operation = prepare_device_document_mutation(
            1,
            "operation".into(),
            "device".into(),
            note("document", "#+title: Note\n", None),
        )
        .unwrap();
        let cursor = DeviceMutationCursor {
            server_id: "server".into(),
            position: 3,
            token: "token".into(),
        };
        let applied = DeviceMutationResult::applied(
            &operation.identity,
            true,
            DeviceMutationProjection::Document(DocumentHead {
                document_id: "document".into(),
                logical_path: "note.org".into(),
                revision: 1,
                blob_sha256: Some("a".repeat(64)),
                active: true,
            }),
            cursor.clone(),
        );
        let rejected = DeviceMutationResult::rejected(
            &operation.identity,
            DeviceMutationRejection::RevisionConflict {
                expected: Some(1),
                actual: Some(2),
            },
            cursor,
        );

        for result in [applied, rejected] {
            let encoded = serde_json::to_string(&result).unwrap();
            let decoded: DeviceMutationResult = serde_json::from_str(&encoded).unwrap();
            assert_eq!(decoded, result);
            assert!(decoded.validate_for(&operation.identity).is_ok());
        }
    }

    #[test]
    fn mutation_results_reject_another_operation_identity() {
        let first = prepare_device_document_mutation(
            1,
            "first".into(),
            "device".into(),
            note("document", "#+title: Note\n", None),
        )
        .unwrap();
        let second = prepare_device_document_mutation(
            1,
            "second".into(),
            "device".into(),
            note("document", "#+title: Note\n", None),
        )
        .unwrap();
        let result = DeviceMutationResult::rejected(
            &first.identity,
            DeviceMutationRejection::InvalidContent,
            DeviceMutationCursor {
                server_id: "server".into(),
                position: 0,
                token: "token".into(),
            },
        );

        assert_eq!(
            result.validate_for(&second.identity),
            Err(DeviceMutationError::Result)
        );
    }

    fn note(
        id: &str,
        org: &str,
        expected_revision: Option<u64>,
    ) -> crate::application::PreparedNote {
        prepare_note(
            Some(id.into()),
            "note.org".into(),
            org.into(),
            expected_revision,
        )
        .unwrap()
    }
}
