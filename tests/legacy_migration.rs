use std::{
    fs,
    os::unix::ffi::OsStrExt,
    os::unix::fs::{PermissionsExt, symlink},
    path::Path,
    process::Command,
};

use gnosis_core::migration::{
    LegacyMigrationError, migrate_legacy_database, migrate_legacy_server,
};
use gnosis_core::protocol::{
    DaemonConfig, DaemonServer, PROTOCOL_VERSION, Request, RequestEnvelope, ResponseOutcome,
    ResponseResult, request,
};
use rusqlite::Connection;
use tempfile::tempdir;
use tokio::sync::oneshot;

#[test]
fn migrates_v9_snapshot_without_changing_source() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    create_legacy_database(&source, 9);
    let source_bytes = fs::read(&source).unwrap();
    let source_mtime = fs::metadata(&source).unwrap().modified().unwrap();

    let summary = migrate_legacy_database(&source, &target).unwrap();

    assert_eq!(summary.source_schema_version(), 9);
    assert_eq!(summary.legacy_table_count(), 7);
    assert_eq!(summary.legacy_row_count(), 7);
    assert_source_is_unchanged(&source);
    assert_eq!(fs::read(&source).unwrap(), source_bytes);
    assert_eq!(
        fs::metadata(&source).unwrap().modified().unwrap(),
        source_mtime
    );
    assert_target_is_normalized(&target);
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(directory.path().read_dir().unwrap().count(), 2);
}

#[test]
fn empty_legacy_review_image_is_absent() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    create_legacy_database(&source, 9);
    Connection::open(&source)
        .unwrap()
        .execute("UPDATE extras SET review_image = '\"\"' WHERE id = 7", [])
        .unwrap();

    migrate_legacy_database(&source, &target).unwrap();

    let review_image: Option<String> = Connection::open(&target)
        .unwrap()
        .query_row(
            "SELECT review_image FROM thema WHERE thema_id = '7'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(review_image, None);
}

#[test]
fn refuses_to_replace_an_existing_target() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    create_legacy_database(&source, 9);
    fs::write(&target, b"keep me").unwrap();

    let error = migrate_legacy_database(&source, &target).unwrap_err();

    assert!(matches!(error, LegacyMigrationError::TargetExists));
    assert_eq!(fs::read(&target).unwrap(), b"keep me");
}

#[test]
fn recovers_an_interrupted_server_publication_before_retrying() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    let notes = directory.path().join("Notes");
    let blobs = directory.path().join("blobs");
    create_legacy_database(&source, 9);
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&blobs).unwrap();
    fs::write(blobs.join("orphan"), b"partial").unwrap();
    fs::create_dir(directory.path().join(".blobs.migrating")).unwrap();
    fs::write(
        directory.path().join(".gnosis.sqlite.migrating"),
        b"partial",
    )
    .unwrap();
    write_publication_marker(&target, &blobs);

    migrate_legacy_server(&source, &target, &notes, &blobs).unwrap();

    assert!(target.exists());
    assert!(blobs.is_dir());
    assert!(!directory.path().join(".gnosis.sqlite.publishing").exists());
    assert!(!directory.path().join(".gnosis.sqlite.migrating").exists());
    assert!(!directory.path().join(".blobs.migrating").exists());
}

#[test]
fn completed_publication_recovery_removes_only_staging_artifacts() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    let notes = directory.path().join("Notes");
    let blobs = directory.path().join("blobs");
    create_legacy_database(&source, 9);
    fs::create_dir(&notes).unwrap();
    migrate_legacy_server(&source, &target, &notes, &blobs).unwrap();
    fs::create_dir(directory.path().join(".blobs.migrating")).unwrap();
    fs::write(
        directory.path().join(".gnosis.sqlite.migrating"),
        b"duplicate",
    )
    .unwrap();
    write_publication_marker(&target, &blobs);

    let error = migrate_legacy_server(&source, &target, &notes, &blobs).unwrap_err();

    assert!(matches!(error, LegacyMigrationError::TargetExists));
    assert!(target.is_file());
    assert!(blobs.is_dir());
    assert!(!directory.path().join(".gnosis.sqlite.publishing").exists());
    assert!(!directory.path().join(".gnosis.sqlite.migrating").exists());
    assert!(!directory.path().join(".blobs.migrating").exists());
}

#[test]
fn interrupted_publication_rejects_a_different_blob_target() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    let notes = directory.path().join("Notes");
    let original_blobs = directory.path().join("original-blobs");
    let different_blobs = directory.path().join("different-blobs");
    create_legacy_database(&source, 9);
    fs::create_dir(&notes).unwrap();
    fs::create_dir(&original_blobs).unwrap();
    fs::write(original_blobs.join("partial"), b"partial").unwrap();
    fs::create_dir(&different_blobs).unwrap();
    fs::write(different_blobs.join("keep"), b"keep me").unwrap();
    write_publication_marker(&target, &original_blobs);

    let error = migrate_legacy_server(&source, &target, &notes, &different_blobs).unwrap_err();

    assert!(matches!(
        error,
        LegacyMigrationError::PublicationTargetMismatch
    ));
    assert_eq!(fs::read(different_blobs.join("keep")).unwrap(), b"keep me");
    assert!(original_blobs.join("partial").exists());
    assert!(directory.path().join(".gnosis.sqlite.publishing").exists());
}

#[test]
fn rejects_unsupported_legacy_schema_without_publishing_a_target() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    create_legacy_database(&source, 8);

    let error = migrate_legacy_database(&source, &target).unwrap_err();

    assert!(matches!(
        error,
        LegacyMigrationError::UnsupportedSourceSchema { found: 8 }
    ));
    assert!(!target.exists());
    assert_eq!(directory.path().read_dir().unwrap().count(), 1);
}

#[test]
fn rejects_incomplete_v9_schema_without_publishing_a_target() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    create_legacy_database(&source, 9);
    Connection::open(&source)
        .unwrap()
        .execute("DROP TABLE extras", [])
        .unwrap();

    let error = migrate_legacy_database(&source, &target).unwrap_err();

    assert!(matches!(
        error,
        LegacyMigrationError::IncompleteSourceSchema
    ));
    assert!(!target.exists());
}

#[test]
fn refuses_a_source_leased_by_emacs() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    create_legacy_database(&source, 9);
    fs::write(
        directory.path().join(".#legacy.db.gnosis-lease"),
        b"emacs lease",
    )
    .unwrap();

    let error = migrate_legacy_database(&source, &target).unwrap_err();

    assert!(matches!(error, LegacyMigrationError::SourceLeased));
    assert!(!target.exists());
}

#[test]
fn daemon_cli_reports_completed_offline_migration() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    create_legacy_database(&source, 9);

    let output = Command::new(env!("CARGO_BIN_EXE_gnosisd"))
        .args(["--database"])
        .arg(&target)
        .arg("--migrate-legacy")
        .arg(&source)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "migrated legacy schema v9: 7 tables, 7 rows\n"
    );
    assert!(output.stderr.is_empty());
    assert_target_is_normalized(&target);
}

#[tokio::test]
async fn migrated_snapshot_serves_legacy_thema_and_due_queue() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    let socket = directory.path().join("gnosis.sock");
    create_legacy_database(&source, 9);
    insert_due_thema(&source, 8, 0, 20260713);
    insert_due_thema(&source, 10, 0, 20260713);
    insert_due_thema(&source, 9, 1, 20260715);
    migrate_legacy_database(&source, &target).unwrap();

    let server = DaemonServer::bind(DaemonConfig::new(&socket, &target))
        .await
        .unwrap();
    let (shutdown_sender, shutdown_receiver) = oneshot::channel();
    let task = tokio::spawn(server.run(async {
        let _ = shutdown_receiver.await;
    }));

    assert_legacy_info(&socket).await;
    assert_legacy_thema(&socket).await;
    assert_due_queue(&socket).await;
    assert_due_pages(&socket).await;

    shutdown_sender.send(()).unwrap();
    task.await.unwrap().unwrap();
}

async fn assert_legacy_info(socket: &Path) {
    let result = successful_result(socket, Request::LegacyInfo {}).await;
    let ResponseResult::LegacyInfo(info) = result else {
        panic!("unexpected legacy info response");
    };
    assert!(info.imported);
    assert_eq!(info.source_schema_version, Some(9));
    assert_eq!(info.thema_count, 4);
    assert_eq!(info.review_state_count, 4);
}

async fn assert_legacy_thema(socket: &Path) {
    let result = successful_result(
        socket,
        Request::ThemaGet {
            thema_id: "7".into(),
        },
    )
    .await;
    let ResponseResult::Thema(thema) = result else {
        panic!("unexpected thema response");
    };
    assert_eq!(thema.id, "7");
    assert_eq!(thema.kind, "cloze");
    assert_eq!(thema.keimenon, "Question\ncontinued");
    assert_eq!(thema.hypothesis, [None]);
    assert_eq!(thema.answer, ["Answer"]);
    assert_eq!(thema.source_guid.as_deref(), Some("source-7"));
    assert_eq!(thema.parathema.as_deref(), Some("Context"));
    assert_eq!(thema.review_image, None);
    assert_eq!(thema.tags, ["medicine"]);
    assert_eq!(thema.links, ["7"]);
    assert_eq!(thema.review_state.review_count, 12);
    assert_eq!(thema.review_state.next_review_day, 20260714);
}

async fn assert_due_queue(socket: &Path) {
    let result = successful_result(
        socket,
        Request::ReviewDue {
            day: 20260714,
            limit: 10,
        },
    )
    .await;
    let ResponseResult::DueThemata(themata) = result else {
        panic!("unexpected due response");
    };
    assert_eq!(themata.len(), 3);
    assert_eq!(themata[0].id, "8");
    assert_eq!(themata[1].id, "10");
    assert_eq!(themata[2].id, "7");
    assert_eq!(themata[2].keimenon, "Question\ncontinued");
    assert_eq!(themata[2].hypothesis, [None]);
    assert_eq!(themata[2].answer, ["Answer"]);
    assert_eq!(themata[2].review_count, 12);
}

async fn assert_due_pages(socket: &Path) {
    let first = successful_result(
        socket,
        Request::ReviewDuePage {
            day: 20260714,
            limit: 1,
            after: None,
        },
    )
    .await;
    let ResponseResult::DuePage(first) = first else {
        panic!("unexpected due page response");
    };
    assert_eq!(first.items[0].id, "8");
    let next = first.next.expect("first page must continue");

    let second = successful_result(
        socket,
        Request::ReviewDuePage {
            day: 20260714,
            limit: 1,
            after: Some(next),
        },
    )
    .await;
    let ResponseResult::DuePage(second) = second else {
        panic!("unexpected due page response");
    };
    assert_eq!(second.items[0].id, "10");
    let next = second.next.expect("second page must continue");

    let third = successful_result(
        socket,
        Request::ReviewDuePage {
            day: 20260714,
            limit: 1,
            after: Some(next),
        },
    )
    .await;
    let ResponseResult::DuePage(third) = third else {
        panic!("unexpected due page response");
    };
    assert_eq!(third.items[0].id, "7");
    assert_eq!(third.next, None);
}

async fn successful_result(socket: &Path, request_value: Request) -> ResponseResult {
    let envelope = RequestEnvelope {
        version: PROTOCOL_VERSION,
        id: "legacy-test".into(),
        request: request_value,
    };
    let response = request(socket, &envelope).await.unwrap();
    let ResponseOutcome::Success { result } = response.outcome else {
        panic!("legacy request failed");
    };
    *result
}

fn create_legacy_database(path: &Path, version: u32) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(&format!(
            r#"
            PRAGMA foreign_keys = ON;
            CREATE TABLE themata (
                id INTEGER PRIMARY KEY,
                type TEXT NOT NULL,
                keimenon TEXT NOT NULL,
                hypothesis TEXT NOT NULL,
                answer TEXT NOT NULL,
                source_guid TEXT,
                archived_at_us INTEGER
            );
            CREATE TABLE nodes (
                id TEXT PRIMARY KEY,
                file TEXT NOT NULL,
                title TEXT NOT NULL,
                level TEXT NOT NULL
            );
            CREATE TABLE node_tag (
                node_id TEXT NOT NULL,
                tag TEXT NOT NULL,
                FOREIGN KEY (node_id) REFERENCES nodes(id),
                UNIQUE (node_id, tag)
            );
            CREATE TABLE review_state (
                thema_id INTEGER PRIMARY KEY,
                state_revision INTEGER NOT NULL,
                review_count INTEGER NOT NULL,
                legacy_review_count INTEGER NOT NULL,
                legacy_unclassified_count INTEGER NOT NULL,
                last_reviewed_at_us INTEGER,
                last_review_day INTEGER,
                next_review_day INTEGER NOT NULL,
                interval_days INTEGER,
                consecutive_successes INTEGER NOT NULL,
                total_successes INTEGER NOT NULL,
                consecutive_failures INTEGER NOT NULL,
                total_failures INTEGER NOT NULL,
                suspended INTEGER NOT NULL,
                gnosis_plus REAL NOT NULL,
                gnosis_minus REAL NOT NULL,
                gnosis_synolon REAL NOT NULL,
                FOREIGN KEY (thema_id) REFERENCES themata(id)
            );
            CREATE TABLE extras (
                id INTEGER PRIMARY KEY,
                parathema TEXT,
                review_image TEXT,
                FOREIGN KEY (id) REFERENCES themata(id)
            );
            CREATE TABLE thema_tag (
                thema_id INTEGER NOT NULL,
                tag TEXT NOT NULL,
                UNIQUE (thema_id, tag),
                FOREIGN KEY (thema_id) REFERENCES themata(id)
            );
            CREATE TABLE thema_links (
                source TEXT,
                dest TEXT,
                UNIQUE (source, dest),
                FOREIGN KEY (source) REFERENCES themata(id)
            );
            CREATE TRIGGER themata_no_delete
            BEFORE DELETE ON themata
            BEGIN
                SELECT RAISE(ABORT, 'themata are immutable');
            END;
            INSERT INTO themata VALUES (
                7, '"cloze"', '"Question
continued"', '(nil)', '("Answer")', '"source-7"', NULL
            );
            INSERT INTO nodes VALUES ('node-1', 'notes.org', 'Node', '1');
            INSERT INTO node_tag VALUES ('node-1', 'medicine');
            INSERT INTO extras VALUES (7, '"Context"', 'nil');
            INSERT INTO thema_tag VALUES (7, '"medicine"');
            INSERT INTO thema_links VALUES ('7', '"7"');
            INSERT INTO review_state VALUES (
                7, 2, 12, 12, 12, 1784040000000000, 20260713, 20260714, 1,
                0, 0, 0, 0, 0, 1.0, 1.0, 2.3
            );
            PRAGMA user_version = {version};
            "#
        ))
        .unwrap();
}

fn insert_due_thema(path: &Path, id: i64, review_count: i64, next_review_day: i64) {
    let connection = Connection::open(path).unwrap();
    connection
        .execute(
            "INSERT INTO themata VALUES (?1, '\"basic\"', '\"Question\"', 'nil', \
             '(\"Answer\")', NULL, NULL)",
            [id],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO review_state VALUES \
             (?1, 1, ?2, ?2, ?2, NULL, NULL, ?3, NULL, 0, 0, 0, 0, 0, 1.0, 1.0, 1.3)",
            rusqlite::params![id, review_count, next_review_day],
        )
        .unwrap();
}

fn assert_source_is_unchanged(path: &Path) {
    let connection = Connection::open(path).unwrap();
    assert_eq!(user_version(&connection), 9);
    assert_eq!(table_count(&connection, "node_tag"), 1);
    assert_eq!(table_count(&connection, "themata"), 1);
    assert_eq!(table_count(&connection, "document"), 0);
}

fn assert_target_is_normalized(path: &Path) {
    let connection = Connection::open(path).unwrap();
    assert_eq!(user_version(&connection), 10);
    assert_eq!(table_count(&connection, "themata"), 0);
    assert_eq!(table_count(&connection, "node_tag"), 1);
    assert_eq!(table_count(&connection, "thema"), 1);
    assert_eq!(table_count(&connection, "import_manifest"), 1);
    assert_eq!(scalar(&connection, "SELECT count(*) FROM thema"), 1);
    assert_eq!(
        scalar(
            &connection,
            "SELECT source_schema_version FROM import_manifest"
        ),
        9
    );
    assert_eq!(
        scalar(
            &connection,
            "SELECT count(*) FROM pragma_table_info('node_tag') WHERE name = 'document_id'"
        ),
        1
    );
}

#[test]
fn imports_notes_and_rewrites_assets_before_atomic_publish() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    let notes = directory.path().join("Notes");
    let blobs = directory.path().join("blobs");
    fs::create_dir(&notes).unwrap();
    fs::create_dir(notes.join("Pictures")).unwrap();
    fs::write(notes.join("Pictures/image.png"), b"image").unwrap();
    fs::write(
        notes.join("note.org"),
        "#+TITLE: Note\n[[file:Pictures/image.png]]\n",
    )
    .unwrap();
    create_legacy_database(&source, 9);

    let summary = migrate_legacy_server(&source, &target, &notes, &blobs).unwrap();

    assert_eq!(summary.note_count(), 1);
    assert_eq!(summary.asset_count(), 1);
    let connection = Connection::open(&target).unwrap();
    assert_eq!(
        scalar(&connection, "SELECT count(*) FROM document_revision"),
        1
    );
    assert_eq!(scalar(&connection, "SELECT count(*) FROM managed_asset"), 1);
    let digest: String = connection
        .query_row("SELECT head_blob_sha256 FROM document", [], |row| {
            row.get(0)
        })
        .unwrap();
    let content = fs::read_to_string(blobs.join(&digest[..2]).join(digest)).unwrap();
    assert!(content.contains("file:assets/"));
    assert!(!content.contains("Pictures/"));
    assert_eq!(
        scalar(
            &connection,
            "SELECT count(*) FROM import_path_rewrite WHERE reason = 'asset_normalization'"
        ),
        1
    );
}

#[test]
fn rewrites_legacy_thema_assets_into_managed_content() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    let notes = directory.path().join("Notes");
    let blobs = directory.path().join("blobs");
    fs::create_dir(&notes).unwrap();
    fs::write(notes.join("note.org"), "#+TITLE: Note\n").unwrap();
    fs::create_dir(directory.path().join("images")).unwrap();
    fs::write(directory.path().join("images/scan.png"), b"scan").unwrap();
    create_legacy_database(&source, 9);
    let connection = Connection::open(&source).unwrap();
    connection
        .execute(
            "UPDATE themata SET keimenon = ?1 WHERE id = 7",
            [r#""Question [file:scan.png]""#],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE extras SET review_image = ?1 WHERE id = 7",
            [r#""scan.png""#],
        )
        .unwrap();
    drop(connection);

    migrate_legacy_server(&source, &target, &notes, &blobs).unwrap();

    let connection = Connection::open(&target).unwrap();
    let (keimenon, review_image): (String, String) = connection
        .query_row(
            "SELECT keimenon, review_image FROM thema WHERE thema_id = '7'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert!(keimenon.contains("[file:assets/"));
    assert!(review_image.starts_with("assets/"));
    assert!(!keimenon.contains("[file:scan.png]"));
    assert_eq!(scalar(&connection, "SELECT count(*) FROM thema_asset"), 1);
}

#[test]
fn server_migration_preserves_historical_blank_cloze_entries() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    let notes = directory.path().join("Notes");
    let blobs = directory.path().join("blobs");
    fs::create_dir(&notes).unwrap();
    create_legacy_database(&source, 9);
    Connection::open(&source)
        .unwrap()
        .execute(
            "UPDATE themata SET hypothesis = '(nil \"\")', answer = '(\"Answer\" \"\")' \
             WHERE id = 7",
            [],
        )
        .unwrap();

    migrate_legacy_server(&source, &target, &notes, &blobs).unwrap();

    let connection = Connection::open(&target).unwrap();
    let (hypothesis, answer): (String, String) = connection
        .query_row(
            "SELECT hypothesis_json, answer_json FROM thema WHERE thema_id = '7'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(hypothesis, r#"[null,""]"#);
    assert_eq!(answer, r#"["Answer",""]"#);
}

#[test]
fn rejects_case_colliding_note_paths() {
    let directory = tempdir().unwrap();
    let notes = directory.path().join("Notes");
    fs::create_dir(&notes).unwrap();
    fs::write(notes.join("Topic.org"), "#+TITLE: One\n").unwrap();
    fs::write(notes.join("topic.org"), "#+TITLE: Two\n").unwrap();

    assert_invalid_notes_import(directory.path(), &notes);
}

#[test]
fn rejects_escaping_note_links_and_relevant_symlinks() {
    let directory = tempdir().unwrap();
    let notes = directory.path().join("Notes");
    fs::create_dir(&notes).unwrap();
    fs::write(
        notes.join("note.org"),
        "#+TITLE: Note\n[[file:../outside.org]]\n",
    )
    .unwrap();
    assert_invalid_notes_import(directory.path(), &notes);

    fs::remove_file(notes.join("note.org")).unwrap();
    fs::write(directory.path().join("outside.org"), "#+TITLE: Outside\n").unwrap();
    symlink(
        directory.path().join("outside.org"),
        notes.join("linked.org"),
    )
    .unwrap();
    assert_invalid_notes_import(directory.path(), &notes);
}

#[test]
fn rejects_malformed_emacsql_without_publishing_targets() {
    let directory = tempdir().unwrap();
    let source = directory.path().join("legacy.db");
    let target = directory.path().join("gnosis.sqlite");
    create_legacy_database(&source, 9);
    Connection::open(&source)
        .unwrap()
        .execute("UPDATE themata SET answer = '(\"unterminated)'", [])
        .unwrap();

    let error = migrate_legacy_database(&source, &target).unwrap_err();

    assert!(matches!(
        error,
        LegacyMigrationError::InvalidLegacyValue { field: "answer" }
    ));
    assert!(!target.exists());
    assert!(!directory.path().join(".gnosis.sqlite.migrating").exists());
}

fn assert_invalid_notes_import(root: &Path, notes: &Path) {
    let source = root.join("legacy.db");
    let target = root.join("gnosis.sqlite");
    let blobs = root.join("blobs");
    if !source.exists() {
        create_legacy_database(&source, 9);
    }

    let error = migrate_legacy_server(&source, &target, notes, &blobs).unwrap_err();

    assert!(matches!(error, LegacyMigrationError::InvalidNotesTree));
    assert!(!target.exists());
    assert!(!blobs.exists());
}

fn write_publication_marker(database_target: &Path, blob_target: &Path) {
    let database_target = canonical_destination(database_target);
    let blob_target = canonical_destination(blob_target);
    let mut contents = b"gnosis-publication-v1\0".to_vec();
    for path in [&database_target, &blob_target] {
        let bytes = path.as_os_str().as_bytes();
        contents.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
        contents.extend_from_slice(bytes);
    }
    let marker = database_target.with_file_name(".gnosis.sqlite.publishing");
    fs::write(marker, contents).unwrap();
}

fn canonical_destination(path: &Path) -> std::path::PathBuf {
    fs::canonicalize(path.parent().unwrap())
        .unwrap()
        .join(path.file_name().unwrap())
}

fn user_version(connection: &Connection) -> u32 {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

fn table_count(connection: &Connection, name: &str) -> u32 {
    connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name = ?1",
            [name],
            |row| row.get(0),
        )
        .unwrap()
}

fn scalar(connection: &Connection, query: &str) -> u64 {
    connection.query_row(query, [], |row| row.get(0)).unwrap()
}
