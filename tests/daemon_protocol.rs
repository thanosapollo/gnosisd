#![cfg(unix)]

use std::time::Duration;

use gnosis_core::protocol::{
    DaemonConfig, DaemonServer, PROTOCOL_VERSION, Request, RequestEnvelope, ResponseEnvelope,
    ResponseOutcome, ResponseResult,
};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

#[tokio::test]
async fn unix_socket_serves_operator_system_info() {
    let temporary = TempDir::new().unwrap();
    let socket = temporary.path().join("gnosis.sock");
    let database = temporary.path().join("gnosis.sqlite");
    let server = DaemonServer::bind(DaemonConfig::new(socket.clone(), database))
        .await
        .unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = shutdown_rx.await;
    }));

    let mut stream = connect(&socket).await;
    let request = RequestEnvelope {
        version: PROTOCOL_VERSION,
        id: "system-info".into(),
        request: Request::SystemInfo {},
    };
    let frame = serde_json::to_vec(&request).unwrap();
    stream
        .write_all(&u32::try_from(frame.len()).unwrap().to_be_bytes())
        .await
        .unwrap();
    stream.write_all(&frame).await.unwrap();

    let size = stream.read_u32().await.unwrap();
    let mut response = vec![0; usize::try_from(size).unwrap()];
    stream.read_exact(&mut response).await.unwrap();
    let response: ResponseEnvelope = serde_json::from_slice(&response).unwrap();
    match response.outcome {
        ResponseOutcome::Success { result } => assert_eq!(
            *result,
            ResponseResult::SystemInfo(gnosis_core::protocol::SystemInfo {
                protocol_version: PROTOCOL_VERSION,
            })
        ),
        ResponseOutcome::Failure { error } => panic!("system.info failed: {error:?}"),
    }

    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

async fn connect(socket: &std::path::Path) -> UnixStream {
    for _ in 0..100 {
        if let Ok(stream) = UnixStream::connect(socket).await {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("daemon socket was not ready");
}
