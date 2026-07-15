PRAGMA foreign_keys = ON;

CREATE TABLE replica_identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    device_id TEXT NOT NULL,
    server_id TEXT,
    observed_server_id TEXT
) STRICT;

CREATE TABLE replica_sync (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    cursor TEXT,
    next_review_sequence INTEGER NOT NULL CHECK (next_review_sequence >= 1),
    result_cursor_hint TEXT,
    snapshot_id TEXT,
    snapshot_server_id TEXT,
    snapshot_cut_cursor INTEGER CHECK (snapshot_cut_cursor >= 0),
    snapshot_cut_token TEXT,
    snapshot_next TEXT,
    snapshot_after INTEGER NOT NULL DEFAULT 0 CHECK (snapshot_after >= 0),
    CHECK ((snapshot_id IS NULL) = (snapshot_server_id IS NULL)),
    CHECK ((snapshot_id IS NULL) = (snapshot_cut_cursor IS NULL)),
    CHECK ((snapshot_id IS NULL) = (snapshot_cut_token IS NULL))
) STRICT;

CREATE TABLE replica_projection (
    entity_kind TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    tombstone INTEGER NOT NULL CHECK (tombstone IN (0, 1)),
    payload_json TEXT NOT NULL,
    PRIMARY KEY (entity_kind, entity_id)
) STRICT;

CREATE TABLE replica_snapshot_projection (
    snapshot_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    entity_kind TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision >= 0),
    tombstone INTEGER NOT NULL CHECK (tombstone IN (0, 1)),
    payload_json TEXT NOT NULL,
    PRIMARY KEY (snapshot_id, ordinal),
    UNIQUE (snapshot_id, entity_kind, entity_id)
) STRICT;

CREATE TABLE replica_snapshot_blob (
    snapshot_id TEXT NOT NULL,
    entity_kind TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    PRIMARY KEY (snapshot_id, entity_kind, entity_id, content_sha256),
    FOREIGN KEY (snapshot_id, entity_kind, entity_id)
        REFERENCES replica_snapshot_projection(snapshot_id, entity_kind, entity_id)
        ON DELETE CASCADE
) STRICT;

CREATE TABLE replica_snapshot_page (
    snapshot_id TEXT NOT NULL,
    request_cursor TEXT NOT NULL,
    response_sha256 TEXT NOT NULL,
    next_cursor TEXT,
    terminal_cursor TEXT,
    PRIMARY KEY (snapshot_id, request_cursor),
    CHECK ((next_cursor IS NULL) <> (terminal_cursor IS NULL))
) STRICT;

CREATE TABLE replica_blob (
    content_sha256 TEXT PRIMARY KEY,
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    bytes BLOB NOT NULL
) STRICT;

CREATE TABLE replica_projection_blob (
    entity_kind TEXT NOT NULL,
    entity_id TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    PRIMARY KEY (entity_kind, entity_id, content_sha256),
    FOREIGN KEY (entity_kind, entity_id)
        REFERENCES replica_projection(entity_kind, entity_id)
        ON DELETE CASCADE
) STRICT;

CREATE TRIGGER replica_blob_immutable_update
BEFORE UPDATE ON replica_blob
BEGIN
    SELECT RAISE(ABORT, 'replica blob is immutable');
END;

CREATE TRIGGER replica_blob_immutable_delete
BEFORE DELETE ON replica_blob
BEGIN
    SELECT RAISE(ABORT, 'replica blob is immutable');
END;

CREATE TABLE replica_review_pack (
    pack_id TEXT PRIMARY KEY,
    session_id TEXT NOT NULL UNIQUE,
    device_id TEXT NOT NULL,
    server_id TEXT NOT NULL,
    cursor TEXT NOT NULL,
    pack_json TEXT NOT NULL
) STRICT;

CREATE TRIGGER replica_review_pack_immutable_update
BEFORE UPDATE ON replica_review_pack
BEGIN
    SELECT RAISE(ABORT, 'replica review pack is immutable');
END;

CREATE TRIGGER replica_review_pack_immutable_delete
BEFORE DELETE ON replica_review_pack
BEGIN
    SELECT RAISE(ABORT, 'replica review pack is immutable');
END;

CREATE TABLE replica_review_progress (
    pack_id TEXT PRIMARY KEY REFERENCES replica_review_pack(pack_id),
    local_position INTEGER NOT NULL CHECK (local_position >= 0),
    status TEXT NOT NULL CHECK (status IN ('active', 'completed', 'abandoned'))
) STRICT;

CREATE UNIQUE INDEX replica_one_active_review_pack
ON replica_review_progress(status) WHERE status = 'active';

CREATE TABLE replica_review_pack_asset (
    pack_id TEXT NOT NULL REFERENCES replica_review_pack(pack_id),
    logical_path TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    media_type TEXT NOT NULL,
    PRIMARY KEY (pack_id, logical_path)
) STRICT;

CREATE TABLE replica_review_operation (
    attempt_id TEXT PRIMARY KEY,
    server_id TEXT NOT NULL,
    device_sequence INTEGER NOT NULL CHECK (device_sequence >= 1),
    pack_id TEXT NOT NULL REFERENCES replica_review_pack(pack_id),
    session_position INTEGER NOT NULL CHECK (session_position >= 0),
    operation_json TEXT NOT NULL,
    UNIQUE (server_id, device_sequence)
) STRICT;

CREATE TRIGGER replica_review_operation_immutable_update
BEFORE UPDATE ON replica_review_operation
BEGIN
    SELECT RAISE(ABORT, 'replica review operation is immutable');
END;

CREATE TRIGGER replica_review_operation_immutable_delete
BEFORE DELETE ON replica_review_operation
BEGIN
    SELECT RAISE(ABORT, 'replica review operation is immutable');
END;

CREATE TABLE replica_review_result (
    result_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    attempt_id TEXT NOT NULL UNIQUE REFERENCES replica_review_operation(attempt_id),
    status TEXT NOT NULL CHECK (status IN ('accepted', 'conflict')),
    conflict_reason TEXT,
    result_json TEXT NOT NULL,
    CHECK ((status = 'accepted') = (conflict_reason IS NULL))
) STRICT;

CREATE TRIGGER replica_review_result_immutable_update
BEFORE UPDATE ON replica_review_result
BEGIN
    SELECT RAISE(ABORT, 'replica review result is immutable');
END;

CREATE TRIGGER replica_review_result_immutable_delete
BEFORE DELETE ON replica_review_result
BEGIN
    SELECT RAISE(ABORT, 'replica review result is immutable');
END;

PRAGMA user_version = 1;
