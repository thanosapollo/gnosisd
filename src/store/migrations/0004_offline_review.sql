BEGIN IMMEDIATE;

ALTER TABLE review_session ADD COLUMN owner_device_id TEXT
    REFERENCES device(device_id) ON DELETE RESTRICT;
ALTER TABLE review_session ADD COLUMN review_mode TEXT NOT NULL DEFAULT 'online'
    CHECK (review_mode IN ('online', 'offline'));
ALTER TABLE device ADD COLUMN last_review_sequence INTEGER NOT NULL DEFAULT 0
    CHECK (last_review_sequence >= 0);

CREATE TABLE review_pack (
    pack_id TEXT PRIMARY KEY CHECK (length(pack_id) > 0),
    format_version INTEGER NOT NULL CHECK (format_version = 1),
    session_id TEXT NOT NULL UNIQUE,
    device_id TEXT NOT NULL,
    lease_id TEXT NOT NULL UNIQUE CHECK (length(lease_id) > 0),
    status TEXT NOT NULL CHECK (status IN ('active', 'completed', 'abandoned')),
    cursor INTEGER NOT NULL CHECK (cursor >= 0),
    issued_at_us INTEGER NOT NULL CHECK (issued_at_us > 0),
    FOREIGN KEY (session_id) REFERENCES review_session(session_id) ON DELETE RESTRICT,
    FOREIGN KEY (device_id) REFERENCES device(device_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE review_pack_item (
    pack_id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    thema_id TEXT NOT NULL,
    item_json TEXT NOT NULL CHECK (length(item_json) > 0),
    PRIMARY KEY (pack_id, position),
    FOREIGN KEY (pack_id) REFERENCES review_pack(pack_id) ON DELETE RESTRICT,
    FOREIGN KEY (thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE review_pack_asset (
    pack_id TEXT NOT NULL,
    asset_path TEXT NOT NULL,
    content_sha256 TEXT NOT NULL
        CHECK (length(content_sha256) = 64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    media_type TEXT NOT NULL CHECK (length(media_type) > 0),
    PRIMARY KEY (pack_id, asset_path),
    FOREIGN KEY (pack_id) REFERENCES review_pack(pack_id) ON DELETE RESTRICT,
    FOREIGN KEY (asset_path) REFERENCES managed_asset(asset_path) ON DELETE RESTRICT,
    FOREIGN KEY (content_sha256) REFERENCES blob(content_sha256) ON DELETE RESTRICT
) STRICT;

CREATE TABLE offline_review_operation (
    attempt_id TEXT PRIMARY KEY CHECK (length(attempt_id) > 0),
    pack_id TEXT NOT NULL,
    device_id TEXT NOT NULL,
    device_sequence INTEGER NOT NULL CHECK (device_sequence > 0),
    format_version INTEGER NOT NULL CHECK (format_version = 1),
    payload_sha256 TEXT NOT NULL UNIQUE
        CHECK (length(payload_sha256) = 64 AND payload_sha256 NOT GLOB '*[^0-9a-f]*'),
    payload_json TEXT NOT NULL CHECK (length(payload_json) > 0),
    received_at_us INTEGER NOT NULL CHECK (received_at_us > 0),
    UNIQUE (device_id, device_sequence),
    FOREIGN KEY (pack_id) REFERENCES review_pack(pack_id) ON DELETE RESTRICT,
    FOREIGN KEY (device_id) REFERENCES device(device_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE offline_review_result (
    attempt_id TEXT PRIMARY KEY,
    format_version INTEGER NOT NULL CHECK (format_version = 1),
    status TEXT NOT NULL CHECK (status IN ('accepted', 'conflict')),
    conflict_reason TEXT CHECK (conflict_reason IS NULL OR conflict_reason IN (
        'lease_ended', 'session_advanced', 'content_changed',
        'scheduling_changed', 'capture_mismatch'
    )),
    event_id TEXT,
    authoritative_state_json TEXT NOT NULL,
    session_json TEXT NOT NULL,
    cursor INTEGER NOT NULL CHECK (cursor >= 0),
    reconciled_at_us INTEGER NOT NULL CHECK (reconciled_at_us > 0),
    FOREIGN KEY (attempt_id) REFERENCES offline_review_operation(attempt_id) ON DELETE RESTRICT,
    FOREIGN KEY (event_id) REFERENCES review_event(event_id) ON DELETE RESTRICT,
    CHECK ((status = 'accepted') = (event_id IS NOT NULL)),
    CHECK ((status = 'conflict') = (conflict_reason IS NOT NULL))
) STRICT;

ALTER TABLE content_change RENAME TO content_change_v3;

CREATE TABLE content_change (
    cursor INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_kind TEXT NOT NULL CHECK (entity_kind IN (
        'document', 'asset', 'thema', 'settings', 'review'
    )),
    entity_id TEXT NOT NULL CHECK (length(entity_id) > 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    operation TEXT NOT NULL CHECK (length(operation) > 0),
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0)
) STRICT;

INSERT INTO content_change(
    cursor, entity_kind, entity_id, revision, operation, created_at_us
)
SELECT cursor, entity_kind, entity_id, revision, operation, created_at_us
FROM content_change_v3 ORDER BY cursor;

DROP TABLE content_change_v3;

INSERT INTO schema_migration(version) VALUES (4);
PRAGMA user_version = 4;

COMMIT;
