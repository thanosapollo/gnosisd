mod client;
mod frame;
pub mod http_v1;
mod message;
mod offline_asset;
mod offline_content;
mod offline_review;
mod replica;
mod server;

pub use client::{ClientError, request};
pub(crate) use message::MessageValidationError;
pub use message::{
    ActiveReviewSessionView, AssetChunkView, BackupView, BackupsPrunedView, DeviceCreatedView,
    DeviceView, DocumentView, DuePageView, DueThemaView, ErrorCode, ImportResult, LegacyInfoView,
    LinkOccurrenceView, LocalParametersView, LocalPresentationView, LocalReviewAttemptMutationV2,
    LocalReviewAttemptStateV2, LocalReviewAttemptView, LocalReviewSessionView,
    LocalReviewStateView, LocalTransitionView, MAX_ASSET_CHUNK_BYTES, MAX_FRAME_SIZE,
    MAX_REQUEST_ID_LEN, MAX_REVIEW_ID_LEN, NodeEdgeView, NodeView, PROTOCOL_VERSION, ProtocolError,
    Request, RequestEnvelope, ResponseEnvelope, ResponseOutcome, ResponseResult, ReviewAnswerInput,
    ReviewEventView, ReviewSourceInput, ReviewSourceKindInput, ReviewStateView, SystemInfo,
    ThemaView,
};
pub(crate) use offline_asset::{
    DecodedOfflineAssetResult, OfflineAssetSubmission, OfflineAssetWireError,
    canonical_asset_operation, decode_offline_asset_capabilities, decode_offline_asset_result,
    offline_asset_capabilities, offline_asset_result_json,
};
pub(crate) use offline_content::{
    DecodedOfflineContentResult, OfflineContentSubmission, OfflineContentWireError,
    canonical_content_operation, decode_offline_content_capabilities,
    decode_offline_content_result, offline_content_capabilities, offline_content_result_json,
};
pub(crate) use offline_review::canonical_offline_operation;
pub(crate) use offline_review::{
    OFFLINE_REVIEW_VERSION, OfflineReviewSubmission, OfflineReviewWireError,
    active_review_pack_json, decode_offline_result, decode_review_pack, offline_result_json,
    review_pack_json,
};
pub(crate) use replica::{
    DecodedReplicaItem, DecodedSnapshotPage, MAX_REPLICA_CURSOR_BYTES, ReplicaWireError,
    decode_change_page, decode_cursor, decode_replica_state, decode_snapshot_cursor,
    decode_snapshot_page, replica_change_page, replica_snapshot_page, replica_state_response,
};
#[cfg(test)]
pub(crate) use replica::{MAX_REPLICA_PAGE_BYTES, encode_cursor, encode_snapshot_cursor};
pub use server::{DaemonConfig, DaemonServer, ServerError};
