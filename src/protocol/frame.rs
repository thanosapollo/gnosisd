use std::io::ErrorKind;

use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::MAX_FRAME_SIZE;

#[derive(Debug, Error)]
pub(crate) enum FrameError {
    #[error("connection closed in the middle of a frame")]
    Truncated,
    #[error("frame must not be empty")]
    Empty,
    #[error("frame exceeds the maximum size")]
    TooLarge,
    #[error("frame I/O failed")]
    Io(#[source] std::io::Error),
    #[error("frame contains malformed JSON")]
    Json(#[source] serde_json::Error),
}

pub(crate) async fn read_json_frame<T, R>(reader: &mut R) -> Result<Option<T>, FrameError>
where
    T: DeserializeOwned,
    R: AsyncRead + Unpin,
{
    let Some(size) = read_size(reader).await? else {
        return Ok(None);
    };
    let mut bytes = vec![0; size];
    reader
        .read_exact(&mut bytes)
        .await
        .map_err(map_read_error)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(FrameError::Json)
}

pub(crate) async fn write_json_frame<T, W>(writer: &mut W, value: &T) -> Result<(), FrameError>
where
    T: Serialize,
    W: AsyncWrite + Unpin,
{
    let bytes = serde_json::to_vec(value).map_err(FrameError::Json)?;
    let size = checked_size(bytes.len())?;
    writer
        .write_all(&size.to_be_bytes())
        .await
        .map_err(FrameError::Io)?;
    writer.write_all(&bytes).await.map_err(FrameError::Io)?;
    writer.flush().await.map_err(FrameError::Io)
}

async fn read_size<R>(reader: &mut R) -> Result<Option<usize>, FrameError>
where
    R: AsyncRead + Unpin,
{
    let mut prefix = [0; 4];
    match reader.read(&mut prefix[..1]).await {
        Ok(0) => return Ok(None),
        Ok(_) => {}
        Err(error) => return Err(FrameError::Io(error)),
    }
    reader
        .read_exact(&mut prefix[1..])
        .await
        .map_err(map_read_error)?;
    let size = u32::from_be_bytes(prefix) as usize;
    checked_size(size).map(|_| Some(size))
}

fn checked_size(size: usize) -> Result<u32, FrameError> {
    if size == 0 {
        return Err(FrameError::Empty);
    }
    if size > MAX_FRAME_SIZE {
        return Err(FrameError::TooLarge);
    }
    u32::try_from(size).map_err(|_| FrameError::TooLarge)
}

fn map_read_error(error: std::io::Error) -> FrameError {
    if error.kind() == ErrorKind::UnexpectedEof {
        FrameError::Truncated
    } else {
        FrameError::Io(error)
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::{AsyncWriteExt, duplex};

    use super::{FrameError, read_json_frame};
    use crate::protocol::{MAX_FRAME_SIZE, RequestEnvelope};

    #[tokio::test]
    async fn rejects_oversized_frame_before_reading_body() {
        let (mut client, mut server) = duplex(16);
        client
            .write_all(&u32::try_from(MAX_FRAME_SIZE + 1).unwrap().to_be_bytes())
            .await
            .unwrap();
        let error = read_json_frame::<RequestEnvelope, _>(&mut server)
            .await
            .unwrap_err();
        assert!(matches!(error, FrameError::TooLarge));
    }

    #[tokio::test]
    async fn rejects_empty_frame() {
        let (mut client, mut server) = duplex(16);
        client.write_all(&0_u32.to_be_bytes()).await.unwrap();
        let error = read_json_frame::<RequestEnvelope, _>(&mut server)
            .await
            .unwrap_err();
        assert!(matches!(error, FrameError::Empty));
    }

    #[tokio::test]
    async fn rejects_partial_size_prefix() {
        let (mut client, mut server) = duplex(16);
        client.write_all(&[0, 0]).await.unwrap();
        drop(client);
        let error = read_json_frame::<RequestEnvelope, _>(&mut server)
            .await
            .unwrap_err();
        assert!(matches!(error, FrameError::Truncated));
    }
}
