BEGIN IMMEDIATE;

CREATE TABLE local_review_replay (
    request_id TEXT PRIMARY KEY CHECK (
        length(request_id) > 0 AND length(request_id) <= 128
    ),
    request_sha256 TEXT NOT NULL CHECK (
        length(request_sha256) = 64
        AND request_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    attempt_id TEXT NOT NULL,
    action TEXT NOT NULL CHECK (action IN ('override', 'suspension_toggle')),
    result_json TEXT NOT NULL CHECK (length(result_json) > 0),
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0),
    FOREIGN KEY (attempt_id) REFERENCES review_attempt(attempt_id) ON DELETE RESTRICT
) STRICT;

CREATE TRIGGER local_review_replay_immutable_update
BEFORE UPDATE ON local_review_replay
BEGIN
    SELECT RAISE(ABORT, 'local review replay is immutable');
END;

CREATE TRIGGER local_review_replay_immutable_delete
BEFORE DELETE ON local_review_replay
BEGIN
    SELECT RAISE(ABORT, 'local review replay is immutable');
END;

INSERT INTO schema_migration(version) VALUES (10);
PRAGMA user_version = 10;

COMMIT;
