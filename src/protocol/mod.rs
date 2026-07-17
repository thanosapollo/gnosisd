mod client;
pub mod client_v1;
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
    LinkOccurrenceView, LocalFsrsConfigView, LocalPresentationView, LocalReviewAttemptMutationV2,
    LocalReviewAttemptStateV2, LocalReviewAttemptView, LocalReviewSessionView,
    LocalReviewStateView, LocalTransitionView, MAX_ASSET_CHUNK_BYTES, MAX_FRAME_SIZE,
    MAX_REQUEST_ID_LEN, MAX_REVIEW_ID_LEN, NodeEdgeView, NodeView, PROTOCOL_VERSION, ProtocolError,
    Request, RequestEnvelope, ResponseEnvelope, ResponseOutcome, ResponseResult, ReviewAnswerInput,
    ReviewEventView, ReviewSourceInput, ReviewSourceKindInput, ReviewStateView, SystemInfo,
    ThemaView,
};
#[cfg(test)]
pub(crate) use offline_asset::offline_asset_result_json;
pub(crate) use offline_asset::{
    DecodedOfflineAssetResult, MAX_OFFLINE_ASSET_OPERATION_BYTES, MAX_OFFLINE_ASSET_RESULT_BYTES,
    OfflineAssetSubmission, OfflineAssetWireError, canonical_asset_operation,
    decode_offline_asset_capabilities, decode_offline_asset_result, offline_asset_capabilities,
    offline_asset_result_response,
};
#[cfg(test)]
pub(crate) use offline_content::offline_content_result_json;
pub(crate) use offline_content::{
    DecodedOfflineContentResult, MAX_OFFLINE_CONTENT_OPERATION_BYTES,
    MAX_OFFLINE_CONTENT_RESULT_BYTES, OfflineContentSubmission, OfflineContentWireError,
    canonical_content_operation, decode_offline_content_capabilities,
    decode_offline_content_result, offline_content_capabilities, offline_content_result_response,
};
pub(crate) use offline_review::canonical_offline_operation;
pub(crate) use offline_review::{
    MAX_OFFLINE_REVIEW_OPERATION_BYTES, MAX_OFFLINE_REVIEW_RESULT_BYTES, OFFLINE_REVIEW_VERSION,
    OfflineReviewSubmission, OfflineReviewWireError, active_review_pack_response,
    decode_offline_result, decode_review_pack, offline_result_response, review_pack_response,
};
#[cfg(test)]
pub(crate) use offline_review::{offline_result_json, review_pack_json};
pub(crate) use replica::MAX_REPLICA_PAGE_BYTES;
pub(crate) use replica::encode_cursor;
#[cfg(test)]
pub(crate) use replica::encode_snapshot_cursor;
pub(crate) use replica::{
    DecodedReplicaItem, DecodedSnapshotPage, MAX_REPLICA_CURSOR_BYTES, ReplicaWireError,
    decode_change_page, decode_cursor, decode_replica_state, decode_snapshot_cursor,
    decode_snapshot_page, replica_change_page, replica_snapshot_page, replica_state_response,
};
pub use server::{DaemonConfig, DaemonServer, ServerError};
