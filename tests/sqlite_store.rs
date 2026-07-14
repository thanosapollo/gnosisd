use std::path::{Path, PathBuf};

use gnosis_core::protocol::{
    DaemonConfig, DaemonServer, PROTOCOL_VERSION, Request, RequestEnvelope, ResponseOutcome,
    ResponseResult, request,
};
use tokio::sync::oneshot;

#[tokio::test]
async fn document_persists_across_daemon_restart_without_client_database_access() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("store.sqlite");
    let first_socket = directory.path().join("first.sock");
    let first = RunningDaemon::start(first_socket, database.clone()).await;
    let response = first
        .call(Request::DocumentImport {
            document_id: "persistent".into(),
            org: "#+TITLE: Persistent\n".into(),
            expected_revision: None,
        })
        .await;
    let ResponseOutcome::Success { result } = response else {
        panic!("document import failed");
    };
    assert!(matches!(*result, ResponseResult::DocumentImported(_)));
    first.stop().await;

    let second = RunningDaemon::start(directory.path().join("second.sock"), database).await;
    let response = second
        .call(Request::DocumentGet {
            document_id: "persistent".into(),
        })
        .await;
    let ResponseOutcome::Success { result } = response else {
        panic!("persisted document was not returned after restart");
    };
    let ResponseResult::Document(document) = *result else {
        panic!("unexpected document response");
    };
    assert_eq!(document.revision, 1);
    assert_eq!(document.title, "Persistent");
    second.stop().await;
}

struct RunningDaemon {
    socket: PathBuf,
    shutdown: oneshot::Sender<()>,
    task: tokio::task::JoinHandle<Result<(), gnosis_core::protocol::ServerError>>,
}

impl RunningDaemon {
    async fn start(socket: PathBuf, database: PathBuf) -> Self {
        let server = DaemonServer::bind(DaemonConfig::new(&socket, database))
            .await
            .unwrap();
        let (shutdown, receiver) = oneshot::channel();
        let task = tokio::spawn(server.run(async {
            let _ = receiver.await;
        }));
        Self {
            socket,
            shutdown,
            task,
        }
    }

    async fn call(&self, request_body: Request) -> ResponseOutcome {
        let envelope = RequestEnvelope {
            version: PROTOCOL_VERSION,
            id: "store-test".into(),
            request: request_body,
        };
        request(Path::new(&self.socket), &envelope)
            .await
            .unwrap()
            .outcome
    }

    async fn stop(self) {
        self.shutdown.send(()).unwrap();
        self.task.await.unwrap().unwrap();
    }
}
