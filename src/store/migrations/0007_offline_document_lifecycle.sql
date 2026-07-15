BEGIN IMMEDIATE;

PRAGMA defer_foreign_keys = ON;

DROP TRIGGER offline_content_operation_immutable_update;
DROP TRIGGER offline_content_operation_immutable_delete;
DROP TRIGGER offline_content_result_immutable_update;
DROP TRIGGER offline_content_result_immutable_delete;

ALTER TABLE offline_content_result RENAME TO offline_content_result_v6;
ALTER TABLE offline_content_operation RENAME TO offline_content_operation_v6;

CREATE TABLE offline_content_operation (
    operation_id TEXT PRIMARY KEY,
    document_id TEXT NOT NULL CHECK (length(document_id) > 0),
    content_sha256 TEXT CHECK (
        content_sha256 IS NULL OR (
            length(content_sha256) = 64
            AND content_sha256 NOT GLOB '*[^0-9a-f]*'
        )
    ),
    mutation TEXT NOT NULL CHECK (mutation IN (
        'put_document', 'rename_document', 'tombstone_document'
    )),
    base_kind TEXT NOT NULL CHECK (base_kind IN ('absent', 'revision')),
    base_revision INTEGER CHECK (base_revision > 0),
    base_content_sha256 TEXT CHECK (
        base_content_sha256 IS NULL OR (
            length(base_content_sha256) = 64
            AND base_content_sha256 NOT GLOB '*[^0-9a-f]*'
        )
    ),
    predecessor_operation_id TEXT REFERENCES offline_content_operation(operation_id),
    cursor_server_id TEXT NOT NULL CHECK (
        length(cursor_server_id) = 32
        AND cursor_server_id NOT GLOB '*[^0-9a-f]*'
    ),
    cursor INTEGER NOT NULL CHECK (cursor >= 0),
    cursor_token TEXT NOT NULL CHECK (
        length(cursor_token) = 32 AND cursor_token NOT GLOB '*[^0-9a-f]*'
    ),
    authored_at_us INTEGER NOT NULL CHECK (authored_at_us > 0),
    FOREIGN KEY (operation_id) REFERENCES device_operation(operation_id) ON DELETE RESTRICT,
    CHECK ((mutation = 'put_document') = (content_sha256 IS NOT NULL)),
    CHECK ((mutation = 'put_document') OR base_kind = 'revision'),
    CHECK ((base_kind = 'revision') = (base_revision IS NOT NULL)),
    CHECK ((base_kind = 'revision') = (base_content_sha256 IS NOT NULL)),
    CHECK (base_kind = 'revision' OR (
        base_content_sha256 IS NULL AND predecessor_operation_id IS NULL
    ))
) STRICT;

INSERT INTO offline_content_operation(
    operation_id, document_id, content_sha256, mutation, base_kind, base_revision,
    base_content_sha256, predecessor_operation_id, cursor_server_id, cursor,
    cursor_token, authored_at_us
)
SELECT old.operation_id, old.document_id, old.content_sha256, old.mutation,
       old.base_kind, old.base_revision, old.base_content_sha256,
       old.predecessor_operation_id, old.cursor_server_id, old.cursor,
       old.cursor_token, old.authored_at_us
FROM offline_content_operation_v6 AS old
JOIN device_operation AS device USING(operation_id)
ORDER BY device.device_sequence;

CREATE TABLE offline_content_result (
    operation_id TEXT PRIMARY KEY,
    format_version INTEGER NOT NULL CHECK (format_version = 1),
    status TEXT NOT NULL CHECK (status IN ('accepted', 'conflict')),
    conflict_reason TEXT CHECK (conflict_reason IS NULL OR conflict_reason IN (
        'cursor_changed', 'base_changed', 'invalid_content', 'path_conflict'
    )),
    authoritative_json TEXT,
    cursor INTEGER NOT NULL CHECK (cursor >= 0),
    cursor_server_id TEXT NOT NULL CHECK (
        length(cursor_server_id) = 32
        AND cursor_server_id NOT GLOB '*[^0-9a-f]*'
    ),
    cursor_token TEXT NOT NULL CHECK (
        length(cursor_token) = 32 AND cursor_token NOT GLOB '*[^0-9a-f]*'
    ),
    reconciled_at_us INTEGER NOT NULL CHECK (reconciled_at_us > 0),
    FOREIGN KEY (operation_id) REFERENCES offline_content_operation(operation_id)
        ON DELETE RESTRICT,
    CHECK ((status = 'accepted') = (conflict_reason IS NULL))
) STRICT;

INSERT INTO offline_content_result
SELECT * FROM offline_content_result_v6;

DROP TABLE offline_content_result_v6;
DROP TABLE offline_content_operation_v6;

CREATE TRIGGER offline_content_operation_immutable_update
BEFORE UPDATE ON offline_content_operation
BEGIN
    SELECT RAISE(ABORT, 'offline content operation is immutable');
END;

CREATE TRIGGER offline_content_operation_immutable_delete
BEFORE DELETE ON offline_content_operation
BEGIN
    SELECT RAISE(ABORT, 'offline content operation is immutable');
END;

CREATE TRIGGER offline_content_result_immutable_update
BEFORE UPDATE ON offline_content_result
BEGIN
    SELECT RAISE(ABORT, 'offline content result is immutable');
END;

CREATE TRIGGER offline_content_result_immutable_delete
BEFORE DELETE ON offline_content_result
BEGIN
    SELECT RAISE(ABORT, 'offline content result is immutable');
END;

INSERT INTO schema_migration(version) VALUES (7);
PRAGMA user_version = 7;

COMMIT;
