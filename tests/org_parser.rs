use std::path::Path;

use gnosis_core::protocol::{
    DaemonConfig, DaemonServer, PROTOCOL_VERSION, Request, RequestEnvelope, ResponseOutcome,
    ResponseResult, request,
};
use tokio::sync::oneshot;

#[tokio::test]
async fn org_projection_is_available_only_through_versioned_protocol() {
    let directory = tempfile::tempdir().unwrap();
    let socket = directory.path().join("parser.sock");
    let server = DaemonServer::bind(DaemonConfig::new(
        &socket,
        directory.path().join("parser.sqlite"),
    ))
    .await
    .unwrap();
    let (shutdown, receiver) = oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = receiver.await;
    }));

    let imported = call(
        &socket,
        "import",
        Request::DocumentImport {
            document_id: "fixture".into(),
            org: include_str!("fixtures/org/basic.org").into(),
            expected_revision: None,
        },
    )
    .await;
    let ResponseOutcome::Success {
        result: ResponseResult::DocumentImported(result),
    } = imported
    else {
        panic!("fixture import failed");
    };
    assert_eq!((result.node_count, result.occurrence_count), (3, 4));

    let document = call(
        &socket,
        "get",
        Request::DocumentGet {
            document_id: "fixture".into(),
        },
    )
    .await;
    let ResponseOutcome::Success {
        result: ResponseResult::Document(document),
    } = document
    else {
        panic!("fixture query failed");
    };
    assert_eq!(document.title, "Learn Emacs");
    assert_eq!(document.file_node_id.as_deref(), Some("root-id"));
    assert_eq!(document.nodes[2].title, "Elisp");
    assert_eq!(document.nodes[2].title_path, ["Learn Emacs", "Elisp"]);
    assert_eq!(document.link_occurrences.len(), 4);
    assert_eq!(document.semantic_edges.len(), 4);

    shutdown.send(()).unwrap();
    task.await.unwrap().unwrap();
}

async fn call(socket: &Path, id: &str, request_body: Request) -> ResponseOutcome {
    let envelope = RequestEnvelope {
        version: PROTOCOL_VERSION,
        id: id.into(),
        request: request_body,
    };
    request(socket, &envelope).await.unwrap().outcome
}
