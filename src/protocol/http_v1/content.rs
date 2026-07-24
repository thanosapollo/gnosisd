use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{DocumentOperationV1, FsrsConfigV1, ThemaKindV1};
use crate::application::{
    AssetHead, ContentChange, DeviceMutationProjection, DeviceMutationRejection,
    DeviceMutationResult, DocumentHead, DocumentRevision, NodeCatalogRecord, NodeRecord,
    NodeThemaAssociation, SettingsInput, SettingsRecord, ThemaMutationProjection, ThemaRecord,
};
use crate::domain::{ThemaConcept, ThemaConceptRelation, ThemaKind};

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// Complete typed source for an explicit document projection.
pub struct DocumentImportRequestV1 {
    pub content_type: String,
    pub source: String,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<u64>", required)]
    pub expected_revision: Option<u64>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentEntityKindV1 {
    Document,
    Asset,
    Thema,
    Settings,
    #[serde(rename = "review")]
    ReviewState,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentOperationV1 {
    Create,
    Update,
    Rename,
    Snapshot,
    Tombstone,
    Archive,
    Unarchive,
    Commit,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContentChangeV1 {
    pub cursor: u64,
    pub entity_kind: ContentEntityKindV1,
    pub entity_id: String,
    pub revision: u64,
    pub operation: ContentOperationV1,
    pub created_at_us: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContentChangePageV1 {
    #[schemars(length(max = 500))]
    pub items: Vec<ContentChangeV1>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<u64>", required)]
    pub next: Option<u64>,
}

impl ContentChangePageV1 {
    pub(crate) fn from_application(items: &[ContentChange]) -> Result<Self, ContentWireError> {
        Ok(Self {
            items: items
                .iter()
                .map(ContentChangeV1::from_application)
                .collect::<Result<_, _>>()?,
            next: items.last().map(|item| item.cursor),
        })
    }
}

impl ContentChangeV1 {
    fn from_application(value: &ContentChange) -> Result<Self, ContentWireError> {
        Ok(Self {
            cursor: value.cursor,
            entity_kind: content_kind(&value.entity_kind)?,
            entity_id: value.entity_id.clone(),
            revision: value.revision,
            operation: content_operation(&value.operation)?,
            created_at_us: value.created_at_us,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoteWriteRequestV1 {
    pub logical_path: String,
    pub content_type: String,
    pub source: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NoteRenameRequestV1 {
    pub logical_path: String,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentHeadV1 {
    pub document_id: String,
    pub logical_path: String,
    pub content_type: String,
    pub revision: u64,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub content_sha256: Option<String>,
    pub active: bool,
}

impl DocumentHeadV1 {
    pub(crate) fn from_application(value: &DocumentHead) -> Self {
        Self {
            document_id: value.document_id.clone(),
            logical_path: value.logical_path.clone(),
            content_type: value.content_type.clone(),
            revision: value.revision,
            content_sha256: value.blob_sha256.clone(),
            active: value.active,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum DocumentMutationRejectionV1 {
    RevisionConflict {
        #[serde(deserialize_with = "super::deserialize_required_option")]
        #[schemars(with = "crate::protocol::RequiredNullable<u64>", required)]
        expected_revision: Option<u64>,
        #[serde(deserialize_with = "super::deserialize_required_option")]
        #[schemars(with = "crate::protocol::RequiredNullable<u64>", required)]
        current_revision: Option<u64>,
    },
    NotFound,
    InvalidContent,
    PathConflict,
    RenameRequired,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum DocumentMutationResultV1 {
    Applied {
        #[schemars(extend("const" = 1))]
        format_version: u32,
        #[schemars(
            length(min = 1, max = 128),
            regex(pattern = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
        )]
        operation_id: String,
        changed: bool,
        authoritative: DocumentHeadV1,
        #[schemars(length(min = 1, max = 512))]
        cursor: String,
    },
    Rejected {
        #[schemars(extend("const" = 1))]
        format_version: u32,
        #[schemars(
            length(min = 1, max = 128),
            regex(pattern = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
        )]
        operation_id: String,
        rejection: DocumentMutationRejectionV1,
        #[schemars(length(min = 1, max = 512))]
        cursor: String,
    },
}

impl DocumentMutationResultV1 {
    pub(crate) fn from_application(value: &DeviceMutationResult) -> Result<Self, ContentWireError> {
        match value {
            DeviceMutationResult::Applied {
                format_version,
                operation_id,
                changed,
                authoritative: DeviceMutationProjection::Document(document),
                cursor,
            } => Ok(Self::Applied {
                format_version: *format_version,
                operation_id: operation_id.clone(),
                changed: *changed,
                authoritative: DocumentHeadV1::from_application(document),
                cursor: mutation_cursor(cursor)?,
            }),
            DeviceMutationResult::Rejected {
                format_version,
                operation_id,
                rejection,
                cursor,
            } => Ok(Self::Rejected {
                format_version: *format_version,
                operation_id: operation_id.clone(),
                rejection: mutation_rejection(rejection),
                cursor: mutation_cursor(cursor)?,
            }),
            DeviceMutationResult::Applied { .. } => Err(ContentWireError),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum DesiredStateMutationProjectionV1 {
    Asset(AssetHeadV1),
    Thema(ThemaV1),
    Settings(SettingsV1),
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
#[allow(
    clippy::large_enum_variant,
    reason = "wire projection remains inline so runtime DTO and OpenAPI expose one shape"
)]
pub enum DesiredStateMutationResultV1 {
    Applied {
        #[schemars(extend("const" = 1))]
        format_version: u32,
        #[schemars(
            length(min = 1, max = 128),
            regex(pattern = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
        )]
        operation_id: String,
        changed: bool,
        authoritative: DesiredStateMutationProjectionV1,
        #[schemars(length(min = 1, max = 512))]
        cursor: String,
    },
    Rejected {
        #[schemars(extend("const" = 1))]
        format_version: u32,
        #[schemars(
            length(min = 1, max = 128),
            regex(pattern = "^[A-Za-z0-9][A-Za-z0-9._:-]{0,127}$")
        )]
        operation_id: String,
        rejection: DocumentMutationRejectionV1,
        #[schemars(length(min = 1, max = 512))]
        cursor: String,
    },
}

impl DesiredStateMutationResultV1 {
    pub(crate) fn from_application(value: &DeviceMutationResult) -> Result<Self, ContentWireError> {
        match value {
            DeviceMutationResult::Applied {
                format_version,
                operation_id,
                changed,
                authoritative,
                cursor,
            } => Ok(Self::Applied {
                format_version: *format_version,
                operation_id: operation_id.clone(),
                changed: *changed,
                authoritative: desired_state_projection(authoritative)?,
                cursor: mutation_cursor(cursor)?,
            }),
            DeviceMutationResult::Rejected {
                format_version,
                operation_id,
                rejection,
                cursor,
            } => Ok(Self::Rejected {
                format_version: *format_version,
                operation_id: operation_id.clone(),
                rejection: mutation_rejection(rejection),
                cursor: mutation_cursor(cursor)?,
            }),
        }
    }
}

fn desired_state_projection(
    value: &DeviceMutationProjection,
) -> Result<DesiredStateMutationProjectionV1, ContentWireError> {
    match value {
        DeviceMutationProjection::Asset(asset) => Ok(DesiredStateMutationProjectionV1::Asset(
            AssetHeadV1::from_application(asset),
        )),
        DeviceMutationProjection::Thema(thema) => Ok(DesiredStateMutationProjectionV1::Thema(
            ThemaV1::from_mutation_projection(thema)?,
        )),
        DeviceMutationProjection::Settings(settings) => Ok(
            DesiredStateMutationProjectionV1::Settings(SettingsV1::from_application(settings)),
        ),
        DeviceMutationProjection::Document(_) => Err(ContentWireError),
    }
}

fn mutation_cursor(
    cursor: &crate::application::DeviceMutationCursor,
) -> Result<String, ContentWireError> {
    crate::protocol::encode_cursor(&cursor.server_id, cursor.position, &cursor.token)
        .map_err(|_| ContentWireError)
}

fn mutation_rejection(value: &DeviceMutationRejection) -> DocumentMutationRejectionV1 {
    match value {
        DeviceMutationRejection::RevisionConflict { expected, actual } => {
            DocumentMutationRejectionV1::RevisionConflict {
                expected_revision: *expected,
                current_revision: *actual,
            }
        }
        DeviceMutationRejection::NotFound => DocumentMutationRejectionV1::NotFound,
        DeviceMutationRejection::InvalidContent => DocumentMutationRejectionV1::InvalidContent,
        DeviceMutationRejection::PathConflict => DocumentMutationRejectionV1::PathConflict,
        DeviceMutationRejection::RenameRequired => DocumentMutationRejectionV1::RenameRequired,
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentRevisionSummaryV1 {
    pub document_id: String,
    pub revision: u64,
    pub logical_path: String,
    pub content_type: String,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub content_sha256: Option<String>,
    pub operation: DocumentOperationV1,
    pub created_at_us: u64,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub author_device_id: Option<String>,
}

impl DocumentRevisionSummaryV1 {
    pub(crate) fn from_application(value: &DocumentRevision) -> Result<Self, ContentWireError> {
        Ok(Self {
            document_id: value.document_id.clone(),
            revision: value.revision,
            logical_path: value.logical_path.clone(),
            content_type: value.content_type.clone(),
            content_sha256: value.blob_sha256.clone(),
            operation: document_operation(&value.operation)?,
            created_at_us: value.created_at_us,
            author_device_id: value.author_device_id.clone(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DocumentRevisionPageV1 {
    #[schemars(length(max = 500))]
    pub items: Vec<DocumentRevisionSummaryV1>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<u64>", required)]
    pub next: Option<u64>,
}

impl DocumentRevisionPageV1 {
    pub(crate) fn from_application(items: &[DocumentRevision]) -> Result<Self, ContentWireError> {
        Ok(Self {
            items: items
                .iter()
                .map(DocumentRevisionSummaryV1::from_application)
                .collect::<Result<_, _>>()?,
            next: items.last().map(|item| item.revision),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetHeadV1 {
    pub logical_path: String,
    pub revision: u64,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub content_sha256: Option<String>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub media_type: Option<String>,
    pub active: bool,
}

impl AssetHeadV1 {
    pub(crate) fn from_application(value: &AssetHead) -> Self {
        Self {
            logical_path: value.logical_path.clone(),
            revision: value.revision,
            content_sha256: value.blob_sha256.clone(),
            media_type: value.media_type.clone(),
            active: value.active,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StagedBlobV1 {
    pub content_sha256: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemaWriteRequestV1 {
    #[schemars(regex(pattern = "^(basic|cloze|mcq|mc-cloze)$"))]
    pub kind: String,
    pub keimenon: String,
    #[serde(default)]
    #[schemars(default)]
    pub hypothesis: Vec<Option<String>>,
    pub answer: Vec<String>,
    #[schemars(default)]
    pub source_guid: Option<String>,
    #[schemars(default)]
    pub parathema: Option<String>,
    #[schemars(default)]
    pub review_image: Option<String>,
    #[serde(default)]
    #[schemars(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    #[schemars(default)]
    pub concepts: Vec<ThemaConceptV1>,
    #[serde(default)]
    #[schemars(default)]
    pub asset_paths: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemaV1 {
    pub thema_id: String,
    pub revision: u64,
    pub kind: ThemaKindV1,
    pub keimenon: String,
    pub hypothesis: Vec<Option<String>>,
    pub answer: Vec<String>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub source_guid: Option<String>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub parathema: Option<String>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub review_image: Option<String>,
    pub tags: Vec<String>,
    pub concepts: Vec<ThemaConceptV1>,
    pub asset_paths: Vec<String>,
    pub content_sha256: String,
    pub archived: bool,
}

impl ThemaV1 {
    pub(crate) fn from_application(value: &ThemaRecord) -> Self {
        Self {
            thema_id: value.thema_id.clone(),
            revision: value.revision,
            kind: thema_kind(value.content.kind),
            keimenon: value.content.keimenon.clone(),
            hypothesis: value.content.hypothesis.clone(),
            answer: value.content.answer.clone(),
            source_guid: value.content.source_guid.clone(),
            parathema: value.content.parathema.clone(),
            review_image: value.content.review_image.clone(),
            tags: value.tags.clone(),
            concepts: value.concepts.iter().map(ThemaConceptV1::from).collect(),
            asset_paths: value.asset_paths.clone(),
            content_sha256: value.content_sha256.clone(),
            archived: value.archived,
        }
    }

    fn from_mutation_projection(value: &ThemaMutationProjection) -> Result<Self, ContentWireError> {
        let kind = match value.kind.as_str() {
            "basic" => ThemaKindV1::Basic,
            "cloze" => ThemaKindV1::Cloze,
            "mcq" => ThemaKindV1::Mcq,
            "mc-cloze" => ThemaKindV1::McCloze,
            _ => return Err(ContentWireError),
        };
        let concepts = value
            .concepts
            .iter()
            .map(|(node_id, relation)| {
                let relation = match relation.as_str() {
                    "primary" => ThemaConceptRelationV1::Primary,
                    "supporting" => ThemaConceptRelationV1::Supporting,
                    "reference" => ThemaConceptRelationV1::Reference,
                    _ => return Err(ContentWireError),
                };
                Ok(ThemaConceptV1 {
                    node_id: node_id.clone(),
                    relation,
                })
            })
            .collect::<Result<_, _>>()?;
        Ok(Self {
            thema_id: value.thema_id.clone(),
            revision: value.revision,
            kind,
            keimenon: value.keimenon.clone(),
            hypothesis: value.hypothesis.clone(),
            answer: value.answer.clone(),
            source_guid: value.source_guid.clone(),
            parathema: value.parathema.clone(),
            review_image: value.review_image.clone(),
            tags: value.tags.clone(),
            concepts,
            asset_paths: value.asset_paths.clone(),
            content_sha256: value.content_sha256.clone(),
            archived: value.archived,
        })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemaConceptV1 {
    pub node_id: String,
    pub relation: ThemaConceptRelationV1,
}

impl From<&ThemaConcept> for ThemaConceptV1 {
    fn from(value: &ThemaConcept) -> Self {
        Self {
            node_id: value.concept_id().to_string(),
            relation: value.relation().into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemaConceptRelationV1 {
    Primary,
    Supporting,
    Reference,
}

impl From<ThemaConceptRelationV1> for ThemaConceptRelation {
    fn from(value: ThemaConceptRelationV1) -> Self {
        match value {
            ThemaConceptRelationV1::Primary => Self::Primary,
            ThemaConceptRelationV1::Supporting => Self::Supporting,
            ThemaConceptRelationV1::Reference => Self::Reference,
        }
    }
}

impl From<ThemaConceptRelation> for ThemaConceptRelationV1 {
    fn from(value: ThemaConceptRelation) -> Self {
        match value {
            ThemaConceptRelation::Primary => Self::Primary,
            ThemaConceptRelation::Supporting => Self::Supporting,
            ThemaConceptRelation::Reference => Self::Reference,
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ThemaPageV1 {
    #[schemars(length(max = 500))]
    pub items: Vec<ThemaV1>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub next: Option<String>,
}

impl ThemaPageV1 {
    pub(crate) fn from_application(items: &[ThemaRecord]) -> Self {
        Self {
            items: items.iter().map(ThemaV1::from_application).collect(),
            next: items.last().map(|item| item.thema_id.clone()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeV1 {
    pub node_id: String,
    pub document_id: String,
    pub logical_path: String,
    #[schemars(range(min = 1))]
    pub document_revision: u64,
    #[schemars(length(equal = 64))]
    #[schemars(regex(pattern = "^[0-9a-f]{64}$"))]
    pub content_sha256: String,
    pub title: String,
    pub title_path: Vec<String>,
    pub tags: Vec<String>,
    pub level: u32,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<usize>", required)]
    pub start_byte: Option<usize>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<usize>", required)]
    pub end_byte: Option<usize>,
    pub active: bool,
}

impl NodeV1 {
    pub(crate) fn from_application(value: &NodeRecord) -> Self {
        Self {
            node_id: value.node_id.clone(),
            document_id: value.document_id.clone(),
            logical_path: value.logical_path.clone(),
            document_revision: value.document_revision,
            content_sha256: value.content_sha256.clone(),
            title: value.title.clone(),
            title_path: value.title_path.clone(),
            tags: value.tags.clone(),
            level: value.level,
            start_byte: value.start_byte,
            end_byte: value.end_byte,
            active: value.active,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCatalogItemV1 {
    pub node_id: String,
    pub document_id: String,
    pub logical_path: String,
    pub title_path: Vec<String>,
    pub active: bool,
}

impl NodeCatalogItemV1 {
    fn from_application(value: &NodeCatalogRecord) -> Self {
        Self {
            node_id: value.node_id.clone(),
            document_id: value.document_id.clone(),
            logical_path: value.logical_path.clone(),
            title_path: value.title_path.clone(),
            active: value.active,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeCatalogPageV1 {
    #[schemars(length(min = 1, max = 512))]
    pub generation: String,
    #[schemars(length(max = 5000))]
    pub items: Vec<NodeCatalogItemV1>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub next: Option<String>,
}

impl NodeCatalogPageV1 {
    pub(crate) fn from_application(items: &[NodeCatalogRecord], generation: String) -> Self {
        Self {
            generation,
            items: items
                .iter()
                .map(NodeCatalogItemV1::from_application)
                .collect(),
            next: items.last().map(|item| item.node_id.clone()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeThemaAssociationV1 {
    pub thema_id: String,
    pub relation: ThemaConceptRelationV1,
}

impl NodeThemaAssociationV1 {
    fn from_application(value: &NodeThemaAssociation) -> Self {
        Self {
            thema_id: value.thema_id.clone(),
            relation: value.relation.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeThemaAssociationPageV1 {
    #[schemars(length(max = 500))]
    pub items: Vec<NodeThemaAssociationV1>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub next: Option<String>,
}

impl NodeThemaAssociationPageV1 {
    pub(crate) fn from_application(items: &[NodeThemaAssociation]) -> Self {
        Self {
            items: items
                .iter()
                .map(NodeThemaAssociationV1::from_application)
                .collect(),
            next: items.last().map(|item| item.thema_id.clone()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodePageV1 {
    #[schemars(length(max = 500))]
    pub items: Vec<NodeV1>,
    #[serde(deserialize_with = "super::deserialize_required_option")]
    #[schemars(with = "crate::protocol::RequiredNullable<String>", required)]
    pub next: Option<String>,
}

impl NodePageV1 {
    pub(crate) fn from_application(items: &[NodeRecord]) -> Self {
        Self {
            items: items.iter().map(NodeV1::from_application).collect(),
            next: items.last().map(|item| item.node_id.clone()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsV1 {
    pub revision: u64,
    pub timezone: String,
    #[schemars(range(max = 23))]
    pub day_boundary_hour: u8,
    pub scheduler: FsrsConfigV1,
}

impl SettingsV1 {
    pub(crate) fn from_application(value: &SettingsRecord) -> Self {
        Self {
            revision: value.revision,
            timezone: value.timezone.clone(),
            day_boundary_hour: value.day_boundary_hour,
            scheduler: FsrsConfigV1::from(&value.scheduler),
        }
    }
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateSettingsRequestV1 {
    pub timezone: String,
    #[schemars(range(max = 23))]
    pub day_boundary_hour: u8,
    pub desired_retention: f32,
}

impl UpdateSettingsRequestV1 {
    pub(crate) fn into_application(self) -> SettingsInput {
        SettingsInput {
            timezone: self.timezone,
            day_boundary_hour: self.day_boundary_hour,
            desired_retention: self.desired_retention,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ContentWireError;

fn content_kind(value: &str) -> Result<ContentEntityKindV1, ContentWireError> {
    match value {
        "document" => Ok(ContentEntityKindV1::Document),
        "asset" => Ok(ContentEntityKindV1::Asset),
        "thema" => Ok(ContentEntityKindV1::Thema),
        "settings" => Ok(ContentEntityKindV1::Settings),
        "review" => Ok(ContentEntityKindV1::ReviewState),
        _ => Err(ContentWireError),
    }
}

fn content_operation(value: &str) -> Result<ContentOperationV1, ContentWireError> {
    match value {
        "create" => Ok(ContentOperationV1::Create),
        "update" => Ok(ContentOperationV1::Update),
        "rename" => Ok(ContentOperationV1::Rename),
        "snapshot" => Ok(ContentOperationV1::Snapshot),
        "tombstone" => Ok(ContentOperationV1::Tombstone),
        "archive" => Ok(ContentOperationV1::Archive),
        "unarchive" => Ok(ContentOperationV1::Unarchive),
        "commit" => Ok(ContentOperationV1::Commit),
        _ => Err(ContentWireError),
    }
}

fn document_operation(value: &str) -> Result<DocumentOperationV1, ContentWireError> {
    match value {
        "create" => Ok(DocumentOperationV1::Create),
        "update" => Ok(DocumentOperationV1::Update),
        "rename" => Ok(DocumentOperationV1::Rename),
        "snapshot" => Ok(DocumentOperationV1::Snapshot),
        "tombstone" => Ok(DocumentOperationV1::Tombstone),
        _ => Err(ContentWireError),
    }
}

const fn thema_kind(value: ThemaKind) -> ThemaKindV1 {
    match value {
        ThemaKind::Basic => ThemaKindV1::Basic,
        ThemaKind::Cloze => ThemaKindV1::Cloze,
        ThemaKind::Mcq => ThemaKindV1::Mcq,
        ThemaKind::McCloze => ThemaKindV1::McCloze,
    }
}
