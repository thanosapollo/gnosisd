#![cfg(unix)]

use std::{fs, os::unix::fs::PermissionsExt, time::Duration};

use gnosis_core::protocol::{
    DaemonConfig, DaemonServer, ErrorCode, MAX_REPAIR_BATCH_SIZE, PROTOCOL_VERSION, RepairView,
    Request, RequestEnvelope, ResponseEnvelope, ResponseOutcome, ResponseResult,
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

    let provisioning = Request::DeviceCreate {
        device_id: "0198-operator-test".into(),
        name: "operator-test".into(),
        token: "gnosis_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
        scopes: vec!["read".into()],
    };
    let created = request(&socket, "device-create", provisioning.clone()).await;
    assert!(matches!(
        &created.outcome,
        ResponseOutcome::Success { result }
            if matches!(**result, ResponseResult::DeviceCreated(_))
    ));
    assert!(!serde_json::to_string(&created).unwrap().contains("gnosis_"));
    let replayed = request(&socket, "device-create-replay", provisioning).await;
    assert!(matches!(
        replayed.outcome,
        ResponseOutcome::Success { result }
            if matches!(*result, ResponseResult::DeviceCreated(_))
    ));
    let mismatch = request(
        &socket,
        "device-create-mismatch",
        Request::DeviceCreate {
            device_id: "0198-operator-test".into(),
            name: "other".into(),
            token: "gnosis_AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into(),
            scopes: vec!["read".into()],
        },
    )
    .await;
    assert!(matches!(
        mismatch.outcome,
        ResponseOutcome::Failure { error } if error.code == ErrorCode::Conflict
    ));
    let listed = request(&socket, "device-list", Request::DeviceList {}).await;
    assert!(matches!(
        listed.outcome,
        ResponseOutcome::Success { result }
            if matches!(*result, ResponseResult::Devices(ref devices) if devices.len() == 1)
    ));

    let repair = request(
        &socket,
        "repair",
        Request::Repair {
            batch_size: MAX_REPAIR_BATCH_SIZE,
        },
    )
    .await;
    match repair.outcome {
        ResponseOutcome::Success { result } => assert_eq!(
            *result,
            ResponseResult::Repair(RepairView {
                document_revisions_repaired: 0,
                blobs_verified: 0,
                max_batch_observed: 0,
            })
        ),
        ResponseOutcome::Failure { error } => panic!("admin.repair failed: {error:?}"),
    }

    let invalid = request(&socket, "repair-invalid", Request::Repair { batch_size: 0 }).await;
    assert!(matches!(
        invalid.outcome,
        ResponseOutcome::Failure { error }
            if error.code == ErrorCode::MalformedRequest
    ));

    shutdown_tx.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn shutdown_cancels_an_accepted_contended_backup_before_draining_handlers() {
    let temporary = TempDir::new().unwrap();
    let socket = temporary.path().join("gnosis.sock");
    let database = temporary.path().join("gnosis.sqlite");
    let backup = temporary.path().join("backup");
    let server = DaemonServer::bind(DaemonConfig::new(&socket, &database))
        .await
        .unwrap();
    let writer = rusqlite::Connection::open(&database).unwrap();
    writer.execute_batch("BEGIN EXCLUSIVE;").unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_task = tokio::spawn(server.run(async {
        let _ = shutdown_rx.await;
    }));
    let request_socket = socket.clone();
    let request_backup = backup.clone();
    let backup_task = tokio::spawn(async move {
        request(
            &request_socket,
            "backup-create",
            Request::BackupCreate {
                destination: request_backup.to_string_lossy().into_owned(),
            },
        )
        .await
    });

    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            let snapshot_exists = fs::read_dir(temporary.path()).unwrap().any(|entry| {
                entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".gnosis-backup-snapshot-")
            });
            if snapshot_exists {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("backup was not accepted before shutdown");
    shutdown_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(1), server_task)
        .await
        .expect("accepted contended backup blocked daemon shutdown")
        .unwrap()
        .unwrap();
    let response = backup_task.await.unwrap();
    assert!(matches!(response.outcome, ResponseOutcome::Failure { .. }));
    assert!(!backup.exists());
    assert!(fs::read_dir(temporary.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".gnosis-backup-")
    }));
    writer.execute_batch("ROLLBACK;").unwrap();
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
