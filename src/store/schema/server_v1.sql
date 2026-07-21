BEGIN IMMEDIATE;

CREATE TABLE document(
  document_id TEXT PRIMARY KEY,
  revision INTEGER NOT NULL CHECK(revision > 0),
  title TEXT NOT NULL CHECK(length(trim(title)) > 0),
  file_node_id TEXT,
  file_tags_json TEXT NOT NULL,
  content_sha256 TEXT NOT NULL,
  logical_path TEXT,
  path_key TEXT,
  head_blob_sha256 TEXT,
  active INTEGER NOT NULL DEFAULT 1 CHECK(active IN(0, 1)),
  updated_at_us INTEGER NOT NULL DEFAULT 0 CHECK(updated_at_us >= 0),
  FOREIGN KEY(document_id, file_node_id)
  REFERENCES node(document_id, node_id)
  DEFERRABLE INITIALLY DEFERRED
) STRICT;
CREATE TABLE node_identity(
  node_id TEXT PRIMARY KEY CHECK(length(node_id) > 0)
) STRICT;
CREATE TABLE node(
  document_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
  node_id TEXT NOT NULL,
  title TEXT NOT NULL CHECK(length(trim(title)) > 0),
  title_path_json TEXT NOT NULL,
  parent_id TEXT,
  level INTEGER NOT NULL CHECK(level >= 0),
  active INTEGER NOT NULL DEFAULT 1 CHECK(active IN(0, 1)),
  PRIMARY KEY(document_id, node_id),
  UNIQUE(document_id, ordinal),
  UNIQUE(node_id),
  FOREIGN KEY(document_id) REFERENCES document(document_id) ON DELETE CASCADE,
  FOREIGN KEY(node_id) REFERENCES node_identity(node_id) ON DELETE RESTRICT,
  FOREIGN KEY(document_id, parent_id)
  REFERENCES node(document_id, node_id)
  DEFERRABLE INITIALLY DEFERRED
) STRICT;
CREATE TABLE node_tag(
  document_id TEXT NOT NULL,
  node_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
  tag TEXT NOT NULL,
  PRIMARY KEY(document_id, node_id, ordinal),
  FOREIGN KEY(document_id, node_id)
  REFERENCES node(document_id, node_id)
  ON DELETE CASCADE
) STRICT;
CREATE TABLE link_occurrence(
  document_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
  owner_id TEXT NOT NULL,
  target_id TEXT NOT NULL,
  span_start INTEGER NOT NULL CHECK(span_start >= 0),
  span_end INTEGER NOT NULL CHECK(span_end > span_start),
  PRIMARY KEY(document_id, ordinal),
  FOREIGN KEY(document_id, owner_id)
  REFERENCES node(document_id, node_id)
  ON DELETE CASCADE
) STRICT;
CREATE TABLE node_edge(
  document_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal >= 0),
  source_id TEXT NOT NULL,
  target_id TEXT NOT NULL,
  PRIMARY KEY(document_id, source_id, target_id),
  UNIQUE(document_id, ordinal),
  FOREIGN KEY(document_id, source_id)
  REFERENCES node(document_id, node_id)
  ON DELETE CASCADE
) STRICT;
CREATE TABLE thema(
  thema_id TEXT PRIMARY KEY,
  revision INTEGER NOT NULL DEFAULT 1 CHECK(revision > 0),
  kind TEXT NOT NULL DEFAULT 'basic'
  CHECK(kind IN('basic', 'cloze', 'mcq', 'mc-cloze')),
  keimenon TEXT NOT NULL DEFAULT '',
  hypothesis_json TEXT NOT NULL DEFAULT '[]', answer_json TEXT NOT NULL DEFAULT '[]', source_guid TEXT, archived_at_us INTEGER CHECK(archived_at_us IS NULL OR archived_at_us > 0), parathema TEXT, review_image TEXT, content_sha256 TEXT NOT NULL DEFAULT
'0000000000000000000000000000000000000000000000000000000000000000', created_at_us INTEGER NOT NULL DEFAULT 0 CHECK(created_at_us >= 0), updated_at_us INTEGER NOT NULL DEFAULT 0 CHECK(updated_at_us >= 0)
) STRICT;
CREATE TABLE thema_concept(
  thema_id TEXT NOT NULL,
  concept_id TEXT NOT NULL,
  relation TEXT NOT NULL CHECK(relation IN('primary', 'supporting', 'reference')),
  PRIMARY KEY(thema_id, concept_id),
  FOREIGN KEY(thema_id) REFERENCES thema(thema_id) ON DELETE CASCADE,
  FOREIGN KEY(concept_id)
  REFERENCES node_identity(node_id)
  DEFERRABLE INITIALLY DEFERRED
) STRICT;
CREATE UNIQUE INDEX document_logical_path_unique
ON document(
  logical_path
)
WHERE logical_path IS NOT NULL;
CREATE UNIQUE INDEX document_path_key_unique
ON document(
  path_key
)
WHERE path_key IS NOT NULL;

CREATE TABLE blob(
  content_sha256 TEXT PRIMARY KEY
  CHECK(length(content_sha256) = 64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'),
size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
media_type TEXT NOT NULL CHECK(length(media_type) > 0),
created_at_us INTEGER NOT NULL CHECK(created_at_us > 0)
) STRICT;
CREATE TABLE managed_asset(
  asset_path TEXT PRIMARY KEY CHECK(length(asset_path) > 0),
  path_key TEXT NOT NULL UNIQUE CHECK(length(path_key) > 0),
  blob_sha256 TEXT NOT NULL,
  original_name TEXT NOT NULL CHECK(length(original_name) > 0),
  revision INTEGER NOT NULL CHECK(revision > 0),
  active INTEGER NOT NULL DEFAULT 1 CHECK(active IN(0, 1)),
  created_at_us INTEGER NOT NULL CHECK(created_at_us > 0),
  updated_at_us INTEGER NOT NULL CHECK(updated_at_us > 0),
  FOREIGN KEY(blob_sha256) REFERENCES blob(content_sha256) ON DELETE RESTRICT
) STRICT;
CREATE TABLE asset_revision(
  asset_path TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision > 0),
  blob_sha256 TEXT,
  operation TEXT NOT NULL CHECK(operation IN('create', 'update', 'tombstone')),
  created_at_us INTEGER NOT NULL CHECK(created_at_us > 0),
  author_device_id TEXT,
  PRIMARY KEY(asset_path, revision),
  FOREIGN KEY(blob_sha256) REFERENCES blob(content_sha256) ON DELETE RESTRICT,
  CHECK((operation = 'tombstone') =(blob_sha256 IS NULL))
) STRICT;
CREATE TABLE retired_node(
  node_id TEXT PRIMARY KEY,
  document_id TEXT NOT NULL,
  logical_path TEXT NOT NULL,
  title TEXT NOT NULL,
  title_path_json TEXT NOT NULL,
  tags_json TEXT NOT NULL,
  level INTEGER NOT NULL CHECK(level >= 0),
  retired_at_revision INTEGER NOT NULL CHECK(retired_at_revision > 0),
  FOREIGN KEY(document_id) REFERENCES document(document_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE thema_tag(
  thema_id TEXT NOT NULL,
  tag TEXT NOT NULL CHECK(length(tag) > 0),
  PRIMARY KEY(thema_id, tag),
  FOREIGN KEY(thema_id) REFERENCES thema(thema_id) ON DELETE CASCADE
) STRICT;
CREATE TABLE thema_asset(
  thema_id TEXT NOT NULL,
  asset_path TEXT NOT NULL,
  role TEXT NOT NULL CHECK(role IN('content', 'review_image')),
  PRIMARY KEY(thema_id, asset_path, role),
  FOREIGN KEY(thema_id) REFERENCES thema(thema_id) ON DELETE CASCADE,
  FOREIGN KEY(asset_path) REFERENCES managed_asset(asset_path) ON DELETE RESTRICT
) STRICT;
CREATE TABLE review_state(
  thema_id TEXT PRIMARY KEY,
  state_revision INTEGER NOT NULL CHECK(state_revision >= 0),
  review_count INTEGER NOT NULL CHECK(review_count >= 0),
  last_reviewed_at_us INTEGER CHECK(last_reviewed_at_us IS NULL OR last_reviewed_at_us > 0),
  last_review_day INTEGER,
  next_review_day INTEGER NOT NULL,
  interval_days INTEGER CHECK(interval_days IS NULL OR interval_days >= 1),
  total_successes INTEGER NOT NULL CHECK(total_successes >= 0),
  total_failures INTEGER NOT NULL CHECK(total_failures >= 0),
  suspended INTEGER NOT NULL CHECK(suspended IN(0, 1)),
  stability REAL CHECK(stability IS NULL OR stability > 0),
  difficulty REAL CHECK(difficulty IS NULL OR (difficulty >= 1 AND difficulty <= 10)),
  FOREIGN KEY(thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT,
  CHECK(review_count = total_successes + total_failures),
  CHECK((review_count = 0 AND interval_days IS NULL AND stability IS NULL AND difficulty IS NULL)
    OR (review_count > 0 AND interval_days IS NOT NULL AND stability IS NOT NULL AND difficulty IS NOT NULL))
) STRICT;
CREATE TABLE settings(
  id INTEGER PRIMARY KEY CHECK(id = 1),
  revision INTEGER NOT NULL CHECK(revision > 0),
  timezone TEXT NOT NULL CHECK(length(timezone) > 0),
  day_boundary_hour INTEGER NOT NULL CHECK(day_boundary_hour BETWEEN 0 AND 23),
  scheduler_json TEXT NOT NULL CHECK(json_valid(scheduler_json)),
  updated_at_us INTEGER NOT NULL CHECK(updated_at_us > 0)
) STRICT;
CREATE TABLE review_session(
  session_id TEXT PRIMARY KEY CHECK(length(session_id) > 0),
  source_kind TEXT NOT NULL CHECK(source_kind IN('due', 'overdue', 'due_without_overdue', 'tag', 'due_tag', 'topic', 'manual')),
  source_json TEXT NOT NULL,
  target_count INTEGER CHECK(target_count IS NULL OR target_count > 0),
  timezone TEXT NOT NULL,
  day_boundary_hour INTEGER NOT NULL CHECK(day_boundary_hour BETWEEN 0 AND 23),
  scheduler_json TEXT NOT NULL,
  status TEXT NOT NULL CHECK(status IN('active', 'completed', 'quit', 'aborted')),
  started_at_us INTEGER NOT NULL CHECK(started_at_us > 0),
  ended_at_us INTEGER CHECK(ended_at_us IS NULL OR ended_at_us >= started_at_us),
  current_position INTEGER NOT NULL DEFAULT 0 CHECK(current_position >= 0)
  ,
  owner_device_id TEXT
  REFERENCES device(device_id) ON DELETE RESTRICT,
  review_mode TEXT NOT NULL DEFAULT 'online'
  CHECK(review_mode IN('online', 'offline'))
) STRICT;
CREATE UNIQUE INDEX one_active_review_session
ON review_session(
  status
)
WHERE status = 'active';
CREATE TABLE review_session_item(
  session_id TEXT NOT NULL,
  position INTEGER NOT NULL CHECK(position >= 0),
  thema_id TEXT NOT NULL,
  completed_attempt_id TEXT,
  PRIMARY KEY(session_id, position),
  FOREIGN KEY(session_id) REFERENCES review_session(session_id) ON DELETE RESTRICT,
  FOREIGN KEY(thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE review_attempt(
  attempt_id TEXT PRIMARY KEY CHECK(length(attempt_id) > 0),
  session_id TEXT NOT NULL,
  session_position INTEGER NOT NULL CHECK(session_position >= 0),
  thema_id TEXT NOT NULL,
  state_revision INTEGER NOT NULL CHECK(state_revision >= 0),
  content_sha256 TEXT NOT NULL CHECK(length(content_sha256) = 64),
  presentation_json TEXT NOT NULL,
  matcher_version INTEGER NOT NULL CHECK(matcher_version >= 1),
  response_json TEXT,
  detected_success INTEGER CHECK(detected_success IS NULL OR detected_success IN(0, 1)),
  accepted_success INTEGER CHECK(accepted_success IS NULL OR accepted_success IN(0, 1)),
  override_count INTEGER NOT NULL DEFAULT 0 CHECK(override_count >= 0),
  proposed_transition_json TEXT,
  desired_suspended INTEGER NOT NULL CHECK(desired_suspended IN(0, 1)),
  status TEXT NOT NULL CHECK(status IN('pending', 'graded', 'committed')),
  started_at_us INTEGER NOT NULL CHECK(started_at_us > 0),
  answered_at_us INTEGER,
  committed_at_us INTEGER,
  thema_revision INTEGER NOT NULL DEFAULT 1
  CHECK(thema_revision > 0),
  config_json TEXT NOT NULL CHECK(json_valid(config_json)), mutation_revision INTEGER NOT NULL DEFAULT 0
CHECK(mutation_revision >= 0),
UNIQUE(session_id, session_position),
FOREIGN KEY(session_id) REFERENCES review_session(session_id) ON DELETE RESTRICT,
FOREIGN KEY(thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE review_event(
  event_id TEXT PRIMARY KEY CHECK(length(event_id) > 0),
  attempt_id TEXT UNIQUE,
  thema_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  reviewed_at_us INTEGER NOT NULL CHECK(reviewed_at_us > 0),
  review_day INTEGER NOT NULL,
  matcher_version INTEGER NOT NULL CHECK(matcher_version >= 1),
  response_json TEXT,
  detected_success INTEGER NOT NULL CHECK(detected_success IN(0, 1)),
  accepted_success INTEGER NOT NULL CHECK(accepted_success IN(0, 1)),
  override_count INTEGER NOT NULL CHECK(override_count >= 0),
  config_json TEXT NOT NULL CHECK(json_valid(config_json)),
  transition_json TEXT NOT NULL,
  imported_payload_json TEXT,
  FOREIGN KEY(attempt_id) REFERENCES review_attempt(attempt_id) ON DELETE RESTRICT,
  FOREIGN KEY(thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT,
  FOREIGN KEY(session_id) REFERENCES review_session(session_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE review_day(
  review_day INTEGER PRIMARY KEY,
  review_count INTEGER NOT NULL CHECK(review_count >= 0),
  new_count INTEGER NOT NULL CHECK(new_count >= 0),
  success_count INTEGER NOT NULL CHECK(success_count >= 0),
  failure_count INTEGER NOT NULL CHECK(failure_count >= 0),
  override_count INTEGER NOT NULL CHECK(override_count >= 0)
) STRICT;
CREATE TABLE device(
  device_id TEXT PRIMARY KEY CHECK(length(device_id) > 0),
  name TEXT NOT NULL CHECK(length(trim(name)) > 0),
  token_digest TEXT NOT NULL UNIQUE CHECK(length(token_digest) = 64),
  scopes_json TEXT NOT NULL,
  created_at_us INTEGER NOT NULL CHECK(created_at_us > 0),
  last_used_at_us INTEGER,
  revoked_at_us INTEGER
  ,
  last_device_sequence INTEGER NOT NULL DEFAULT 0
  CHECK(last_device_sequence >= 0)
) STRICT;
CREATE TABLE audit_event(
  audit_id INTEGER PRIMARY KEY AUTOINCREMENT,
  device_id TEXT,
  action TEXT NOT NULL CHECK(length(action) > 0),
  resource_kind TEXT,
  resource_id TEXT,
  created_at_us INTEGER NOT NULL CHECK(created_at_us > 0),
  FOREIGN KEY(device_id) REFERENCES device(device_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE retired_node_edge(
  document_id TEXT NOT NULL,
  source_id TEXT NOT NULL,
  target_id TEXT NOT NULL,
  retired_at_revision INTEGER NOT NULL CHECK(retired_at_revision > 0),
  PRIMARY KEY(document_id, source_id, target_id)
) STRICT;
CREATE TABLE review_pack(
  pack_id TEXT PRIMARY KEY CHECK(length(pack_id) > 0),
  format_version INTEGER NOT NULL CHECK(format_version = 1),
  session_id TEXT NOT NULL UNIQUE,
  device_id TEXT NOT NULL,
  lease_id TEXT NOT NULL UNIQUE CHECK(length(lease_id) > 0),
  status TEXT NOT NULL CHECK(status IN('active', 'completed', 'abandoned')),
  cursor INTEGER NOT NULL CHECK(cursor >= 0),
  issued_at_us INTEGER NOT NULL CHECK(issued_at_us > 0),
  cursor_server_id TEXT,
  cursor_token TEXT,
  FOREIGN KEY(session_id) REFERENCES review_session(session_id) ON DELETE RESTRICT,
  FOREIGN KEY(device_id) REFERENCES device(device_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE review_pack_item(
  pack_id TEXT NOT NULL,
  position INTEGER NOT NULL CHECK(position >= 0),
  thema_id TEXT NOT NULL,
  item_json TEXT NOT NULL CHECK(length(item_json) > 0),
  PRIMARY KEY(pack_id, position),
  FOREIGN KEY(pack_id) REFERENCES review_pack(pack_id) ON DELETE RESTRICT,
  FOREIGN KEY(thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE review_pack_asset(
  pack_id TEXT NOT NULL,
  asset_path TEXT NOT NULL,
  content_sha256 TEXT NOT NULL
  CHECK(length(content_sha256) = 64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'),
size_bytes INTEGER NOT NULL CHECK(size_bytes >= 0),
media_type TEXT NOT NULL CHECK(length(media_type) > 0),
PRIMARY KEY(pack_id, asset_path),
FOREIGN KEY(pack_id) REFERENCES review_pack(pack_id) ON DELETE RESTRICT,
FOREIGN KEY(asset_path) REFERENCES managed_asset(asset_path) ON DELETE RESTRICT,
FOREIGN KEY(content_sha256) REFERENCES blob(content_sha256) ON DELETE RESTRICT
) STRICT;
CREATE TABLE offline_review_operation(
  attempt_id TEXT PRIMARY KEY CHECK(length(attempt_id) > 0),
  pack_id TEXT NOT NULL,
  device_id TEXT NOT NULL,
  device_sequence INTEGER NOT NULL CHECK(device_sequence > 0),
  format_version INTEGER NOT NULL CHECK(format_version = 1),
  payload_sha256 TEXT NOT NULL
  CHECK(length(payload_sha256) = 64 AND payload_sha256 NOT GLOB '*[^0-9a-f]*'),
payload_json TEXT NOT NULL CHECK(length(payload_json) > 0),
received_at_us INTEGER NOT NULL CHECK(received_at_us > 0),
UNIQUE(device_id, device_sequence),
UNIQUE(device_id, payload_sha256),
FOREIGN KEY(pack_id) REFERENCES review_pack(pack_id) ON DELETE RESTRICT,
FOREIGN KEY(device_id) REFERENCES device(device_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE offline_review_result(
  attempt_id TEXT PRIMARY KEY,
  format_version INTEGER NOT NULL CHECK(format_version = 1),
  status TEXT NOT NULL CHECK(status IN('accepted', 'conflict')),
  conflict_reason TEXT CHECK(conflict_reason IS NULL OR conflict_reason IN('lease_ended', 'session_advanced', 'content_changed',
'scheduling_changed', 'capture_mismatch')),
  event_id TEXT,
  authoritative_state_json TEXT NOT NULL,
  session_json TEXT NOT NULL,
  cursor INTEGER NOT NULL CHECK(cursor >= 0),
  reconciled_at_us INTEGER NOT NULL CHECK(reconciled_at_us > 0),
  cursor_server_id TEXT,
  cursor_token TEXT,
  FOREIGN KEY(attempt_id) REFERENCES offline_review_operation(attempt_id) ON DELETE RESTRICT,
  FOREIGN KEY(event_id) REFERENCES review_event(event_id) ON DELETE RESTRICT,
  CHECK((status = 'accepted') =(event_id IS NOT NULL)),
  CHECK((status = 'conflict') =(conflict_reason IS NOT NULL))
) STRICT;
CREATE TABLE server_identity(
  id INTEGER PRIMARY KEY CHECK(id = 1),
  server_id TEXT NOT NULL UNIQUE CHECK(length(server_id) = 32 AND server_id NOT GLOB '*[^0-9a-f]*')
) STRICT;
CREATE TABLE content_change(
  cursor INTEGER PRIMARY KEY AUTOINCREMENT,
  entity_kind TEXT NOT NULL CHECK(entity_kind IN('document', 'asset', 'thema', 'settings', 'review')),
  entity_id TEXT NOT NULL CHECK(length(entity_id) > 0),
  revision INTEGER NOT NULL CHECK(revision >= 0),
  operation TEXT NOT NULL CHECK(length(operation) > 0),
  created_at_us INTEGER NOT NULL CHECK(created_at_us > 0),
  feed_token TEXT NOT NULL UNIQUE DEFAULT(lower(hex(randomblob(16)))) CHECK(length(feed_token) = 32 AND feed_token NOT GLOB '*[^0-9a-f]*')
) STRICT;
CREATE TABLE sync_feed_state(
  id INTEGER PRIMARY KEY CHECK(id = 1),
  retained_after_cursor INTEGER NOT NULL DEFAULT 0
  CHECK(retained_after_cursor >= 0),
  retained_after_token TEXT NOT NULL CHECK(length(retained_after_token) = 32
AND retained_after_token NOT GLOB '*[^0-9a-f]*')
) STRICT;
CREATE TABLE document_revision(
  document_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision > 0),
  logical_path TEXT NOT NULL CHECK(length(logical_path) > 0),
  blob_sha256 TEXT,
  operation TEXT NOT NULL CHECK(operation IN('create', 'update', 'rename', 'tombstone', 'snapshot')),
  created_at_us INTEGER NOT NULL CHECK(created_at_us > 0),
  author_device_id TEXT,
  PRIMARY KEY(document_id, revision),
  FOREIGN KEY(document_id) REFERENCES document(document_id) ON DELETE RESTRICT,
  FOREIGN KEY(blob_sha256) REFERENCES blob(content_sha256) ON DELETE RESTRICT,
  CHECK((operation IN('tombstone', 'snapshot')) =(blob_sha256 IS NULL))
) STRICT;
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
CREATE TABLE replica_snapshot(
  snapshot_id TEXT PRIMARY KEY CHECK(length(snapshot_id) > 0),
  device_id TEXT NOT NULL UNIQUE,
  server_id TEXT NOT NULL,
  cut_cursor INTEGER NOT NULL CHECK(cut_cursor >= 0),
  cut_token TEXT NOT NULL CHECK(length(cut_token) = 32 AND cut_token NOT GLOB '*[^0-9a-f]*'),
created_at_us INTEGER NOT NULL CHECK(created_at_us > 0),
FOREIGN KEY(device_id) REFERENCES device(device_id) ON DELETE RESTRICT,
FOREIGN KEY(server_id) REFERENCES server_identity(server_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE replica_snapshot_item(
  snapshot_id TEXT NOT NULL,
  ordinal INTEGER NOT NULL CHECK(ordinal > 0),
  entity_kind TEXT NOT NULL CHECK(entity_kind IN('document', 'asset', 'thema', 'settings', 'review')),
  entity_id TEXT NOT NULL CHECK(length(entity_id) > 0),
  revision INTEGER NOT NULL CHECK(revision >= 0),
  PRIMARY KEY(snapshot_id, ordinal),
  UNIQUE(snapshot_id, entity_kind, entity_id),
  FOREIGN KEY(snapshot_id) REFERENCES replica_snapshot(snapshot_id) ON DELETE CASCADE
) STRICT;
CREATE TABLE thema_revision(
  thema_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision > 0),
  kind TEXT NOT NULL CHECK(kind IN('basic', 'cloze', 'mcq', 'mc-cloze')),
  keimenon TEXT NOT NULL,
  hypothesis_json TEXT NOT NULL,
  answer_json TEXT NOT NULL,
  source_guid TEXT,
  parathema TEXT,
  review_image TEXT,
  tags_json TEXT NOT NULL,
  concepts_json TEXT NOT NULL,
  asset_paths_json TEXT NOT NULL,
  content_sha256 TEXT NOT NULL CHECK(length(content_sha256) = 64 AND content_sha256 NOT GLOB '*[^0-9a-f]*'),
archived INTEGER NOT NULL CHECK(archived IN(0, 1)),
operation TEXT NOT NULL CHECK(operation IN('create', 'update', 'archive', 'unarchive', 'snapshot')),
created_at_us INTEGER NOT NULL CHECK(created_at_us >= 0),
author_device_id TEXT,
PRIMARY KEY(thema_id, revision),
FOREIGN KEY(thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT,
FOREIGN KEY(author_device_id) REFERENCES device(device_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE document_projection_revision(
  document_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision > 0),
  projection_json TEXT NOT NULL,
  PRIMARY KEY(document_id, revision),
  FOREIGN KEY(document_id, revision)
  REFERENCES document_revision(document_id, revision) ON DELETE RESTRICT
) STRICT;
CREATE TABLE settings_revision(
  revision INTEGER PRIMARY KEY CHECK(revision > 0),
  timezone TEXT NOT NULL,
  day_boundary_hour INTEGER NOT NULL CHECK(day_boundary_hour BETWEEN 0 AND 23),
  scheduler_json TEXT NOT NULL CHECK(json_valid(scheduler_json)),
  operation TEXT NOT NULL CHECK(operation IN('update', 'snapshot')),
  created_at_us INTEGER NOT NULL CHECK(created_at_us >= 0),
  author_device_id TEXT,
  FOREIGN KEY(author_device_id) REFERENCES device(device_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE review_state_revision(
  thema_id TEXT NOT NULL,
  state_revision INTEGER NOT NULL CHECK(state_revision >= 0),
  state_json TEXT NOT NULL,
  event_id TEXT,
  created_at_us INTEGER NOT NULL CHECK(created_at_us >= 0),
  PRIMARY KEY(thema_id, state_revision),
  FOREIGN KEY(thema_id) REFERENCES thema(thema_id) ON DELETE RESTRICT,
  FOREIGN KEY(event_id) REFERENCES review_event(event_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE device_operation(
  operation_id TEXT PRIMARY KEY CHECK(length(operation_id) > 0),
  device_id TEXT NOT NULL,
  device_sequence INTEGER NOT NULL CHECK(device_sequence > 0),
  operation_kind TEXT NOT NULL CHECK(operation_kind IN('review', 'content')),
  format_version INTEGER NOT NULL CHECK(format_version = 1),
  payload_sha256 TEXT NOT NULL CHECK(length(payload_sha256) = 64 AND payload_sha256 NOT GLOB '*[^0-9a-f]*'),
payload_json TEXT NOT NULL CHECK(length(payload_json) > 0),
received_at_us INTEGER NOT NULL CHECK(received_at_us > 0),
UNIQUE(device_id, device_sequence),
FOREIGN KEY(device_id) REFERENCES device(device_id) ON DELETE RESTRICT
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
CREATE TABLE offline_content_operation(
  operation_id TEXT PRIMARY KEY,
  document_id TEXT NOT NULL CHECK(length(document_id) > 0),
  content_sha256 TEXT CHECK(content_sha256 IS NULL OR(length(content_sha256) = 64
AND content_sha256 NOT GLOB '*[^0-9a-f]*')),
mutation TEXT NOT NULL CHECK(mutation IN('put_document', 'rename_document', 'tombstone_document')),
base_kind TEXT NOT NULL CHECK(base_kind IN('absent', 'revision')),
base_revision INTEGER CHECK(base_revision > 0),
base_content_sha256 TEXT CHECK(base_content_sha256 IS NULL OR(length(base_content_sha256) = 64
AND base_content_sha256 NOT GLOB '*[^0-9a-f]*')),
predecessor_operation_id TEXT REFERENCES offline_content_operation(operation_id),
cursor_server_id TEXT NOT NULL CHECK(length(cursor_server_id) = 32
AND cursor_server_id NOT GLOB '*[^0-9a-f]*'),
cursor INTEGER NOT NULL CHECK(cursor >= 0),
cursor_token TEXT NOT NULL CHECK(length(cursor_token) = 32 AND cursor_token NOT GLOB '*[^0-9a-f]*'),
authored_at_us INTEGER NOT NULL CHECK(authored_at_us > 0),
FOREIGN KEY(operation_id) REFERENCES device_operation(operation_id) ON DELETE RESTRICT,
CHECK((mutation = 'put_document') =(content_sha256 IS NOT NULL)),
CHECK((mutation = 'put_document') OR base_kind = 'revision'),
CHECK((base_kind = 'revision') =(base_revision IS NOT NULL)),
CHECK((base_kind = 'revision') =(base_content_sha256 IS NOT NULL)),
CHECK(base_kind = 'revision' OR(base_content_sha256 IS NULL AND predecessor_operation_id IS NULL))
) STRICT;
CREATE TABLE offline_content_result(
  operation_id TEXT PRIMARY KEY,
  format_version INTEGER NOT NULL CHECK(format_version = 1),
  status TEXT NOT NULL CHECK(status IN('accepted', 'conflict')),
  conflict_reason TEXT CHECK(conflict_reason IS NULL OR conflict_reason IN('cursor_changed', 'base_changed', 'invalid_content', 'path_conflict')),
  authoritative_json TEXT,
  cursor INTEGER NOT NULL CHECK(cursor >= 0),
  cursor_server_id TEXT NOT NULL CHECK(length(cursor_server_id) = 32
AND cursor_server_id NOT GLOB '*[^0-9a-f]*'),
cursor_token TEXT NOT NULL CHECK(length(cursor_token) = 32 AND cursor_token NOT GLOB '*[^0-9a-f]*'),
reconciled_at_us INTEGER NOT NULL CHECK(reconciled_at_us > 0),
FOREIGN KEY(operation_id) REFERENCES offline_content_operation(operation_id)
ON DELETE RESTRICT,
CHECK((status = 'accepted') =(conflict_reason IS NULL))
) STRICT;
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
CREATE TABLE document_asset(
  document_id TEXT NOT NULL,
  asset_path TEXT NOT NULL CHECK(length(asset_path) > 0),
  PRIMARY KEY(document_id, asset_path),
  FOREIGN KEY(document_id) REFERENCES document(document_id) ON DELETE RESTRICT
) STRICT;
CREATE INDEX document_asset_path ON document_asset(asset_path);
CREATE TABLE document_revision_asset(
  document_id TEXT NOT NULL,
  revision INTEGER NOT NULL CHECK(revision > 0),
  asset_path TEXT NOT NULL CHECK(length(asset_path) > 0),
  PRIMARY KEY(document_id, revision, asset_path),
  FOREIGN KEY(document_id, revision)
  REFERENCES document_revision(document_id, revision) ON DELETE RESTRICT
) STRICT;
CREATE INDEX document_revision_asset_path
ON document_revision_asset(
  asset_path
);
CREATE TABLE staged_blob(
  content_sha256 TEXT PRIMARY KEY CHECK(length(content_sha256) = 64
AND content_sha256 NOT GLOB '*[^0-9a-f]*'),
size_bytes INTEGER NOT NULL CHECK(size_bytes > 0),
staged_at_us INTEGER NOT NULL CHECK(staged_at_us > 0)
) STRICT;
CREATE TABLE offline_asset_operation(
  operation_id TEXT PRIMARY KEY,
  logical_path TEXT NOT NULL CHECK(length(logical_path) > 0),
  original_name TEXT NOT NULL CHECK(length(original_name) > 0),
  content_sha256 TEXT NOT NULL CHECK(length(content_sha256) = 64
AND content_sha256 NOT GLOB '*[^0-9a-f]*'),
size_bytes INTEGER NOT NULL CHECK(size_bytes > 0),
media_type TEXT NOT NULL CHECK(length(media_type) > 0 AND length(media_type) <= 255),
cursor_server_id TEXT NOT NULL CHECK(length(cursor_server_id) = 32
AND cursor_server_id NOT GLOB '*[^0-9a-f]*'),
cursor INTEGER NOT NULL CHECK(cursor >= 0),
cursor_token TEXT NOT NULL CHECK(length(cursor_token) = 32 AND cursor_token NOT GLOB '*[^0-9a-f]*'),
authored_at_us INTEGER NOT NULL CHECK(authored_at_us > 0),
FOREIGN KEY(operation_id) REFERENCES device_operation(operation_id) ON DELETE RESTRICT
) STRICT;
CREATE TABLE offline_asset_result(
  operation_id TEXT PRIMARY KEY,
  format_version INTEGER NOT NULL CHECK(format_version = 1),
  status TEXT NOT NULL CHECK(status IN('accepted', 'conflict')),
  conflict_reason TEXT CHECK(conflict_reason IS NULL OR conflict_reason IN('cursor_changed', 'base_changed', 'invalid_content', 'path_conflict')),
  authoritative_json TEXT,
  cursor INTEGER NOT NULL CHECK(cursor >= 0),
  cursor_server_id TEXT NOT NULL CHECK(length(cursor_server_id) = 32
AND cursor_server_id NOT GLOB '*[^0-9a-f]*'),
cursor_token TEXT NOT NULL CHECK(length(cursor_token) = 32 AND cursor_token NOT GLOB '*[^0-9a-f]*'),
reconciled_at_us INTEGER NOT NULL CHECK(reconciled_at_us > 0),
FOREIGN KEY(operation_id) REFERENCES offline_asset_operation(operation_id)
ON DELETE RESTRICT,
CHECK((status = 'accepted') =(conflict_reason IS NULL))
) STRICT;
CREATE TABLE offline_asset_evidence(
  operation_id TEXT PRIMARY KEY REFERENCES offline_asset_operation(operation_id)
  ON DELETE RESTRICT,
  content_sha256 TEXT NOT NULL CHECK(length(content_sha256) = 64
AND content_sha256 NOT GLOB '*[^0-9a-f]*'),
size_bytes INTEGER NOT NULL CHECK(size_bytes > 0),
media_type TEXT NOT NULL CHECK(length(media_type) > 0 AND length(media_type) <= 255),
retained_at_us INTEGER NOT NULL CHECK(retained_at_us > 0)
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
CREATE TABLE review_attempt_operation(
  operation_id TEXT NOT NULL CHECK(length(operation_id) > 0 AND length(operation_id) <= 128
AND instr(operation_id, char(0)) = 0
AND substr(operation_id, 1, 1) GLOB '[A-Za-z0-9]'
AND operation_id NOT GLOB '*[^A-Za-z0-9._:-]*'),
format_version INTEGER NOT NULL CHECK(format_version = 1),
actor_device_id TEXT NOT NULL,
attempt_id TEXT NOT NULL,
operation_kind TEXT NOT NULL CHECK(operation_kind IN('accepted_success_set', 'suspension_set')),
expected_revision INTEGER NOT NULL CHECK(expected_revision >= 0),
desired_value INTEGER NOT NULL CHECK(desired_value IN(0, 1)),
payload_sha256 TEXT NOT NULL CHECK(length(payload_sha256) = 64
AND payload_sha256 NOT GLOB '*[^0-9a-f]*'),
payload_json TEXT NOT NULL CHECK(length(payload_json) > 0),
result_kind TEXT NOT NULL CHECK(result_kind IN('accepted', 'revision_conflict', 'stale_attempt', 'state_conflict')),
result_json TEXT NOT NULL CHECK(length(result_json) > 0),
created_at_us INTEGER NOT NULL CHECK(created_at_us > 0),
FOREIGN KEY(actor_device_id) REFERENCES device(device_id) ON DELETE RESTRICT,
FOREIGN KEY(attempt_id) REFERENCES review_attempt(attempt_id) ON DELETE RESTRICT,
UNIQUE(actor_device_id, operation_id)
) STRICT;
CREATE INDEX review_attempt_operation_attempt
ON review_attempt_operation(
  attempt_id,
  created_at_us,
  operation_id
);
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
CREATE TABLE device_mutation_receipt(
  device_id TEXT NOT NULL,
  operation_id TEXT NOT NULL CHECK(length(operation_id) > 0 AND length(operation_id) <= 128
AND instr(operation_id, char(0)) = 0
AND substr(operation_id, 1, 1) GLOB '[A-Za-z0-9]'
AND operation_id NOT GLOB '*[^A-Za-z0-9._:-]*'),
format_version INTEGER NOT NULL CHECK(format_version = 1),
mutation_kind TEXT NOT NULL CHECK(mutation_kind IN('document_create',
'document_replace',
'document_rename',
'document_tombstone',
'asset_create',
'asset_replace',
'asset_tombstone',
'thema_create',
'thema_replace',
'thema_archive_set',
'settings_replace')),
request_sha256 TEXT NOT NULL CHECK(length(request_sha256) = 64
AND request_sha256 NOT GLOB '*[^0-9a-f]*'),
request_json TEXT NOT NULL CHECK(length(request_json) > 0 AND json_valid(request_json)),
result_kind TEXT NOT NULL CHECK(result_kind IN('applied', 'rejected')),
result_json TEXT NOT NULL CHECK(length(result_json) > 0 AND json_valid(result_json)),
created_at_us INTEGER NOT NULL CHECK(created_at_us > 0),
PRIMARY KEY(device_id, operation_id),
FOREIGN KEY(device_id) REFERENCES device(device_id) ON DELETE RESTRICT
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

INSERT INTO settings(
  id, revision, timezone, day_boundary_hour, scheduler_json, updated_at_us
) VALUES (
  1, 1, 'Europe/Athens', 0,
  '{"algorithm":"fsrs","model":"fsrs6","implementation":"fsrs-6.6.1","desired_retention":0.9,"parameters":[0.212,1.2931,2.3065,8.2956,6.4133,0.8334,3.0194,0.001,1.8722,0.1666,0.796,1.4835,0.0614,0.2629,1.6483,0.6014,1.8729,0.5425,0.0912,0.0658,0.1542]}',
  1
);

INSERT INTO server_identity(id, server_id)
VALUES (1, lower(hex(randomblob(16))));

INSERT INTO sync_feed_state(
  id, retained_after_cursor, retained_after_token
) VALUES (1, 0, lower(hex(randomblob(16))));

INSERT INTO settings_revision(
  revision, timezone, day_boundary_hour, scheduler_json,
  operation, created_at_us
)
SELECT
  settings.revision,
  settings.timezone,
  settings.day_boundary_hour,
  settings.scheduler_json,
  'snapshot',
  settings.updated_at_us
FROM settings WHERE id = 1;

PRAGMA application_id = 1196314451;
PRAGMA user_version = 1;

COMMIT;
