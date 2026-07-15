BEGIN IMMEDIATE;

CREATE TABLE server_identity (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    server_id TEXT NOT NULL UNIQUE CHECK (
        length(server_id) = 32 AND server_id NOT GLOB '*[^0-9a-f]*'
    )
) STRICT;

INSERT INTO server_identity(id, server_id)
VALUES (1, lower(hex(randomblob(16))));

ALTER TABLE content_change RENAME TO content_change_v4;

CREATE TABLE content_change (
    cursor INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_kind TEXT NOT NULL CHECK (entity_kind IN (
        'document', 'asset', 'thema', 'settings', 'review'
    )),
    entity_id TEXT NOT NULL CHECK (length(entity_id) > 0),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    operation TEXT NOT NULL CHECK (length(operation) > 0),
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0),
    feed_token TEXT NOT NULL UNIQUE DEFAULT (lower(hex(randomblob(16)))) CHECK (
        length(feed_token) = 32 AND feed_token NOT GLOB '*[^0-9a-f]*'
    )
) STRICT;

INSERT INTO content_change(
    cursor, entity_kind, entity_id, revision, operation, created_at_us
)
SELECT cursor, entity_kind, entity_id, revision, operation, created_at_us
FROM content_change_v4 ORDER BY cursor;

DROP TABLE content_change_v4;

CREATE TABLE sync_feed_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    retained_after_cursor INTEGER NOT NULL DEFAULT 0
        CHECK (retained_after_cursor >= 0),
    retained_after_token TEXT NOT NULL CHECK (
        length(retained_after_token) = 32
        AND retained_after_token NOT GLOB '*[^0-9a-f]*'
    )
) STRICT;

INSERT INTO sync_feed_state(
    id, retained_after_cursor, retained_after_token
)
SELECT 1,
       coalesce(max(cursor), 0),
       coalesce(
           (SELECT feed_token FROM content_change ORDER BY cursor DESC LIMIT 1),
           lower(hex(randomblob(16)))
       )
FROM content_change;

ALTER TABLE document_revision RENAME TO document_revision_v4;

CREATE TABLE document_revision (
    document_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    logical_path TEXT NOT NULL CHECK (length(logical_path) > 0),
    blob_sha256 TEXT,
    operation TEXT NOT NULL CHECK (operation IN (
        'create', 'update', 'rename', 'tombstone', 'snapshot'
    )),
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0),
    author_device_id TEXT,
    PRIMARY KEY (document_id, revision),
    FOREIGN KEY (document_id) REFERENCES document(document_id) ON DELETE RESTRICT,
    FOREIGN KEY (blob_sha256) REFERENCES blob(content_sha256) ON DELETE RESTRICT,
    CHECK ((operation IN ('tombstone', 'snapshot')) = (blob_sha256 IS NULL))
) STRICT;

INSERT INTO document_revision(
    document_id, revision, logical_path, blob_sha256, operation,
    created_at_us, author_device_id
)
SELECT document_id, revision, logical_path, blob_sha256, operation,
       created_at_us, author_device_id
FROM document_revision_v4;

DROP TABLE document_revision_v4;

ALTER TABLE review_pack ADD COLUMN cursor_server_id TEXT;
ALTER TABLE review_pack ADD COLUMN cursor_token TEXT;

UPDATE review_pack
SET cursor_server_id = (SELECT server_id FROM server_identity WHERE id = 1),
    cursor_token = coalesce(
        (SELECT feed_token FROM content_change
         WHERE content_change.cursor = review_pack.cursor),
        (SELECT retained_after_token FROM sync_feed_state WHERE id = 1)
    );

ALTER TABLE offline_review_result ADD COLUMN cursor_server_id TEXT;
ALTER TABLE offline_review_result ADD COLUMN cursor_token TEXT;

UPDATE offline_review_result
SET cursor_server_id = (SELECT server_id FROM server_identity WHERE id = 1),
    cursor_token = coalesce(
        (SELECT feed_token FROM content_change
         WHERE content_change.cursor = offline_review_result.cursor),
        (SELECT retained_after_token FROM sync_feed_state WHERE id = 1)
    );

CREATE TRIGGER review_pack_cursor_required_insert
BEFORE INSERT ON review_pack
WHEN NEW.cursor_server_id IS NULL OR NEW.cursor_token IS NULL
BEGIN
    SELECT RAISE(ABORT, 'review pack cursor identity is required');
END;

CREATE TRIGGER review_pack_cursor_required_update
BEFORE UPDATE OF cursor_server_id, cursor_token ON review_pack
WHEN NEW.cursor_server_id IS NULL OR NEW.cursor_token IS NULL
BEGIN
    SELECT RAISE(ABORT, 'review pack cursor identity is required');
END;

CREATE TRIGGER offline_result_cursor_required_insert
BEFORE INSERT ON offline_review_result
WHEN NEW.cursor_server_id IS NULL OR NEW.cursor_token IS NULL
BEGIN
    SELECT RAISE(ABORT, 'offline result cursor identity is required');
END;

CREATE TRIGGER offline_result_cursor_required_update
BEFORE UPDATE OF cursor_server_id, cursor_token ON offline_review_result
WHEN NEW.cursor_server_id IS NULL OR NEW.cursor_token IS NULL
BEGIN
    SELECT RAISE(ABORT, 'offline result cursor identity is required');
END;

CREATE TABLE replica_snapshot (
    snapshot_id TEXT PRIMARY KEY CHECK (length(snapshot_id) > 0),
    device_id TEXT NOT NULL UNIQUE,
    server_id TEXT NOT NULL,
    cut_cursor INTEGER NOT NULL CHECK (cut_cursor >= 0),
    cut_token TEXT NOT NULL CHECK (
        length(cut_token) = 32 AND cut_token NOT GLOB '*[^0-9a-f]*'
    ),
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0),
    FOREIGN KEY (device_id) REFERENCES device(device_id) ON DELETE RESTRICT,
    FOREIGN KEY (server_id) REFERENCES server_identity(server_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE replica_snapshot_item (
    snapshot_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    entity_kind TEXT NOT NULL CHECK (entity_kind IN (
        'document', 'asset', 'thema', 'settings', 'review'
    )),
    entity_id TEXT NOT NULL CHECK (length(entity_id) > 0),
    revision INTEGER NOT NULL CHECK (revision >= 0),
    PRIMARY KEY (snapshot_id, ordinal),
    UNIQUE (snapshot_id, entity_kind, entity_id),
    FOREIGN KEY (snapshot_id) REFERENCES replica_snapshot(snapshot_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE thema_revision (
    thema_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    kind TEXT NOT NULL CHECK (kind IN ('basic', 'cloze', 'mcq', 'mc-cloze')),
    keimenon TEXT NOT NULL,
    hypothesis_json TEXT NOT NULL,
    answer_json TEXT NOT NULL,
    source_guid TEXT,
    parathema TEXT,
    review_image TEXT,
    tags_json TEXT NOT NULL,
    node_ids_json TEXT NOT NULL,
    asset_paths_json TEXT NOT NULL,
    content_sha256 TEXT NOT NULL CHECK (
        length(content_sha256) = 64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'
    ),
    archived INTEGER NOT NULL CHECK (archived IN (0, 1)),
    operation TEXT NOT NULL CHECK (operation IN (
        'create', 'update', 'archive', 'unarchive', 'snapshot'
    )),
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    author_device_id TEXT,
    PRIMARY KEY (thema_id, revision),
    FOREIGN KEY (thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT,
    FOREIGN KEY (author_device_id) REFERENCES device(device_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE document_projection_revision (
    document_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    projection_json TEXT NOT NULL,
    PRIMARY KEY (document_id, revision),
    FOREIGN KEY (document_id, revision)
        REFERENCES document_revision(document_id, revision) ON DELETE RESTRICT
) STRICT;

INSERT INTO thema_revision(
    thema_id, revision, kind, keimenon, hypothesis_json, answer_json,
    source_guid, parathema, review_image, tags_json, node_ids_json,
    asset_paths_json, content_sha256, archived, operation, created_at_us
)
SELECT
    thema.thema_id,
    thema.revision,
    thema.kind,
    thema.keimenon,
    thema.hypothesis_json,
    thema.answer_json,
    thema.source_guid,
    thema.parathema,
    thema.review_image,
    coalesce((
        SELECT json_group_array(tag)
        FROM (SELECT tag FROM thema_tag WHERE thema_id = thema.thema_id ORDER BY tag)
    ), '[]'),
    coalesce((
        SELECT json_group_array(node_id)
        FROM (
            SELECT node_id FROM thema_node_link
            WHERE thema_id = thema.thema_id ORDER BY node_id
        )
    ), '[]'),
    coalesce((
        SELECT json_group_array(asset_path)
        FROM (
            SELECT asset_path FROM thema_asset
            WHERE thema_id = thema.thema_id ORDER BY asset_path
        )
    ), '[]'),
    thema.content_sha256,
    thema.archived_at_us IS NOT NULL,
    'snapshot',
    thema.updated_at_us
FROM thema;

CREATE TABLE settings_revision (
    revision INTEGER PRIMARY KEY CHECK (revision > 0),
    timezone TEXT NOT NULL,
    day_boundary_hour INTEGER NOT NULL CHECK (day_boundary_hour BETWEEN 0 AND 23),
    scheduler_version INTEGER NOT NULL CHECK (scheduler_version >= 1),
    scheduler_json TEXT NOT NULL,
    tag_rules_json TEXT NOT NULL,
    operation TEXT NOT NULL CHECK (operation IN ('update', 'snapshot')),
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    author_device_id TEXT,
    FOREIGN KEY (author_device_id) REFERENCES device(device_id) ON DELETE RESTRICT
) STRICT;

INSERT INTO settings_revision(
    revision, timezone, day_boundary_hour, scheduler_version, scheduler_json,
    tag_rules_json, operation, created_at_us
)
SELECT
    settings.revision,
    settings.timezone,
    settings.day_boundary_hour,
    settings.scheduler_version,
    settings.scheduler_json,
    coalesce((
        SELECT json_group_object(tag, json(parameters_json))
        FROM (SELECT tag, parameters_json FROM tag_rule ORDER BY tag)
    ), '{}'),
    'snapshot',
    settings.updated_at_us
FROM settings WHERE id = 1;

CREATE TABLE review_state_revision (
    thema_id TEXT NOT NULL,
    state_revision INTEGER NOT NULL CHECK (state_revision >= 0),
    state_json TEXT NOT NULL,
    event_id TEXT,
    created_at_us INTEGER NOT NULL CHECK (created_at_us >= 0),
    PRIMARY KEY (thema_id, state_revision),
    FOREIGN KEY (thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT,
    FOREIGN KEY (event_id) REFERENCES review_event(event_id) ON DELETE RESTRICT
) STRICT;

INSERT INTO review_state_revision(
    thema_id, state_revision, state_json, event_id, created_at_us
)
SELECT
    state.thema_id,
    state.state_revision,
    json_object(
        'state_revision', state.state_revision,
        'review_count', state.review_count,
        'legacy_review_count', state.legacy_review_count,
        'legacy_unclassified_count', state.legacy_unclassified_count,
        'last_reviewed_at_us', state.last_reviewed_at_us,
        'last_review_day', state.last_review_day,
        'next_review_day', state.next_review_day,
        'interval_days', state.interval_days,
        'consecutive_successes', state.consecutive_successes,
        'total_successes', state.total_successes,
        'consecutive_failures', state.consecutive_failures,
        'total_failures', state.total_failures,
        'suspended', json(CASE state.suspended WHEN 1 THEN 'true' ELSE 'false' END),
        'gnosis_plus', state.gnosis_plus,
        'gnosis_minus', state.gnosis_minus,
        'gnosis_synolon', state.gnosis_synolon
    ),
    NULL,
    coalesce(state.last_reviewed_at_us, 0)
FROM review_state AS state;

INSERT INTO schema_migration(version) VALUES (5);
PRAGMA user_version = 5;

COMMIT;
