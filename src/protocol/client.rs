use std::{path::Path, time::Duration};

use thiserror::Error;
use tokio::net::UnixStream;
use tokio::time::timeout;

use super::{PROTOCOL_VERSION, RequestEnvelope, ResponseEnvelope, frame};

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("could not connect to daemon")]
    Connect(#[source] std::io::Error),
    #[error("request transport failed")]
    Transport,
    #[error("daemon closed the connection without a response")]
    MissingResponse,
    #[error("response request ID did not match the request")]
    MismatchedResponse,
    #[error("daemon response used an unsupported protocol version")]
    UnsupportedResponseVersion,
    #[error("daemon request timed out")]
    Timeout,
}

pub async fn request(
    socket_path: impl AsRef<Path>,
    request: &RequestEnvelope,
) -> Result<ResponseEnvelope, ClientError> {
    timeout(
        Duration::from_secs(30),
        request_with_timeout(socket_path.as_ref(), request),
    )
    .await
    .map_err(|_| ClientError::Timeout)?
}

async fn request_with_timeout(
    socket_path: &Path,
    request: &RequestEnvelope,
) -> Result<ResponseEnvelope, ClientError> {
    let mut stream = UnixStream::connect(socket_path)
        .await
        .map_err(ClientError::Connect)?;
    frame::write_json_frame(&mut stream, request)
        .await
        .map_err(|_| ClientError::Transport)?;
    let response: ResponseEnvelope = frame::read_json_frame(&mut stream)
        .await
        .map_err(|_| ClientError::Transport)?
        .ok_or(ClientError::MissingResponse)?;
    validate_response(request, &response)?;
    Ok(response)
}

fn validate_response(
    request: &RequestEnvelope,
    response: &ResponseEnvelope,
) -> Result<(), ClientError> {
    if response.version != PROTOCOL_VERSION {
        return Err(ClientError::UnsupportedResponseVersion);
    }
    if response.id != request.id {
        return Err(ClientError::MismatchedResponse);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ClientError, validate_response};
    use crate::protocol::{
        PROTOCOL_VERSION, Request, RequestEnvelope, ResponseEnvelope, ResponseResult, SystemInfo,
    };

    #[test]
    fn rejects_response_from_another_protocol_version() {
        let request = RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "request".into(),
            request: Request::SystemInfo {},
        };
        let mut response = ResponseEnvelope::success(
            "request".into(),
            ResponseResult::SystemInfo(SystemInfo {
                protocol_version: PROTOCOL_VERSION,
            }),
        );
        response.version += 1;
        assert!(matches!(
            validate_response(&request, &response),
            Err(ClientError::UnsupportedResponseVersion)
        ));
    }
}
