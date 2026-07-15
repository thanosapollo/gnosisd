ALTER TABLE replica_identity ADD COLUMN offline_asset_put INTEGER
    NOT NULL DEFAULT 0 CHECK (offline_asset_put IN (0, 1));
ALTER TABLE replica_identity ADD COLUMN offline_asset_max_bytes INTEGER
    NOT NULL DEFAULT 0 CHECK (offline_asset_max_bytes >= 0);

CREATE TABLE replica_asset_operation (
    operation_id TEXT PRIMARY KEY REFERENCES replica_device_operation(operation_id),
    logical_path TEXT NOT NULL CHECK (length(logical_path) > 0),
    content_sha256 TEXT NOT NULL CHECK (
        length(content_sha256) = 64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    size_bytes INTEGER NOT NULL CHECK (size_bytes > 0),
    media_type TEXT NOT NULL CHECK (
        length(media_type) > 0 AND length(media_type) <= 255
    ),
    operation_json TEXT NOT NULL,
    FOREIGN KEY (content_sha256) REFERENCES replica_blob(content_sha256)
        ON DELETE RESTRICT
) STRICT;

CREATE TABLE replica_asset_result (
    result_sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    operation_id TEXT NOT NULL UNIQUE REFERENCES replica_asset_operation(operation_id),
    status TEXT NOT NULL CHECK (status IN ('accepted', 'conflict')),
    conflict_reason TEXT CHECK (conflict_reason IS NULL OR conflict_reason IN (
        'cursor_changed', 'base_changed', 'invalid_content', 'path_conflict'
    )),
    result_json TEXT NOT NULL,
    CHECK ((status = 'accepted') = (conflict_reason IS NULL))
) STRICT;

CREATE TRIGGER replica_asset_operation_immutable_update
BEFORE UPDATE ON replica_asset_operation
BEGIN
    SELECT RAISE(ABORT, 'replica asset operation is immutable');
END;

CREATE TRIGGER replica_asset_operation_immutable_delete
BEFORE DELETE ON replica_asset_operation
BEGIN
    SELECT RAISE(ABORT, 'replica asset operation is immutable');
END;

CREATE TRIGGER replica_asset_result_immutable_update
BEFORE UPDATE ON replica_asset_result
BEGIN
    SELECT RAISE(ABORT, 'replica asset result is immutable');
END;

CREATE TRIGGER replica_asset_result_immutable_delete
BEFORE DELETE ON replica_asset_result
BEGIN
    SELECT RAISE(ABORT, 'replica asset result is immutable');
END;

PRAGMA user_version = 4;
