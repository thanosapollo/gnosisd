use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::application::{
    AnswerInput, ParametersSnapshot, PresentationSnapshot, ReviewAttemptView, ReviewSessionView,
    ReviewSource, ReviewSourceKind, ReviewStateSnapshot, TransitionSnapshot,
};
use crate::domain::{
    DocumentId, DocumentProjection, DueThema, LegacyDatabaseInfo, LegacyReviewState, LegacyThema,
};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_SIZE: usize = 1024 * 1024;
pub const MAX_REQUEST_ID_LEN: usize = 128;
pub const MAX_REVIEW_ID_LEN: usize = 128;
pub const MAX_ASSET_CHUNK_BYTES: u32 = 512 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RequestEnvelope {
    pub version: u16,
    pub id: String,
    #[serde(flatten)]
    pub request: Request,
}

impl RequestEnvelope {
    pub(crate) fn validate(&self) -> Result<(), MessageValidationError> {
        if self.version != PROTOCOL_VERSION {
            return Err(MessageValidationError::UnsupportedVersion(self.version));
        }
        if self.id.is_empty() || self.id.len() > MAX_REQUEST_ID_LEN {
            return Err(MessageValidationError::InvalidRequestId);
        }
        self.request.validate()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "method", content = "params", deny_unknown_fields)]
pub enum Request {
    #[serde(rename = "system.info")]
    SystemInfo {},
    #[serde(rename = "document.import")]
    DocumentImport {
        document_id: String,
        org: String,
        expected_revision: Option<u64>,
    },
    #[serde(rename = "document.get")]
    DocumentGet { document_id: String },
    #[serde(rename = "legacy.info")]
    LegacyInfo {},
    #[serde(rename = "thema.get")]
    ThemaGet { thema_id: String },
    #[serde(rename = "review.due")]
    ReviewDue { day: u32, limit: u16 },
    #[serde(rename = "review.due_page")]
    ReviewDuePage {
        day: u32,
        limit: u16,
        after: Option<String>,
    },
    #[serde(rename = "review.session_create")]
    ReviewSessionCreate {
        source: ReviewSourceInput,
        target_count: Option<u32>,
    },
    #[serde(rename = "review.session_active")]
    ReviewSessionActive {},
    #[serde(rename = "review.session_resume")]
    ReviewSessionResume { session_id: String },
    #[serde(rename = "review.session_quit")]
    ReviewSessionQuit { session_id: String },
    #[serde(rename = "review.session_abort")]
    ReviewSessionAbort { session_id: String },
    #[serde(rename = "review.attempt_begin")]
    ReviewAttemptBegin { session_id: String },
    #[serde(rename = "review.attempt_answer")]
    ReviewAttemptAnswer {
        attempt_id: String,
        response: ReviewAnswerInput,
    },
    #[serde(rename = "review.attempt_override")]
    ReviewAttemptOverride { attempt_id: String },
    #[serde(rename = "review.attempt_suspension_toggle")]
    ReviewAttemptSuspensionToggle { attempt_id: String },
    #[serde(rename = "review.attempt_commit")]
    ReviewAttemptCommit { attempt_id: String },
    #[serde(rename = "asset.chunk")]
    AssetChunk {
        logical_path: String,
        offset: u64,
        limit: u32,
    },
    #[serde(rename = "admin.device_create")]
    DeviceCreate { name: String, scopes: Vec<String> },
    #[serde(rename = "admin.device_list")]
    DeviceList {},
    #[serde(rename = "admin.device_revoke")]
    DeviceRevoke { device_id: String },
    #[serde(rename = "admin.backup_create")]
    BackupCreate { destination: String },
    #[serde(rename = "admin.backup_verify")]
    BackupVerify { path: String },
    #[serde(rename = "admin.backup_prune")]
    BackupPrune { directory: String, keep: u16 },
    #[serde(rename = "admin.backup_restore_validate")]
    BackupRestoreValidate {
        path: String,
        temporary_directory: String,
    },
}

impl Request {
    fn validate(&self) -> Result<(), MessageValidationError> {
        match self {
            Self::ReviewSessionCreate {
                source,
                target_count,
            } => {
                ReviewSource::try_from(source.clone())
                    .map_err(|_| MessageValidationError::InvalidReviewRequest)?;
                if *target_count == Some(0) {
                    return Err(MessageValidationError::InvalidReviewRequest);
                }
            }
            Self::ReviewSessionResume { session_id }
            | Self::ReviewSessionQuit { session_id }
            | Self::ReviewSessionAbort { session_id }
            | Self::ReviewAttemptBegin { session_id } => require_review_id(session_id)?,
            Self::ReviewAttemptAnswer { attempt_id, .. }
            | Self::ReviewAttemptOverride { attempt_id }
            | Self::ReviewAttemptSuspensionToggle { attempt_id }
            | Self::ReviewAttemptCommit { attempt_id } => require_review_id(attempt_id)?,
            Self::AssetChunk { limit, .. } if *limit == 0 || *limit > MAX_ASSET_CHUNK_BYTES => {
                return Err(MessageValidationError::InvalidAssetChunk);
            }
            _ => {}
        }
        Ok(())
    }
}

fn require_review_id(id: &str) -> Result<(), MessageValidationError> {
    if id.is_empty() || id.len() > MAX_REVIEW_ID_LEN {
        Err(MessageValidationError::InvalidReviewRequest)
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSourceKindInput {
    Due,
    Overdue,
    DueWithoutOverdue,
    Tag,
    DueTag,
    Topic,
    Manual,
}

impl From<ReviewSourceKindInput> for ReviewSourceKind {
    fn from(value: ReviewSourceKindInput) -> Self {
        match value {
            ReviewSourceKindInput::Due => Self::Due,
            ReviewSourceKindInput::Overdue => Self::Overdue,
            ReviewSourceKindInput::DueWithoutOverdue => Self::DueWithoutOverdue,
            ReviewSourceKindInput::Tag => Self::Tag,
            ReviewSourceKindInput::DueTag => Self::DueTag,
            ReviewSourceKindInput::Topic => Self::Topic,
            ReviewSourceKindInput::Manual => Self::Manual,
        }
    }
}

impl From<ReviewSourceKind> for ReviewSourceKindInput {
    fn from(value: ReviewSourceKind) -> Self {
        match value {
            ReviewSourceKind::Due => Self::Due,
            ReviewSourceKind::Overdue => Self::Overdue,
            ReviewSourceKind::DueWithoutOverdue => Self::DueWithoutOverdue,
            ReviewSourceKind::Tag => Self::Tag,
            ReviewSourceKind::DueTag => Self::DueTag,
            ReviewSourceKind::Topic => Self::Topic,
            ReviewSourceKind::Manual => Self::Manual,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewSourceInput {
    pub kind: ReviewSourceKindInput,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub exclude_tags: Vec<String>,
    pub topic_node_id: Option<String>,
    pub topic_forward_depth: Option<u32>,
    pub topic_backward_depth: Option<u32>,
    #[serde(default)]
    pub manual_ids: Vec<String>,
}

impl TryFrom<ReviewSourceInput> for ReviewSource {
    type Error = crate::application::ReviewError;

    fn try_from(value: ReviewSourceInput) -> Result<Self, Self::Error> {
        let ids_are_bounded = value
            .topic_node_id
            .iter()
            .chain(&value.manual_ids)
            .all(|id| !id.is_empty() && id.len() <= MAX_REVIEW_ID_LEN);
        if !ids_are_bounded {
            return Err(crate::application::ReviewError::Source);
        }
        Self {
            kind: value.kind.into(),
            tags: value.tags,
            exclude_tags: value.exclude_tags,
            topic_node_id: value.topic_node_id,
            topic_forward_depth: value.topic_forward_depth,
            topic_backward_depth: value.topic_backward_depth,
            manual_ids: value.manual_ids,
        }
        .validate()
    }
}

impl From<ReviewSource> for ReviewSourceInput {
    fn from(value: ReviewSource) -> Self {
        Self {
            kind: value.kind.into(),
            tags: value.tags,
            exclude_tags: value.exclude_tags,
            topic_node_id: value.topic_node_id,
            topic_forward_depth: value.topic_forward_depth,
            topic_backward_depth: value.topic_backward_depth,
            manual_ids: value.manual_ids,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ReviewAnswerInput {
    Text(String),
    Texts(Vec<String>),
    Choice(String),
}

impl From<ReviewAnswerInput> for AnswerInput {
    fn from(value: ReviewAnswerInput) -> Self {
        match value {
            ReviewAnswerInput::Text(value) => Self::Text(value),
            ReviewAnswerInput::Texts(value) => Self::Texts(value),
            ReviewAnswerInput::Choice(value) => Self::Choice(value),
        }
    }
}

impl From<AnswerInput> for ReviewAnswerInput {
    fn from(value: AnswerInput) -> Self {
        match value {
            AnswerInput::Text(value) => Self::Text(value),
            AnswerInput::Texts(value) => Self::Texts(value),
            AnswerInput::Choice(value) => Self::Choice(value),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResponseEnvelope {
    pub version: u16,
    pub id: String,
    #[serde(flatten)]
    pub outcome: ResponseOutcome,
}

impl ResponseEnvelope {
    pub fn success(id: String, result: ResponseResult) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            outcome: ResponseOutcome::Success {
                result: Box::new(result),
            },
        }
    }

    pub fn error(id: String, error: ProtocolError) -> Self {
        Self {
            version: PROTOCOL_VERSION,
            id,
            outcome: ResponseOutcome::Failure { error },
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResponseOutcome {
    Success { result: Box<ResponseResult> },
    Failure { error: ProtocolError },
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum ResponseResult {
    SystemInfo(SystemInfo),
    DocumentImported(ImportResult),
    Document(DocumentView),
    LegacyInfo(LegacyInfoView),
    Thema(ThemaView),
    DueThemata(Vec<DueThemaView>),
    DuePage(DuePageView),
    ReviewSession(LocalReviewSessionView),
    ActiveReviewSession(ActiveReviewSessionView),
    ReviewAttempt(Box<LocalReviewAttemptView>),
    ReviewEvent(ReviewEventView),
    AssetChunk(AssetChunkView),
    DeviceCreated(DeviceCreatedView),
    Devices(Vec<DeviceView>),
    Device(DeviceView),
    Backup(BackupView),
    BackupsPruned(BackupsPrunedView),
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalReviewSessionView {
    pub session_id: String,
    pub source: ReviewSourceInput,
    pub target_count: Option<u32>,
    pub status: String,
    pub current_position: u32,
    pub queue_length: u32,
    pub started_at_us: u64,
}

impl From<ReviewSessionView> for LocalReviewSessionView {
    fn from(value: ReviewSessionView) -> Self {
        Self {
            session_id: value.session_id,
            source: value.source.into(),
            target_count: value.target_count,
            status: value.status,
            current_position: value.current_position,
            queue_length: value.queue_length,
            started_at_us: value.started_at_us,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveReviewSessionView {
    pub session: Option<LocalReviewSessionView>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalPresentationView {
    pub kind: String,
    pub keimenon: String,
    pub hypothesis: Vec<Option<String>>,
    pub answer: Vec<String>,
    pub source_guid: Option<String>,
    pub parathema: Option<String>,
    pub review_image: Option<String>,
    pub tags: Vec<String>,
}

impl From<PresentationSnapshot> for LocalPresentationView {
    fn from(value: PresentationSnapshot) -> Self {
        Self {
            kind: value.kind,
            keimenon: value.keimenon,
            hypothesis: value.hypothesis,
            answer: value.answer,
            source_guid: value.source_guid,
            parathema: value.parathema,
            review_image: value.review_image,
            tags: value.tags,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalReviewStateView {
    pub state_revision: u64,
    pub review_count: u64,
    pub legacy_review_count: u64,
    pub legacy_unclassified_count: u64,
    pub last_reviewed_at_us: Option<u64>,
    pub last_review_day: Option<u32>,
    pub next_review_day: u32,
    pub interval_days: Option<u32>,
    pub consecutive_successes: u64,
    pub total_successes: u64,
    pub consecutive_failures: u64,
    pub total_failures: u64,
    pub suspended: bool,
    pub gnosis_plus: f64,
    pub gnosis_minus: f64,
    pub gnosis_synolon: f64,
}

impl From<ReviewStateSnapshot> for LocalReviewStateView {
    fn from(value: ReviewStateSnapshot) -> Self {
        Self {
            state_revision: value.state_revision,
            review_count: value.review_count,
            legacy_review_count: value.legacy_review_count,
            legacy_unclassified_count: value.legacy_unclassified_count,
            last_reviewed_at_us: value.last_reviewed_at_us,
            last_review_day: value.last_review_day,
            next_review_day: value.next_review_day,
            interval_days: value.interval_days,
            consecutive_successes: value.consecutive_successes,
            total_successes: value.total_successes,
            consecutive_failures: value.consecutive_failures,
            total_failures: value.total_failures,
            suspended: value.suspended,
            gnosis_plus: value.gnosis_plus,
            gnosis_minus: value.gnosis_minus,
            gnosis_synolon: value.gnosis_synolon,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalParametersView {
    pub scheduler_version: u32,
    pub amnesia: f64,
    pub epignosis: f64,
    pub agnoia: f64,
    pub anagnosis: u64,
    pub lethe: u64,
    pub synolon_max: f64,
    pub interval_fuzz: f64,
    pub proto: Vec<u32>,
}

impl From<ParametersSnapshot> for LocalParametersView {
    fn from(value: ParametersSnapshot) -> Self {
        Self {
            scheduler_version: value.scheduler_version,
            amnesia: value.amnesia,
            epignosis: value.epignosis,
            agnoia: value.agnoia,
            anagnosis: value.anagnosis,
            lethe: value.lethe,
            synolon_max: value.synolon_max,
            interval_fuzz: value.interval_fuzz,
            proto: value.proto,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalTransitionView {
    pub before: LocalReviewStateView,
    pub after: LocalReviewStateView,
    pub parameters: LocalParametersView,
    pub reviewed_at_us: u64,
    pub review_day: u32,
    pub success: bool,
    pub first_review: bool,
    pub elapsed_days: u32,
    pub algorithm_interval_days: f64,
    pub computed_interval_days: u32,
    pub fuzz_delta: f64,
}

impl From<TransitionSnapshot> for LocalTransitionView {
    fn from(value: TransitionSnapshot) -> Self {
        Self {
            before: value.before.into(),
            after: value.after.into(),
            parameters: value.parameters.into(),
            reviewed_at_us: value.reviewed_at_us,
            review_day: value.review_day,
            success: value.success,
            first_review: value.first_review,
            elapsed_days: value.elapsed_days,
            algorithm_interval_days: value.algorithm_interval_days,
            computed_interval_days: value.computed_interval_days,
            fuzz_delta: value.fuzz_delta,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LocalReviewAttemptView {
    pub attempt_id: String,
    pub session_id: String,
    pub thema_id: String,
    pub presentation: LocalPresentationView,
    pub response: Option<ReviewAnswerInput>,
    pub detected_success: Option<bool>,
    pub accepted_success: Option<bool>,
    pub override_count: u32,
    pub proposed_transition: Option<LocalTransitionView>,
    pub desired_suspended: bool,
    pub status: String,
}

impl From<ReviewAttemptView> for LocalReviewAttemptView {
    fn from(value: ReviewAttemptView) -> Self {
        Self {
            attempt_id: value.attempt_id,
            session_id: value.session_id,
            thema_id: value.thema_id,
            presentation: value.presentation.into(),
            response: value.response.map(ReviewAnswerInput::from),
            detected_success: value.detected_success,
            accepted_success: value.accepted_success,
            override_count: value.override_count,
            proposed_transition: value.proposed_transition.map(LocalTransitionView::from),
            desired_suspended: value.desired_suspended,
            status: value.status,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewEventView {
    pub event_id: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AssetChunkView {
    pub logical_path: String,
    pub content_sha256: String,
    pub total_size: u64,
    pub offset: u64,
    pub chunk_base64: String,
    pub eof: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackupsPrunedView {
    pub removed: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BackupView {
    pub path: String,
    pub created_at_us: u64,
    pub database_sha256: String,
    pub blob_count: u64,
}

impl From<crate::application::BackupSummary> for BackupView {
    fn from(summary: crate::application::BackupSummary) -> Self {
        Self {
            path: summary.path.to_string_lossy().into_owned(),
            created_at_us: summary.created_at_us,
            database_sha256: summary.database_sha256,
            blob_count: summary.blob_count,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceCreatedView {
    pub device: DeviceView,
    pub token: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeviceView {
    pub device_id: String,
    pub name: String,
    pub scopes: Vec<String>,
    pub created_at_us: u64,
    pub last_used_at_us: Option<u64>,
    pub revoked_at_us: Option<u64>,
}

impl From<crate::application::DeviceRecord> for DeviceView {
    fn from(device: crate::application::DeviceRecord) -> Self {
        Self {
            device_id: device.device_id,
            name: device.name,
            scopes: device
                .scopes
                .into_iter()
                .map(|scope| scope.as_str().to_owned())
                .collect(),
            created_at_us: device.created_at_us,
            last_used_at_us: device.last_used_at_us,
            revoked_at_us: device.revoked_at_us,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SystemInfo {
    pub protocol_version: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LegacyInfoView {
    pub imported: bool,
    pub source_schema_version: Option<u32>,
    pub legacy_table_count: u64,
    pub legacy_row_count: u64,
    pub thema_count: u64,
    pub active_thema_count: u64,
    pub review_state_count: u64,
}

impl From<Option<LegacyDatabaseInfo>> for LegacyInfoView {
    fn from(info: Option<LegacyDatabaseInfo>) -> Self {
        let Some(info) = info else {
            return Self {
                imported: false,
                source_schema_version: None,
                legacy_table_count: 0,
                legacy_row_count: 0,
                thema_count: 0,
                active_thema_count: 0,
                review_state_count: 0,
            };
        };
        Self {
            imported: true,
            source_schema_version: Some(info.source_schema_version),
            legacy_table_count: info.legacy_table_count,
            legacy_row_count: info.legacy_row_count,
            thema_count: info.thema_count,
            active_thema_count: info.active_thema_count,
            review_state_count: info.review_state_count,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ThemaView {
    pub id: String,
    pub kind: String,
    pub keimenon: String,
    pub hypothesis: Vec<Option<String>>,
    pub answer: Vec<String>,
    pub source_guid: Option<String>,
    pub archived_at_us: Option<u64>,
    pub parathema: Option<String>,
    pub review_image: Option<String>,
    pub tags: Vec<String>,
    pub links: Vec<String>,
    pub review_state: ReviewStateView,
}

impl From<LegacyThema> for ThemaView {
    fn from(thema: LegacyThema) -> Self {
        Self {
            id: thema.id.to_string(),
            kind: thema.kind,
            keimenon: thema.keimenon,
            hypothesis: thema.hypothesis,
            answer: thema.answer,
            source_guid: thema.source_guid,
            archived_at_us: thema.archived_at_us,
            parathema: thema.parathema,
            review_image: thema.review_image,
            tags: thema.tags,
            links: thema.links.into_iter().map(|id| id.to_string()).collect(),
            review_state: thema.review_state.into(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ReviewStateView {
    pub state_revision: u64,
    pub review_count: u64,
    pub legacy_review_count: u64,
    pub legacy_unclassified_count: u64,
    pub last_reviewed_at_us: Option<u64>,
    pub last_review_day: Option<u32>,
    pub next_review_day: u32,
    pub interval_days: Option<u32>,
    pub consecutive_successes: u64,
    pub total_successes: u64,
    pub consecutive_failures: u64,
    pub total_failures: u64,
    pub suspended: bool,
    pub gnosis_plus: f64,
    pub gnosis_minus: f64,
    pub gnosis_synolon: f64,
}

impl From<LegacyReviewState> for ReviewStateView {
    fn from(state: LegacyReviewState) -> Self {
        Self {
            state_revision: state.state_revision,
            review_count: state.review_count,
            legacy_review_count: state.legacy_review_count,
            legacy_unclassified_count: state.legacy_unclassified_count,
            last_reviewed_at_us: state.last_reviewed_at_us,
            last_review_day: state.last_review_day,
            next_review_day: state.next_review_day,
            interval_days: state.interval_days,
            consecutive_successes: state.consecutive_successes,
            total_successes: state.total_successes,
            consecutive_failures: state.consecutive_failures,
            total_failures: state.total_failures,
            suspended: state.suspended,
            gnosis_plus: state.gnosis_plus,
            gnosis_minus: state.gnosis_minus,
            gnosis_synolon: state.gnosis_synolon,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DueThemaView {
    pub id: String,
    pub kind: String,
    pub keimenon: String,
    pub hypothesis: Vec<Option<String>>,
    pub answer: Vec<String>,
    pub next_review_day: u32,
    pub review_count: u64,
}

impl From<DueThema> for DueThemaView {
    fn from(thema: DueThema) -> Self {
        Self {
            id: thema.id.to_string(),
            kind: thema.kind,
            keimenon: thema.keimenon,
            hypothesis: thema.hypothesis,
            answer: thema.answer,
            next_review_day: thema.next_review_day,
            review_count: thema.review_count,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DuePageView {
    pub items: Vec<DueThemaView>,
    pub next: Option<String>,
}

impl DuePageView {
    pub(crate) fn new(items: Vec<DueThema>, next: Option<String>) -> Self {
        Self {
            items: items.into_iter().map(DueThemaView::from).collect(),
            next,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ImportResult {
    pub document_id: String,
    pub revision: u64,
    pub content_sha256: String,
    pub node_count: usize,
    pub occurrence_count: usize,
    pub semantic_edge_count: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DocumentView {
    pub document_id: String,
    pub revision: u64,
    pub title: String,
    pub file_node_id: Option<String>,
    pub file_tags: Vec<String>,
    pub nodes: Vec<NodeView>,
    pub link_occurrences: Vec<LinkOccurrenceView>,
    pub semantic_edges: Vec<NodeEdgeView>,
    pub compatibility_sha1: String,
    pub content_sha256: String,
}

impl DocumentView {
    pub(crate) fn from_projection(
        document_id: &DocumentId,
        revision: u64,
        projection: &DocumentProjection,
    ) -> Self {
        Self {
            document_id: document_id.to_string(),
            revision,
            title: projection.title().to_owned(),
            file_node_id: projection.file_node_id().map(ToString::to_string),
            file_tags: projection.file_tags().to_vec(),
            nodes: projection.nodes().iter().map(NodeView::from).collect(),
            link_occurrences: projection
                .link_occurrences()
                .iter()
                .map(LinkOccurrenceView::from)
                .collect(),
            semantic_edges: projection
                .semantic_edges()
                .iter()
                .map(NodeEdgeView::from)
                .collect(),
            compatibility_sha1: projection.compatibility_sha1().to_owned(),
            content_sha256: projection.content_sha256().to_owned(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeView {
    pub id: String,
    pub title: String,
    pub title_path: Vec<String>,
    pub tags: Vec<String>,
    pub parent_id: Option<String>,
    pub level: u32,
}

impl From<&crate::domain::Node> for NodeView {
    fn from(node: &crate::domain::Node) -> Self {
        Self {
            id: node.id().to_string(),
            title: node.title().to_owned(),
            title_path: node.title_path().parts().to_vec(),
            tags: node.tags().to_vec(),
            parent_id: node.parent_id().map(ToString::to_string),
            level: node.level(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LinkOccurrenceView {
    pub owner_id: String,
    pub target_id: String,
    pub start_byte: usize,
    pub end_byte: usize,
}

impl From<&crate::domain::LinkOccurrence> for LinkOccurrenceView {
    fn from(occurrence: &crate::domain::LinkOccurrence) -> Self {
        Self {
            owner_id: occurrence.owner().to_string(),
            target_id: occurrence.target().to_string(),
            start_byte: occurrence.span().start(),
            end_byte: occurrence.span().end(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeEdgeView {
    pub source_id: String,
    pub target_id: String,
}

impl From<&crate::domain::NodeEdge> for NodeEdgeView {
    fn from(edge: &crate::domain::NodeEdge) -> Self {
        Self {
            source_id: edge.source().to_string(),
            target_id: edge.target().to_string(),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    MalformedRequest,
    UnsupportedVersion,
    InvalidDocument,
    NotFound,
    Conflict,
    Busy,
    Internal,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
}

impl ProtocolError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum MessageValidationError {
    #[error("unsupported protocol version {0}")]
    UnsupportedVersion(u16),
    #[error("request ID must contain 1 to {MAX_REQUEST_ID_LEN} bytes")]
    InvalidRequestId,
    #[error("review request is invalid")]
    InvalidReviewRequest,
    #[error("asset chunk request is invalid")]
    InvalidAssetChunk,
}

#[cfg(test)]
mod tests {
    use super::{
        ErrorCode, MAX_REQUEST_ID_LEN, MAX_REVIEW_ID_LEN, MessageValidationError, ProtocolError,
        Request, RequestEnvelope, ResponseEnvelope, ResponseOutcome, ResponseResult,
        ReviewSourceInput, ReviewSourceKindInput, SystemInfo,
    };

    #[test]
    fn request_round_trips_with_versioned_method() {
        let request = RequestEnvelope {
            version: 1,
            id: "request-1".into(),
            request: Request::DocumentGet {
                document_id: "doc-1".into(),
            },
        };
        let json = serde_json::to_string(&request).unwrap();
        assert_eq!(
            serde_json::from_str::<RequestEnvelope>(&json).unwrap(),
            request
        );
    }

    #[test]
    fn due_page_request_has_stable_wire_shape() {
        let request = RequestEnvelope {
            version: 1,
            id: "due-2".into(),
            request: Request::ReviewDuePage {
                day: 20260714,
                limit: 25,
                after: Some("0:20260713:8".into()),
            },
        };
        let value = serde_json::to_value(request).unwrap();
        assert_eq!(value["method"], "review.due_page");
        assert_eq!(value["params"]["day"], 20260714);
        assert_eq!(value["params"]["limit"], 25);
        assert_eq!(value["params"]["after"], "0:20260713:8");
    }

    #[test]
    fn rejects_unknown_version_and_unbounded_request_id() {
        let mut request = RequestEnvelope {
            version: 2,
            id: "request-1".into(),
            request: Request::SystemInfo {},
        };
        assert_eq!(
            request.validate(),
            Err(MessageValidationError::UnsupportedVersion(2))
        );
        request.version = 1;
        request.id = "x".repeat(MAX_REQUEST_ID_LEN + 1);
        assert_eq!(
            request.validate(),
            Err(MessageValidationError::InvalidRequestId)
        );

        let unknown_method = r#"{
            "version":1,
            "id":"request-2",
            "method":"database.execute",
            "params":{"sql":"SELECT 1"}
        }"#;
        assert!(serde_json::from_str::<RequestEnvelope>(unknown_method).is_err());
    }

    #[test]
    fn response_cannot_contain_success_and_error() {
        let response = ResponseEnvelope::success(
            "request-1".into(),
            ResponseResult::SystemInfo(SystemInfo {
                protocol_version: 1,
            }),
        );
        assert!(matches!(response.outcome, ResponseOutcome::Success { .. }));

        let response = ResponseEnvelope::error(
            "request-2".into(),
            ProtocolError::new(ErrorCode::Busy, "daemon is busy"),
        );
        assert!(matches!(response.outcome, ResponseOutcome::Failure { .. }));

        let both = r#"{
            "version":1,
            "id":"request-3",
            "status":"success",
            "result":{"type":"system_info","value":{"protocol_version":1}},
            "error":{"code":"internal","message":"should be rejected"}
        }"#;
        assert!(serde_json::from_str::<ResponseEnvelope>(both).is_err());
    }

    #[test]
    fn local_review_requests_have_stable_wire_shapes() {
        let requests = [
            r#"{"version":1,"id":"emacs-1","method":"review.session_create","params":{"source":{"kind":"due","tags":[],"exclude_tags":[],"topic_node_id":null,"topic_forward_depth":null,"topic_backward_depth":null,"manual_ids":[]},"target_count":null}}"#,
            r#"{"version":1,"id":"emacs-2","method":"review.session_active","params":{}}"#,
            r#"{"version":1,"id":"emacs-3","method":"review.session_resume","params":{"session_id":"SESSION"}}"#,
            r#"{"version":1,"id":"emacs-4","method":"review.session_quit","params":{"session_id":"SESSION"}}"#,
            r#"{"version":1,"id":"emacs-5","method":"review.session_abort","params":{"session_id":"SESSION"}}"#,
            r#"{"version":1,"id":"emacs-6","method":"review.attempt_begin","params":{"session_id":"SESSION"}}"#,
            r#"{"version":1,"id":"emacs-7","method":"review.attempt_answer","params":{"attempt_id":"ATTEMPT","response":{"kind":"text","value":"answer"}}}"#,
            r#"{"version":1,"id":"emacs-8","method":"review.attempt_override","params":{"attempt_id":"ATTEMPT"}}"#,
            r#"{"version":1,"id":"emacs-9","method":"review.attempt_suspension_toggle","params":{"attempt_id":"ATTEMPT"}}"#,
            r#"{"version":1,"id":"emacs-10","method":"review.attempt_commit","params":{"attempt_id":"ATTEMPT"}}"#,
            r#"{"version":1,"id":"emacs-11","method":"asset.chunk","params":{"logical_path":"assets/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/name.png","offset":0,"limit":524288}}"#,
        ];

        for json in requests {
            let request: RequestEnvelope = serde_json::from_str(json).unwrap();
            request.validate().unwrap();
            assert_eq!(
                serde_json::to_value(request).unwrap(),
                serde_json::from_str::<serde_json::Value>(json).unwrap()
            );
        }
    }

    #[test]
    fn local_review_requests_reject_invalid_inputs() {
        let invalid = [
            r#"{"version":1,"id":"x","method":"review.session_create","params":{"source":{"kind":"due","tags":["tag"],"exclude_tags":[],"topic_node_id":null,"topic_forward_depth":null,"topic_backward_depth":null,"manual_ids":[]},"target_count":null}}"#,
            r#"{"version":1,"id":"x","method":"review.session_resume","params":{"session_id":""}}"#,
            r#"{"version":1,"id":"x","method":"review.attempt_answer","params":{"attempt_id":"ATTEMPT","response":{"kind":"unknown","value":"answer"}}}"#,
            r#"{"version":1,"id":"x","method":"asset.chunk","params":{"logical_path":"assets/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/name.png","offset":0,"limit":0}}"#,
            r#"{"version":1,"id":"x","method":"asset.chunk","params":{"logical_path":"assets/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/name.png","offset":18446744073709551616,"limit":1}}"#,
            r#"{"version":1,"id":"x","method":"review.session_active","params":{"extra":true}}"#,
        ];

        for json in invalid {
            if let Ok(request) = serde_json::from_str::<RequestEnvelope>(json) {
                assert!(request.validate().is_err(), "accepted {json}");
            }
        }
        let request = RequestEnvelope {
            version: 1,
            id: "x".into(),
            request: Request::ReviewSessionCreate {
                source: ReviewSourceInput {
                    kind: ReviewSourceKindInput::Manual,
                    tags: Vec::new(),
                    exclude_tags: Vec::new(),
                    topic_node_id: None,
                    topic_forward_depth: None,
                    topic_backward_depth: None,
                    manual_ids: vec!["x".repeat(MAX_REVIEW_ID_LEN + 1)],
                },
                target_count: None,
            },
        };
        assert_eq!(
            request.validate(),
            Err(MessageValidationError::InvalidReviewRequest)
        );
    }

    #[test]
    fn local_review_results_have_stable_tags_and_reveal_local_answers() {
        let attempt = r#"{
            "version":1,
            "id":"emacs-6",
            "status":"success",
            "result":{"type":"review_attempt","value":{
                "attempt_id":"ATTEMPT",
                "session_id":"SESSION",
                "thema_id":"7",
                "presentation":{"kind":"basic","keimenon":"Question","hypothesis":[],"answer":["Answer"],"source_guid":null,"parathema":"Context","review_image":null,"tags":[]},
                "response":null,
                "detected_success":null,
                "accepted_success":null,
                "override_count":0,
                "proposed_transition":null,
                "desired_suspended":false,
                "status":"pending"
            }}
        }"#;
        let response: ResponseEnvelope = serde_json::from_str(attempt).unwrap();
        let value = serde_json::to_value(response).unwrap();
        assert_eq!(value["result"]["type"], "review_attempt");
        assert_eq!(
            value["result"]["value"]["presentation"]["answer"][0],
            "Answer"
        );
        assert_eq!(
            value["result"]["value"]["presentation"]["parathema"],
            "Context"
        );
    }
}
