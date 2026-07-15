mod client;
mod frame;
mod message;
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
#[cfg(test)]
pub(crate) use offline_review::canonical_offline_operation;
pub(crate) use offline_review::{
    OFFLINE_REVIEW_VERSION, OfflineReviewSubmission, OfflineReviewWireError,
    active_review_pack_json, offline_result_json, review_pack_json,
};
#[cfg(test)]
pub(crate) use replica::{MAX_REPLICA_PAGE_BYTES, encode_cursor};
pub(crate) use replica::{
    ReplicaWireError, decode_cursor, decode_snapshot_cursor, replica_changes_json,
    replica_snapshot_json, replica_state_json,
};
pub use server::{DaemonConfig, DaemonServer, ServerError};
