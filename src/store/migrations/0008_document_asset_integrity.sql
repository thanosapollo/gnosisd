BEGIN IMMEDIATE;

CREATE TABLE document_asset (
    document_id TEXT NOT NULL,
    asset_path TEXT NOT NULL CHECK (length(asset_path) > 0),
    PRIMARY KEY (document_id, asset_path),
    FOREIGN KEY (document_id) REFERENCES document(document_id) ON DELETE RESTRICT
) STRICT;

CREATE INDEX document_asset_path ON document_asset(asset_path);

CREATE TABLE document_revision_asset (
    document_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK (revision > 0),
    asset_path TEXT NOT NULL CHECK (length(asset_path) > 0),
    PRIMARY KEY (document_id, revision, asset_path),
    FOREIGN KEY (document_id, revision)
        REFERENCES document_revision(document_id, revision) ON DELETE RESTRICT
) STRICT;

CREATE INDEX document_revision_asset_path
ON document_revision_asset(asset_path);

INSERT INTO schema_migration(version) VALUES (8);
PRAGMA user_version = 8;

COMMIT;
