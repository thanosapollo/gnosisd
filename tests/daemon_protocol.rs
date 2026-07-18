#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

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
async fn unix_socket_serves_operator_commands_and_secures_files() {
    let temporary = TempDir::new().unwrap();
    let socket = temporary.path().join("gnosis.sock");
    let database = temporary.path().join("gnosis.sqlite");
    let server = DaemonServer::bind(DaemonConfig::new(socket.clone(), database.clone()))
        .await
        .unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = shutdown_rx.await;
    }));

    connect(&socket).await;
    assert_eq!(
        fs::metadata(&socket).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(&database).unwrap().permissions().mode() & 0o777,
        0o600
    );

    let response = request(&socket, "system-info", Request::SystemInfo {}).await;
    match response.outcome {
        ResponseOutcome::Success { result } => assert_eq!(
            *result,
            ResponseResult::SystemInfo(gnosis_core::protocol::SystemInfo {
                protocol_version: PROTOCOL_VERSION,
            })
        ),
        ResponseOutcome::Failure { error } => panic!("system.info failed: {error:?}"),
    }

    let created = request(
        &socket,
        "device-create",
        Request::DeviceCreate {
            name: "operator-test".into(),
            scopes: vec!["read".into()],
        },
    )
    .await;
    assert!(matches!(
        created.outcome,
        ResponseOutcome::Success { result }
            if matches!(*result, ResponseResult::DeviceCreated(_))
    ));
    let listed = request(&socket, "device-list", Request::DeviceList {}).await;
    assert!(matches!(
        listed.outcome,
        ResponseOutcome::Success { result }
            if matches!(*result, ResponseResult::Devices(ref devices) if devices.len() == 1)
    ));

    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

async fn request(socket: &std::path::Path, id: &str, request: Request) -> ResponseEnvelope {
    let mut stream = connect(socket).await;
    let request = RequestEnvelope {
        version: PROTOCOL_VERSION,
        id: id.into(),
        request,
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
    serde_json::from_slice(&response).unwrap()
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
