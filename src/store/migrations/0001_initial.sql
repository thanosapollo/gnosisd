BEGIN IMMEDIATE;

CREATE TABLE IF NOT EXISTS schema_migration (
    version INTEGER PRIMARY KEY CHECK (version > 0)
) STRICT;

CREATE TABLE document (
    document_id TEXT PRIMARY KEY,
    revision INTEGER NOT NULL CHECK (revision > 0),
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    file_node_id TEXT,
    file_tags_json TEXT NOT NULL,
    compatibility_sha1 TEXT NOT NULL,
    content_sha256 TEXT NOT NULL,
    FOREIGN KEY (document_id, file_node_id)
        REFERENCES node(document_id, node_id)
        DEFERRABLE INITIALLY DEFERRED
) STRICT;

CREATE TABLE node (
    document_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    node_id TEXT NOT NULL,
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    title_path_json TEXT NOT NULL,
    parent_id TEXT,
    level INTEGER NOT NULL CHECK (level >= 0),
    PRIMARY KEY (document_id, node_id),
    UNIQUE (document_id, ordinal),
    UNIQUE (node_id),
    FOREIGN KEY (document_id) REFERENCES document(document_id) ON DELETE CASCADE,
    FOREIGN KEY (document_id, parent_id)
        REFERENCES node(document_id, node_id)
        DEFERRABLE INITIALLY DEFERRED
) STRICT;

CREATE TABLE node_tag (
    document_id TEXT NOT NULL,
    node_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    tag TEXT NOT NULL,
    PRIMARY KEY (document_id, node_id, ordinal),
    FOREIGN KEY (document_id, node_id)
        REFERENCES node(document_id, node_id)
        ON DELETE CASCADE
) STRICT;

CREATE TABLE link_occurrence (
    document_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    owner_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    span_start INTEGER NOT NULL CHECK (span_start >= 0),
    span_end INTEGER NOT NULL CHECK (span_end > span_start),
    PRIMARY KEY (document_id, ordinal),
    FOREIGN KEY (document_id, owner_id)
        REFERENCES node(document_id, node_id)
        ON DELETE CASCADE
) STRICT;

CREATE TABLE node_edge (
    document_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal >= 0),
    source_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    PRIMARY KEY (document_id, source_id, target_id),
    UNIQUE (document_id, ordinal),
    FOREIGN KEY (document_id, source_id)
        REFERENCES node(document_id, node_id)
        ON DELETE CASCADE
) STRICT;

CREATE TABLE thema (
    thema_id TEXT PRIMARY KEY
) STRICT;

CREATE TABLE thema_concept (
    thema_id TEXT NOT NULL,
    concept_id TEXT NOT NULL,
    relation TEXT NOT NULL CHECK (relation IN ('primary', 'supporting', 'reference')),
    PRIMARY KEY (thema_id, concept_id),
    FOREIGN KEY (thema_id) REFERENCES thema(thema_id) ON DELETE CASCADE,
    FOREIGN KEY (concept_id)
        REFERENCES node(node_id)
        DEFERRABLE INITIALLY DEFERRED
) STRICT;

INSERT INTO schema_migration(version) VALUES (1);
PRAGMA user_version = 1;

COMMIT;
