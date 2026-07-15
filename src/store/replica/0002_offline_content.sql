ALTER TABLE replica_sync RENAME COLUMN next_review_sequence TO next_device_sequence;

CREATE TABLE replica_device_operation (
    operation_id TEXT PRIMARY KEY,
    server_id TEXT NOT NULL,
    device_sequence INTEGER NOT NULL CHECK (device_sequence >= 1),
    operation_kind TEXT NOT NULL CHECK (operation_kind IN ('review', 'content')),
    operation_json TEXT NOT NULL,
    UNIQUE (server_id, device_sequence)
) STRICT;

INSERT INTO replica_device_operation(
    operation_id, server_id, device_sequence, operation_kind, operation_json
)
SELECT attempt_id, server_id, device_sequence, 'review', operation_json
FROM replica_review_operation;

CREATE TABLE replica_content_operation (
    operation_id TEXT PRIMARY KEY REFERENCES replica_device_operation(operation_id),
    document_id TEXT NOT NULL,
    content_sha256 TEXT NOT NULL CHECK (
        length(content_sha256) = 64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    base_kind TEXT NOT NULL CHECK (base_kind IN ('absent', 'revision')),
    base_revision INTEGER CHECK (base_revision > 0),
    base_content_sha256 TEXT CHECK (
        base_content_sha256 IS NULL OR (
            length(base_content_sha256) = 64
            AND base_content_sha256 NOT GLOB '*[^0-9a-f]*'
        )
    ),
    predecessor_operation_id TEXT REFERENCES replica_content_operation(operation_id),
    operation_json TEXT NOT NULL,
    CHECK ((base_kind = 'revision') = (base_revision IS NOT NULL)),
    CHECK ((base_kind = 'revision') = (base_content_sha256 IS NOT NULL)),
    CHECK (base_kind = 'revision' OR (
        base_content_sha256 IS NULL AND predecessor_operation_id IS NULL
    ))
) STRICT;

CREATE TRIGGER replica_device_operation_immutable_update
BEFORE UPDATE ON replica_device_operation
BEGIN
    SELECT RAISE(ABORT, 'replica device operation is immutable');
END;

CREATE TRIGGER replica_device_operation_immutable_delete
BEFORE DELETE ON replica_device_operation
BEGIN
    SELECT RAISE(ABORT, 'replica device operation is immutable');
END;

CREATE TRIGGER replica_content_operation_immutable_update
BEFORE UPDATE ON replica_content_operation
BEGIN
    SELECT RAISE(ABORT, 'replica content operation is immutable');
END;

CREATE TRIGGER replica_content_operation_immutable_delete
BEFORE DELETE ON replica_content_operation
BEGIN
    SELECT RAISE(ABORT, 'replica content operation is immutable');
END;

CREATE TABLE replica_content_result (
    result_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    operation_id TEXT NOT NULL UNIQUE REFERENCES replica_content_operation(operation_id),
    status TEXT NOT NULL CHECK (status IN ('accepted', 'conflict')),
    conflict_reason TEXT CHECK (conflict_reason IS NULL OR conflict_reason IN (
        'cursor_changed', 'base_changed', 'invalid_content', 'path_conflict'
    )),
    result_json TEXT NOT NULL,
    CHECK ((status = 'accepted') = (conflict_reason IS NULL))
) STRICT;

CREATE TRIGGER replica_content_result_immutable_update
BEFORE UPDATE ON replica_content_result
BEGIN
    SELECT RAISE(ABORT, 'replica content result is immutable');
END;

CREATE TRIGGER replica_content_result_immutable_delete
BEFORE DELETE ON replica_content_result
BEGIN
    SELECT RAISE(ABORT, 'replica content result is immutable');
END;

PRAGMA user_version = 2;
