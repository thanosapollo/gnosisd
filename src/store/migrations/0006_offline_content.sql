BEGIN IMMEDIATE;

ALTER TABLE device RENAME COLUMN last_review_sequence TO last_device_sequence;

CREATE TABLE device_operation (
    operation_id TEXT PRIMARY KEY CHECK (length(operation_id) > 0),
    device_id TEXT NOT NULL,
    device_sequence INTEGER NOT NULL CHECK (device_sequence > 0),
    operation_kind TEXT NOT NULL CHECK (operation_kind IN ('review', 'content')),
    format_version INTEGER NOT NULL CHECK (format_version = 1),
    payload_sha256 TEXT NOT NULL CHECK (
        length(payload_sha256) = 64 AND payload_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    payload_json TEXT NOT NULL CHECK (length(payload_json) > 0),
    received_at_us INTEGER NOT NULL CHECK (received_at_us > 0),
    UNIQUE (device_id, device_sequence),
    FOREIGN KEY (device_id) REFERENCES device(device_id) ON DELETE RESTRICT
) STRICT;

INSERT INTO device_operation(
    operation_id, device_id, device_sequence, operation_kind, format_version,
    payload_sha256, payload_json, received_at_us
)
SELECT attempt_id, device_id, device_sequence, 'review', format_version,
       payload_sha256, payload_json, received_at_us
FROM offline_review_operation;

UPDATE device
SET last_device_sequence = max(
    last_device_sequence,
    coalesce((
        SELECT max(device_sequence)
        FROM device_operation
        WHERE device_operation.device_id = device.device_id
    ), 0)
);

CREATE TABLE offline_content_operation (
    operation_id TEXT PRIMARY KEY,
    document_id TEXT NOT NULL CHECK (length(document_id) > 0),
    content_sha256 TEXT NOT NULL CHECK (
        length(content_sha256) = 64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    mutation TEXT NOT NULL CHECK (mutation = 'put_document'),
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
    CHECK ((base_kind = 'revision') = (base_revision IS NOT NULL)),
    CHECK ((base_kind = 'revision') = (base_content_sha256 IS NOT NULL)),
    CHECK (base_kind = 'revision' OR (
        base_content_sha256 IS NULL AND predecessor_operation_id IS NULL
    ))
) STRICT;

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

CREATE TRIGGER device_operation_immutable_update
BEFORE UPDATE ON device_operation
BEGIN
    SELECT RAISE(ABORT, 'device operation is immutable');
END;

CREATE TRIGGER device_operation_immutable_delete
BEFORE DELETE ON device_operation
BEGIN
    SELECT RAISE(ABORT, 'device operation is immutable');
END;

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

CREATE TRIGGER offline_review_operation_immutable_update
BEFORE UPDATE ON offline_review_operation
BEGIN
    SELECT RAISE(ABORT, 'offline review operation is immutable');
END;

CREATE TRIGGER offline_review_operation_immutable_delete
BEFORE DELETE ON offline_review_operation
BEGIN
    SELECT RAISE(ABORT, 'offline review operation is immutable');
END;

CREATE TRIGGER offline_review_result_immutable_update
BEFORE UPDATE ON offline_review_result
BEGIN
    SELECT RAISE(ABORT, 'offline review result is immutable');
END;

CREATE TRIGGER offline_review_result_immutable_delete
BEFORE DELETE ON offline_review_result
BEGIN
    SELECT RAISE(ABORT, 'offline review result is immutable');
END;

INSERT INTO schema_migration(version) VALUES (6);
PRAGMA user_version = 6;

COMMIT;
