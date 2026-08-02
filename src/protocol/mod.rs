use std::{borrow::Cow, marker::PhantomData};

use schemars::{JsonSchema, Schema, SchemaGenerator};

mod client;
mod frame;
pub mod http_v1;
mod message;
mod offline_asset;
mod offline_content;
mod offline_review;
mod replica;
mod server;

pub(crate) struct RequiredNullable<T>(PhantomData<T>);

impl<T: JsonSchema> JsonSchema for RequiredNullable<T> {
    fn schema_name() -> Cow<'static, str> {
        format!("RequiredNullable_{}", T::schema_name()).into()
    }

    fn schema_id() -> Cow<'static, str> {
        format!("RequiredNullable<{}>", T::schema_id()).into()
    }

    fn json_schema(generator: &mut SchemaGenerator) -> Schema {
        Option::<T>::json_schema(generator)
    }
}

pub use client::{ClientError, request};
pub use http_v1::{
    DocumentView, DuePageView, DueThemaView, LinkOccurrenceView, NodeEdgeView, NodeView,
};
pub(crate) use message::MessageValidationError;
pub use message::{
    BackupView, BackupsPrunedView, DeviceCreatedView, DeviceView, ErrorCode, MAX_FRAME_SIZE,
    MAX_REPAIR_BATCH_SIZE, MAX_REQUEST_ID_LEN, PROTOCOL_VERSION, ProtocolError, RepairView,
    Request, RequestEnvelope, ResponseEnvelope, ResponseOutcome, ResponseResult, SystemInfo,
};
#[cfg(test)]
pub(crate) use offline_asset::canonical_asset_operation;
pub(crate) use offline_asset::{
    OfflineAssetSubmission, OfflineAssetWireError, offline_asset_capabilities,
    offline_asset_result_response,
};
#[cfg(test)]
pub(crate) use offline_content::canonical_content_operation;
pub(crate) use offline_content::{
    MAX_OFFLINE_CONTENT_OPERATION_BYTES, OfflineContentWireError, offline_content_capabilities,
    offline_content_result_response,
};
#[cfg(test)]
pub(crate) use offline_review::canonical_offline_operation;
pub(crate) use offline_review::{
    MAX_OFFLINE_REVIEW_OPERATION_BYTES, OFFLINE_REVIEW_VERSION, OfflineReviewSubmission,
    OfflineReviewWireError, active_review_pack_response, offline_result_response,
    review_pack_response,
};
pub(crate) use replica::encode_cursor;
pub(crate) use replica::{
    MAX_REPLICA_CURSOR_BYTES, ReplicaWireError, decode_cursor, decode_snapshot_cursor,
    replica_change_page, replica_snapshot_page, replica_state_response,
};
pub use server::{DaemonConfig, DaemonServer, ServerError};
