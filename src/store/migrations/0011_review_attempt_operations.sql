BEGIN IMMEDIATE;

ALTER TABLE review_attempt ADD COLUMN mutation_revision INTEGER NOT NULL DEFAULT 0
    CHECK (mutation_revision >= 0);

CREATE TABLE review_attempt_operation (
    operation_id TEXT NOT NULL CHECK (
        length(operation_id) > 0 AND length(operation_id) <= 128
        AND instr(operation_id, char(0)) = 0
        AND substr(operation_id, 1, 1) GLOB '[A-Za-z0-9]'
        AND operation_id NOT GLOB '*[^A-Za-z0-9._:-]*'
    ),
    format_version INTEGER NOT NULL CHECK (format_version = 1),
    actor_kind TEXT NOT NULL CHECK (actor_kind IN ('local', 'device')),
    actor_device_id TEXT,
    attempt_id TEXT NOT NULL,
    operation_kind TEXT NOT NULL CHECK (operation_kind IN (
        'accepted_success_set', 'suspension_set'
    )),
    expected_revision INTEGER NOT NULL CHECK (expected_revision >= 0),
    desired_value INTEGER NOT NULL CHECK (desired_value IN (0, 1)),
    payload_sha256 TEXT NOT NULL CHECK (
        length(payload_sha256) = 64
        AND payload_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    payload_json TEXT NOT NULL CHECK (length(payload_json) > 0),
    result_kind TEXT NOT NULL CHECK (result_kind IN (
        'accepted', 'revision_conflict', 'stale_attempt', 'state_conflict'
    )),
    result_json TEXT NOT NULL CHECK (length(result_json) > 0),
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0),
    FOREIGN KEY (actor_device_id) REFERENCES device(device_id) ON DELETE RESTRICT,
    FOREIGN KEY (attempt_id) REFERENCES review_attempt(attempt_id) ON DELETE RESTRICT,
    CHECK (
        (actor_kind = 'local' AND actor_device_id IS NULL)
        OR (actor_kind = 'device' AND actor_device_id IS NOT NULL)
    )
) STRICT;

CREATE UNIQUE INDEX review_attempt_operation_local_identity
ON review_attempt_operation(operation_id)
WHERE actor_kind = 'local';

CREATE UNIQUE INDEX review_attempt_operation_device_identity
ON review_attempt_operation(actor_device_id, operation_id)
WHERE actor_kind = 'device';

CREATE INDEX review_attempt_operation_attempt
ON review_attempt_operation(attempt_id, created_at_us, operation_id);

CREATE TRIGGER review_attempt_operation_immutable_update
BEFORE UPDATE ON review_attempt_operation
BEGIN
    SELECT RAISE(ABORT, 'review attempt operation is immutable');
END;

CREATE TRIGGER review_attempt_operation_immutable_delete
BEFORE DELETE ON review_attempt_operation
BEGIN
    SELECT RAISE(ABORT, 'review attempt operation is immutable');
END;

INSERT INTO schema_migration(version) VALUES (11);
PRAGMA user_version = 11;

COMMIT;
