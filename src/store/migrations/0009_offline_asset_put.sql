BEGIN IMMEDIATE;

CREATE TABLE staged_blob (
    content_sha256 TEXT PRIMARY KEY CHECK (
        length(content_sha256) = 64
        AND content_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    size_bytes INTEGER NOT NULL CHECK (size_bytes > 0),
    staged_at_us INTEGER NOT NULL CHECK (staged_at_us > 0)
) STRICT;

CREATE TABLE offline_asset_operation (
    operation_id TEXT PRIMARY KEY,
    logical_path TEXT NOT NULL CHECK (length(logical_path) > 0),
    original_name TEXT NOT NULL CHECK (length(original_name) > 0),
    content_sha256 TEXT NOT NULL CHECK (
        length(content_sha256) = 64
        AND content_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    size_bytes INTEGER NOT NULL CHECK (size_bytes > 0),
    media_type TEXT NOT NULL CHECK (
        length(media_type) > 0 AND length(media_type) <= 255
    ),
    cursor_server_id TEXT NOT NULL CHECK (
        length(cursor_server_id) = 32
        AND cursor_server_id NOT GLOB '*[^0-9a-f]*'
    ),
    cursor INTEGER NOT NULL CHECK (cursor >= 0),
    cursor_token TEXT NOT NULL CHECK (
        length(cursor_token) = 32 AND cursor_token NOT GLOB '*[^0-9a-f]*'
    ),
    authored_at_us INTEGER NOT NULL CHECK (authored_at_us > 0),
    FOREIGN KEY (operation_id) REFERENCES device_operation(operation_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE offline_asset_result (
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
    FOREIGN KEY (operation_id) REFERENCES offline_asset_operation(operation_id)
        ON DELETE RESTRICT,
    CHECK ((status = 'accepted') = (conflict_reason IS NULL))
) STRICT;

CREATE TABLE offline_asset_evidence (
    operation_id TEXT PRIMARY KEY REFERENCES offline_asset_operation(operation_id)
        ON DELETE RESTRICT,
    content_sha256 TEXT NOT NULL CHECK (
        length(content_sha256) = 64
        AND content_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    size_bytes INTEGER NOT NULL CHECK (size_bytes > 0),
    media_type TEXT NOT NULL CHECK (
        length(media_type) > 0 AND length(media_type) <= 255
    ),
    retained_at_us INTEGER NOT NULL CHECK (retained_at_us > 0)
) STRICT;

CREATE TRIGGER offline_asset_operation_immutable_update
BEFORE UPDATE ON offline_asset_operation
BEGIN
    SELECT RAISE(ABORT, 'offline asset operation is immutable');
END;

CREATE TRIGGER offline_asset_operation_immutable_delete
BEFORE DELETE ON offline_asset_operation
BEGIN
    SELECT RAISE(ABORT, 'offline asset operation is immutable');
END;

CREATE TRIGGER offline_asset_result_immutable_update
BEFORE UPDATE ON offline_asset_result
BEGIN
    SELECT RAISE(ABORT, 'offline asset result is immutable');
END;

CREATE TRIGGER offline_asset_result_immutable_delete
BEFORE DELETE ON offline_asset_result
BEGIN
    SELECT RAISE(ABORT, 'offline asset result is immutable');
END;

CREATE TRIGGER offline_asset_evidence_immutable_update
BEFORE UPDATE ON offline_asset_evidence
BEGIN
    SELECT RAISE(ABORT, 'offline asset evidence is immutable');
END;

CREATE TRIGGER offline_asset_evidence_immutable_delete
BEFORE DELETE ON offline_asset_evidence
BEGIN
    SELECT RAISE(ABORT, 'offline asset evidence is immutable');
END;

INSERT INTO schema_migration(version) VALUES (9);
PRAGMA user_version = 9;

COMMIT;
