BEGIN IMMEDIATE;

ALTER TABLE document ADD COLUMN logical_path TEXT;
ALTER TABLE document ADD COLUMN path_key TEXT;
ALTER TABLE document ADD COLUMN head_blob_sha256 TEXT;
ALTER TABLE document ADD COLUMN active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1));
ALTER TABLE document ADD COLUMN updated_at_us INTEGER NOT NULL DEFAULT 0 CHECK (updated_at_us >= 0);

UPDATE document
SET logical_path = document_id || '.org',
    path_key = lower(document_id || '.org')
WHERE logical_path IS NULL;

CREATE UNIQUE INDEX document_logical_path_unique
ON document(logical_path)
WHERE logical_path IS NOT NULL;

CREATE UNIQUE INDEX document_path_key_unique
ON document(path_key)
WHERE path_key IS NOT NULL;

ALTER TABLE node ADD COLUMN active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1));

ALTER TABLE thema ADD COLUMN legacy_id INTEGER;
ALTER TABLE thema ADD COLUMN revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0);
ALTER TABLE thema ADD COLUMN kind TEXT NOT NULL DEFAULT 'basic'
    CHECK (kind IN ('basic', 'cloze', 'mcq', 'mc-cloze'));
ALTER TABLE thema ADD COLUMN keimenon TEXT NOT NULL DEFAULT '';
ALTER TABLE thema ADD COLUMN hypothesis_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE thema ADD COLUMN answer_json TEXT NOT NULL DEFAULT '[]';
ALTER TABLE thema ADD COLUMN source_guid TEXT;
ALTER TABLE thema ADD COLUMN archived_at_us INTEGER CHECK (archived_at_us IS NULL OR archived_at_us > 0);
ALTER TABLE thema ADD COLUMN parathema TEXT;
ALTER TABLE thema ADD COLUMN review_image TEXT;
ALTER TABLE thema ADD COLUMN content_sha256 TEXT NOT NULL DEFAULT
    '0000000000000000000000000000000000000000000000000000000000000000';
ALTER TABLE thema ADD COLUMN created_at_us INTEGER NOT NULL DEFAULT 0 CHECK (created_at_us >= 0);
ALTER TABLE thema ADD COLUMN updated_at_us INTEGER NOT NULL DEFAULT 0 CHECK (updated_at_us >= 0);

CREATE UNIQUE INDEX thema_legacy_id_unique
ON thema(legacy_id)
WHERE legacy_id IS NOT NULL;

CREATE TABLE blob (
    content_sha256 TEXT PRIMARY KEY
        CHECK (length(content_sha256) = 64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    media_type TEXT NOT NULL CHECK (length(media_type) > 0),
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0)
) STRICT;

CREATE TABLE document_revision (
    document_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    logical_path TEXT NOT NULL CHECK (length(logical_path) > 0),
    blob_sha256 TEXT,
    operation TEXT NOT NULL CHECK (operation IN ('create', 'update', 'rename', 'tombstone')),
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0),
    author_device_id TEXT,
    PRIMARY KEY (document_id, revision),
    FOREIGN KEY (document_id) REFERENCES document(document_id) ON DELETE RESTRICT,
    FOREIGN KEY (blob_sha256) REFERENCES blob(content_sha256) ON DELETE RESTRICT,
    CHECK ((operation = 'tombstone') = (blob_sha256 IS NULL))
) STRICT;

CREATE TABLE content_change (
    cursor INTEGER PRIMARY KEY AUTOINCREMENT,
    entity_kind TEXT NOT NULL CHECK (entity_kind IN ('document', 'asset', 'thema', 'settings')),
    entity_id TEXT NOT NULL CHECK (length(entity_id) > 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    operation TEXT NOT NULL CHECK (length(operation) > 0),
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0)
) STRICT;

CREATE TABLE managed_asset (
    asset_path TEXT PRIMARY KEY CHECK (length(asset_path) > 0),
    path_key TEXT NOT NULL UNIQUE CHECK (length(path_key) > 0),
    blob_sha256 TEXT NOT NULL,
    original_name TEXT NOT NULL CHECK (length(original_name) > 0),
    revision INTEGER NOT NULL CHECK (revision > 0),
    active INTEGER NOT NULL DEFAULT 1 CHECK (active IN (0, 1)),
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0),
    updated_at_us INTEGER NOT NULL CHECK (updated_at_us > 0),
    FOREIGN KEY (blob_sha256) REFERENCES blob(content_sha256) ON DELETE RESTRICT
) STRICT;

CREATE TABLE asset_revision (
    asset_path TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    blob_sha256 TEXT,
    operation TEXT NOT NULL CHECK (operation IN ('create', 'update', 'tombstone')),
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0),
    author_device_id TEXT,
    PRIMARY KEY (asset_path, revision),
    FOREIGN KEY (blob_sha256) REFERENCES blob(content_sha256) ON DELETE RESTRICT,
    CHECK ((operation = 'tombstone') = (blob_sha256 IS NULL))
) STRICT;

CREATE TABLE retired_node (
    node_id TEXT PRIMARY KEY,
    document_id TEXT NOT NULL,
    logical_path TEXT NOT NULL,
    title TEXT NOT NULL,
    title_path_json TEXT NOT NULL,
    tags_json TEXT NOT NULL,
    level INTEGER NOT NULL CHECK (level >= 0),
    retired_at_revision INTEGER NOT NULL CHECK (retired_at_revision > 0),
    FOREIGN KEY (document_id) REFERENCES document(document_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE thema_tag (
    thema_id TEXT NOT NULL,
    tag TEXT NOT NULL CHECK (length(tag) > 0),
    PRIMARY KEY (thema_id, tag),
    FOREIGN KEY (thema_id) REFERENCES thema(thema_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE thema_node_link (
    thema_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    relation TEXT NOT NULL DEFAULT 'supporting'
        CHECK (relation IN ('primary', 'supporting', 'reference')),
    PRIMARY KEY (thema_id, node_id),
    FOREIGN KEY (thema_id) REFERENCES thema(thema_id) ON DELETE CASCADE
) STRICT;

CREATE TABLE thema_asset (
    thema_id TEXT NOT NULL,
    asset_path TEXT NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('content', 'review_image')),
    PRIMARY KEY (thema_id, asset_path, role),
    FOREIGN KEY (thema_id) REFERENCES thema(thema_id) ON DELETE CASCADE,
    FOREIGN KEY (asset_path) REFERENCES managed_asset(asset_path) ON DELETE RESTRICT
) STRICT;

CREATE TABLE review_state (
    thema_id TEXT PRIMARY KEY,
    state_revision INTEGER NOT NULL CHECK (state_revision >= 0),
    review_count INTEGER NOT NULL CHECK (review_count >= 0),
    legacy_review_count INTEGER NOT NULL CHECK (legacy_review_count >= 0),
    legacy_unclassified_count INTEGER NOT NULL CHECK (legacy_unclassified_count >= 0),
    last_reviewed_at_us INTEGER CHECK (last_reviewed_at_us IS NULL OR last_reviewed_at_us > 0),
    last_review_day INTEGER,
    next_review_day INTEGER NOT NULL,
    interval_days INTEGER CHECK (interval_days IS NULL OR interval_days >= 0),
    consecutive_successes INTEGER NOT NULL CHECK (consecutive_successes >= 0),
    total_successes INTEGER NOT NULL CHECK (total_successes >= 0),
    consecutive_failures INTEGER NOT NULL CHECK (consecutive_failures >= 0),
    total_failures INTEGER NOT NULL CHECK (total_failures >= 0),
    suspended INTEGER NOT NULL CHECK (suspended IN (0, 1)),
    gnosis_plus REAL NOT NULL CHECK (gnosis_plus >= 0),
    gnosis_minus REAL NOT NULL CHECK (gnosis_minus >= 0),
    gnosis_synolon REAL NOT NULL CHECK (gnosis_synolon >= 1.3),
    FOREIGN KEY (thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT,
    CHECK (review_count = total_successes + total_failures + legacy_unclassified_count),
    CHECK (legacy_unclassified_count <= legacy_review_count AND legacy_review_count <= review_count),
    CHECK (consecutive_successes = 0 OR consecutive_failures = 0)
) STRICT;

CREATE TABLE settings (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL CHECK (revision > 0),
    timezone TEXT NOT NULL CHECK (length(timezone) > 0),
    day_boundary_hour INTEGER NOT NULL CHECK (day_boundary_hour BETWEEN 0 AND 23),
    scheduler_version INTEGER NOT NULL CHECK (scheduler_version = 1),
    scheduler_json TEXT NOT NULL,
    updated_at_us INTEGER NOT NULL CHECK (updated_at_us > 0)
) STRICT;

CREATE TABLE tag_rule (
    tag TEXT PRIMARY KEY CHECK (length(tag) > 0),
    parameters_json TEXT NOT NULL
) STRICT;

INSERT INTO settings(
    id, revision, timezone, day_boundary_hour, scheduler_version,
    scheduler_json, updated_at_us
) VALUES (
    1, 1, 'UTC', 0, 1,
    '{"agnoia":0.2,"amnesia":0.5,"anagnosis":3,"epignosis":0.1,"interval_fuzz":0.1,"lethe":2,"proto":[0,1,2],"synolon_max":3.0}',
    1
);

CREATE TABLE review_session (
    session_id TEXT PRIMARY KEY CHECK (length(session_id) > 0),
    source_kind TEXT NOT NULL CHECK (source_kind IN (
        'due', 'overdue', 'due_without_overdue', 'tag', 'due_tag', 'topic', 'manual'
    )),
    source_json TEXT NOT NULL,
    target_count INTEGER CHECK (target_count IS NULL OR target_count > 0),
    timezone TEXT NOT NULL,
    day_boundary_hour INTEGER NOT NULL CHECK (day_boundary_hour BETWEEN 0 AND 23),
    scheduler_json TEXT NOT NULL,
    status TEXT NOT NULL CHECK (status IN ('active', 'completed', 'quit', 'aborted')),
    started_at_us INTEGER NOT NULL CHECK (started_at_us > 0),
    ended_at_us INTEGER CHECK (ended_at_us IS NULL OR ended_at_us >= started_at_us),
    current_position INTEGER NOT NULL DEFAULT 0 CHECK (current_position >= 0)
) STRICT;

CREATE UNIQUE INDEX one_active_review_session
ON review_session(status)
WHERE status = 'active';

CREATE TABLE review_session_item (
    session_id TEXT NOT NULL,
    position INTEGER NOT NULL CHECK (position >= 0),
    thema_id TEXT NOT NULL,
    completed_attempt_id TEXT,
    PRIMARY KEY (session_id, position),
    FOREIGN KEY (session_id) REFERENCES review_session(session_id) ON DELETE RESTRICT,
    FOREIGN KEY (thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE review_attempt (
    attempt_id TEXT PRIMARY KEY CHECK (length(attempt_id) > 0),
    session_id TEXT NOT NULL,
    session_position INTEGER NOT NULL CHECK (session_position >= 0),
    thema_id TEXT NOT NULL,
    state_revision INTEGER NOT NULL CHECK (state_revision >= 0),
    content_sha256 TEXT NOT NULL CHECK (length(content_sha256) = 64),
    presentation_json TEXT NOT NULL,
    matcher_version INTEGER NOT NULL CHECK (matcher_version >= 1),
    response_json TEXT,
    detected_success INTEGER CHECK (detected_success IS NULL OR detected_success IN (0, 1)),
    accepted_success INTEGER CHECK (accepted_success IS NULL OR accepted_success IN (0, 1)),
    override_count INTEGER NOT NULL DEFAULT 0 CHECK (override_count >= 0),
    proposed_transition_json TEXT,
    desired_suspended INTEGER NOT NULL CHECK (desired_suspended IN (0, 1)),
    status TEXT NOT NULL CHECK (status IN ('pending', 'graded', 'committed')),
    started_at_us INTEGER NOT NULL CHECK (started_at_us > 0),
    answered_at_us INTEGER,
    committed_at_us INTEGER,
    UNIQUE (session_id, session_position),
    FOREIGN KEY (session_id) REFERENCES review_session(session_id) ON DELETE RESTRICT,
    FOREIGN KEY (thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE review_event (
    event_id TEXT PRIMARY KEY CHECK (length(event_id) > 0),
    attempt_id TEXT UNIQUE,
    thema_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    reviewed_at_us INTEGER NOT NULL CHECK (reviewed_at_us > 0),
    review_day INTEGER NOT NULL,
    matcher_version INTEGER NOT NULL CHECK (matcher_version >= 1),
    response_json TEXT,
    detected_success INTEGER NOT NULL CHECK (detected_success IN (0, 1)),
    accepted_success INTEGER NOT NULL CHECK (accepted_success IN (0, 1)),
    override_count INTEGER NOT NULL CHECK (override_count >= 0),
    scheduler_version INTEGER NOT NULL CHECK (scheduler_version >= 1),
    transition_json TEXT NOT NULL,
    imported_payload_json TEXT,
    FOREIGN KEY (attempt_id) REFERENCES review_attempt(attempt_id) ON DELETE RESTRICT,
    FOREIGN KEY (thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT,
    FOREIGN KEY (session_id) REFERENCES review_session(session_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE review_day (
    review_day INTEGER PRIMARY KEY,
    review_count INTEGER NOT NULL CHECK (review_count >= 0),
    new_count INTEGER NOT NULL CHECK (new_count >= 0),
    success_count INTEGER NOT NULL CHECK (success_count >= 0),
    failure_count INTEGER NOT NULL CHECK (failure_count >= 0),
    override_count INTEGER NOT NULL CHECK (override_count >= 0)
) STRICT;

CREATE TABLE device (
    device_id TEXT PRIMARY KEY CHECK (length(device_id) > 0),
    name TEXT NOT NULL CHECK (length(trim(name)) > 0),
    token_digest TEXT NOT NULL UNIQUE CHECK (length(token_digest) = 64),
    scopes_json TEXT NOT NULL,
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0),
    last_used_at_us INTEGER,
    revoked_at_us INTEGER
) STRICT;

CREATE TABLE audit_event (
    audit_id INTEGER PRIMARY KEY AUTOINCREMENT,
    device_id TEXT,
    action TEXT NOT NULL CHECK (length(action) > 0),
    resource_kind TEXT,
    resource_id TEXT,
    created_at_us INTEGER NOT NULL CHECK (created_at_us > 0),
    FOREIGN KEY (device_id) REFERENCES device(device_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE import_manifest (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    source_schema_version INTEGER NOT NULL,
    source_sha256 TEXT NOT NULL CHECK (length(source_sha256) = 64),
    source_size_bytes INTEGER NOT NULL CHECK (source_size_bytes >= 0),
    source_mtime_ns INTEGER NOT NULL,
    imported_at_us INTEGER NOT NULL CHECK (imported_at_us > 0)
) STRICT;

CREATE TABLE import_table_count (
    table_name TEXT PRIMARY KEY CHECK (length(table_name) > 0),
    source_rows INTEGER NOT NULL CHECK (source_rows >= 0),
    imported_rows INTEGER NOT NULL CHECK (imported_rows >= 0),
    CHECK (source_rows = imported_rows)
) STRICT;

CREATE TABLE import_path_rewrite (
    source_path TEXT PRIMARY KEY,
    logical_path TEXT NOT NULL,
    reason TEXT NOT NULL CHECK (reason IN ('case_collision', 'asset_normalization'))
) STRICT;

INSERT INTO schema_migration(version) VALUES (2);
PRAGMA user_version = 2;

COMMIT;
