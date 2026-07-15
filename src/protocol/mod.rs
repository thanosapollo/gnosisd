mod client;
mod frame;
mod message;
mod offline_content;
mod offline_review;
mod replica;
mod server;

pub use client::{ClientError, request};
pub(crate) use message::MessageValidationError;
pub use message::{
    BackupView, BackupsPrunedView, DeviceCreatedView, DeviceView, DocumentView, DuePageView,
    DueThemaView, ErrorCode, ImportResult, LegacyInfoView, LinkOccurrenceView, MAX_FRAME_SIZE,
    MAX_REQUEST_ID_LEN, NodeEdgeView, NodeView, PROTOCOL_VERSION, ProtocolError, Request,
    RequestEnvelope, ResponseEnvelope, ResponseOutcome, ResponseResult, ReviewStateView,
    SystemInfo, ThemaView,
};
pub(crate) use offline_content::{
    DecodedOfflineContentResult, OfflineContentSubmission, OfflineContentWireError,
    canonical_content_operation, decode_offline_content_capabilities,
    decode_offline_content_result, offline_content_capabilities_json, offline_content_result_json,
};
pub(crate) use offline_review::canonical_offline_operation;
pub(crate) use offline_review::{
    OFFLINE_REVIEW_VERSION, OfflineReviewSubmission, OfflineReviewWireError,
    active_review_pack_json, decode_offline_result, decode_review_pack, offline_result_json,
    review_pack_json,
};
pub(crate) use replica::{
    DecodedReplicaItem, DecodedSnapshotPage, ReplicaWireError, decode_change_page, decode_cursor,
    decode_replica_state, decode_snapshot_cursor, decode_snapshot_page, replica_changes_json,
    replica_snapshot_json, replica_state_json,
};
#[cfg(test)]
pub(crate) use replica::{MAX_REPLICA_PAGE_BYTES, encode_cursor, encode_snapshot_cursor};
pub use server::{DaemonConfig, DaemonServer, ServerError};
