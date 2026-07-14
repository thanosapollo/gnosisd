BEGIN IMMEDIATE;

CREATE TABLE retired_node_edge (
    document_id TEXT NOT NULL,
    source_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    retired_at_revision INTEGER NOT NULL CHECK (retired_at_revision > 0),
    PRIMARY KEY (document_id, source_id, target_id)
) STRICT;

ALTER TABLE review_attempt ADD COLUMN thema_revision INTEGER NOT NULL DEFAULT 1
    CHECK (thema_revision > 0);
ALTER TABLE review_attempt ADD COLUMN parameters_json TEXT NOT NULL DEFAULT
    '{"agnoia":0.2,"amnesia":0.5,"anagnosis":3,"epignosis":0.1,"interval_fuzz":0.1,"lethe":2,"proto":[0,1,2],"scheduler_version":1,"synolon_max":3.0}';
ALTER TABLE review_attempt ADD COLUMN fuzz_sample REAL;

INSERT INTO schema_migration(version) VALUES (3);
PRAGMA user_version = 3;

COMMIT;
