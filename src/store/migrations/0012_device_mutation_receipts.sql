BEGIN IMMEDIATE;

CREATE TABLE device_mutation_receipt (
    device_id TEXT NOT NULL,
    operation_id TEXT NOT NULL CHECK (
        length(operation_id) > 0 AND length(operation_id) <= 128
        AND instr(operation_id, char(0)) = 0
        AND substr(operation_id, 1, 1) GLOB '[A-Za-z0-9]'
        AND operation_id NOT GLOB '*[^A-Za-z0-9._:-]*'
    ),
    format_version INTEGER NOT NULL CHECK (format_version = 1),
    mutation_kind TEXT NOT NULL CHECK (mutation_kind IN (
        'document_create',
        'document_replace',
        'document_rename',
        'document_tombstone',
        'asset_create',
        'asset_replace',
        'asset_tombstone',
        'thema_create',
        'thema_replace',
        'thema_archive_set',
        'settings_replace'
    )),
    request_sha256 TEXT NOT NULL CHECK (
        length(request_sha256) = 64
        AND request_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    request_json TEXT NOT NULL CHECK (
        length(request_json) > 0 AND json_valid(request_json)
    ),
    result_kind TEXT NOT NULL CHECK (result_kind IN ('applied', 'rejected')),
    result_json TEXT NOT NULL CHECK (
        length(result_json) > 0 AND json_valid(result_json)
    ),
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0),
    PRIMARY KEY (device_id, operation_id),
    FOREIGN KEY (device_id) REFERENCES device(device_id) ON DELETE RESTRICT
) STRICT;

CREATE TRIGGER device_mutation_receipt_immutable_update
BEFORE UPDATE ON device_mutation_receipt
BEGIN
    SELECT RAISE(ABORT, 'device mutation receipt is immutable');
END;

CREATE TRIGGER device_mutation_receipt_immutable_delete
BEFORE DELETE ON device_mutation_receipt
BEGIN
    SELECT RAISE(ABORT, 'device mutation receipt is immutable');
END;

INSERT INTO schema_migration(version) VALUES (12);
PRAGMA user_version = 12;

COMMIT;
