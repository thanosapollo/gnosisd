PRAGMA defer_foreign_keys = ON;

ALTER TABLE replica_identity ADD COLUMN offline_document_lifecycle INTEGER
    NOT NULL DEFAULT 0 CHECK (offline_document_lifecycle IN (0, 1));

DROP TRIGGER replica_content_operation_immutable_update;
DROP TRIGGER replica_content_operation_immutable_delete;
DROP TRIGGER replica_content_result_immutable_update;
DROP TRIGGER replica_content_result_immutable_delete;

ALTER TABLE replica_content_result RENAME TO replica_content_result_v2;
ALTER TABLE replica_content_operation RENAME TO replica_content_operation_v2;

CREATE TABLE replica_content_operation (
    operation_id TEXT PRIMARY KEY REFERENCES replica_device_operation(operation_id),
    document_id TEXT NOT NULL,
    mutation TEXT NOT NULL CHECK (mutation IN (
        'put_document', 'rename_document', 'tombstone_document'
    )),
    logical_path TEXT CHECK (logical_path IS NULL OR length(logical_path) > 0),
    content_sha256 TEXT CHECK (
        content_sha256 IS NULL OR (
            length(content_sha256) = 64
            AND content_sha256 NOT GLOB '*[^0-9a-f]*'
        )
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
    CHECK ((mutation = 'tombstone_document') = (logical_path IS NULL)),
    CHECK ((mutation = 'put_document') = (content_sha256 IS NOT NULL)),
    CHECK ((mutation = 'put_document') OR base_kind = 'revision'),
    CHECK ((base_kind = 'revision') = (base_revision IS NOT NULL)),
    CHECK ((base_kind = 'revision') = (base_content_sha256 IS NOT NULL)),
    CHECK (base_kind = 'revision' OR (
        base_content_sha256 IS NULL AND predecessor_operation_id IS NULL
    ))
) STRICT;

INSERT INTO replica_content_operation(
    operation_id, document_id, mutation, logical_path, content_sha256, base_kind,
    base_revision, base_content_sha256, predecessor_operation_id, operation_json
)
SELECT old.operation_id, old.document_id, 'put_document',
       json_extract(old.operation_json, '$.operation.mutation.logical_path'),
       old.content_sha256, old.base_kind, old.base_revision,
       old.base_content_sha256, old.predecessor_operation_id, old.operation_json
FROM replica_content_operation_v2 AS old
JOIN replica_device_operation AS device USING(operation_id)
ORDER BY device.device_sequence;

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

INSERT INTO replica_content_result
SELECT * FROM replica_content_result_v2;

DROP TABLE replica_content_result_v2;
DROP TABLE replica_content_operation_v2;

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

PRAGMA user_version = 3;
